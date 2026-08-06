use crate::payload::validate_upstream;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use wreq::redirect::Policy;
use wreq::{Client, Proxy};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

const MAX_REDIRECTS: usize = 10;

/// Which browser and platform an upstream request should present as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Profile {
    pub browser: String,
    pub os: String,
}

impl Profile {
    fn cache_key(&self) -> String {
        format!("{}/{}", self.browser, self.os)
    }
}

/// Emulation is a per-client setting in wreq, so supporting more than one
/// profile means holding more than one client. They are built on first use and
/// cached; a `Client` is cheap to clone and pools its connections.
pub struct ClientPool {
    default_profile: Profile,
    /// When set, every client routes its upstream requests through this proxy.
    /// Shared by all profiles so a single provider sees traffic leave from the
    /// proxy's address instead of ours, which is what keeps rate limits at bay.
    proxy: Option<Proxy>,
    clients: RwLock<HashMap<String, Client>>,
}

impl ClientPool {
    pub fn new(browser: &str, os: &str, proxy: Option<Proxy>) -> Result<Self, String> {
        let pool = Self {
            default_profile: Profile {
                browser: browser.to_string(),
                os: os.to_string(),
            },
            proxy,
            clients: RwLock::new(HashMap::new()),
        };
        // Fail at startup rather than on the first request.
        pool.get(None, None)?;
        Ok(pool)
    }

    pub fn default_profile(&self) -> &Profile {
        &self.default_profile
    }

    pub fn get(&self, browser: Option<&str>, os: Option<&str>) -> Result<Client, String> {
        let profile = Profile {
            browser: browser.unwrap_or(&self.default_profile.browser).to_string(),
            os: os.unwrap_or(&self.default_profile.os).to_string(),
        };
        let key = profile.cache_key();

        if let Some(client) = self.clients.read().unwrap().get(&key) {
            return Ok(client.clone());
        }

        let client = build_client(&profile, self.proxy.clone())?;
        self.clients
            .write()
            .unwrap()
            .insert(key.clone(), client.clone());
        tracing::info!(profile = %key, "built emulated client");
        Ok(client)
    }
}

/// wreq-util serializes its profiles as snake_case strings ("chrome_137"),
/// so serde is the parser — no hand-maintained name table to fall behind.
pub fn parse_emulation(name: &str) -> Result<Emulation, String> {
    serde_json::from_value::<Emulation>(serde_json::Value::String(name.to_string()))
        .map_err(|_| format!("unknown emulation profile '{name}'"))
}

pub fn parse_emulation_os(name: &str) -> Result<EmulationOS, String> {
    serde_json::from_value::<EmulationOS>(serde_json::Value::String(name.to_string())).map_err(
        |_| format!("unknown emulation os '{name}' (expected windows, macos, linux, android, ios)"),
    )
}

/// Parse `PROXY_URL` into a proxy applied to every upstream request. The URL
/// carries its own credentials (`http://user:pass@host:port`), which wreq
/// percent-decodes into a `Proxy-Authorization` header for us.
///
/// SOCKS is not compiled in, so only `http`/`https` proxies are accepted; a
/// clearer error here beats a confusing connat failure later.
pub fn parse_proxy(raw: &str) -> Result<Proxy, String> {
    let url = url::Url::parse(raw).map_err(|_| "PROXY_URL is not a valid url".to_string())?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "PROXY_URL scheme '{other}' is not supported (use http or https)"
            ))
        }
    }
    if url.host_str().is_none() {
        return Err("PROXY_URL is missing a host".to_string());
    }
    // Route every scheme (http and https upstreams alike) through the one proxy.
    Proxy::all(raw).map_err(|e| format!("invalid PROXY_URL: {e}"))
}

/// A proxy URL with any credentials stripped, safe to put in logs. Falls back to
/// a fixed placeholder rather than ever risking leaking the raw string.
pub fn redact_proxy(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(url) => {
            let scheme = url.scheme();
            let host = url.host_str().unwrap_or("?");
            match url.port() {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            }
        }
        Err(_) => "<unparseable>".to_string(),
    }
}

/// The SSRF guard runs on the URL in the token, but redirects are followed
/// inside the client where that check cannot see them. Without re-checking each
/// hop, an origin could answer with a 302 to `http://127.0.0.1:6379/` and the
/// guard would be bypassed completely rather than partially.
///
/// A custom policy replaces the built-in hop limit, so it has to enforce its own.
fn guarded_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(format!("exceeded {MAX_REDIRECTS} redirects"));
        }
        match validate_upstream(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(err) => {
                tracing::warn!(url = %attempt.url(), "blocked redirect to disallowed host");
                attempt.error(format!("redirect blocked: {err}"))
            }
        }
    })
}

fn build_client(profile: &Profile, proxy: Option<Proxy>) -> Result<Client, String> {
    let emulation = EmulationOption::builder()
        .emulation(parse_emulation(&profile.browser)?)
        .emulation_os(parse_emulation_os(&profile.os)?)
        .build();

    let mut builder = Client::builder()
        .emulation(emulation)
        .redirect(guarded_redirect_policy())
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90));

    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }

    builder
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_profiles() {
        assert!(parse_emulation("chrome_137").is_ok());
        assert!(parse_emulation("firefox_136").is_ok());
        assert!(parse_emulation_os("windows").is_ok());
        assert!(parse_emulation_os("macos").is_ok());
    }

    #[test]
    fn rejects_unknown_profiles() {
        assert!(parse_emulation("netscape_4").is_err());
        assert!(parse_emulation_os("solaris").is_err());
    }

    #[test]
    fn parses_proxy_with_credentials() {
        assert!(parse_proxy("http://user:pass@185.135.11.34:6033").is_ok());
        assert!(parse_proxy("https://proxy.example.com:8080").is_ok());
    }

    #[test]
    fn rejects_unusable_proxy_urls() {
        assert!(parse_proxy("socks5://user:pass@host:1080").is_err()); // socks not compiled in
        assert!(parse_proxy("ftp://host:21").is_err());
        assert!(parse_proxy("not a url").is_err());
    }

    /// Credentials must never reach the logs.
    #[test]
    fn redaction_strips_credentials() {
        assert_eq!(
            redact_proxy("http://user:secret@185.135.11.34:6033"),
            "http://185.135.11.34:6033"
        );
        assert_eq!(
            redact_proxy("https://proxy.example.com"),
            "https://proxy.example.com"
        );
    }

    /// The initial URL is validated by the proxy layer, but redirects are
    /// followed inside the client where that check cannot reach. Serve a real
    /// redirect pointing at loopback and confirm the client refuses to follow it.
    #[tokio::test]
    async fn refuses_to_follow_redirects_to_blocked_hosts() {
        let app = axum::Router::new()
            .route(
                "/hop",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("http://127.0.0.1:9/secret")
                }),
            )
            .route("/direct", axum::routing::get(|| async { "reached" }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = build_client(
            &Profile {
                browser: "chrome_137".to_string(),
                os: "windows".to_string(),
            },
            None,
        )
        .unwrap();

        let err = client
            .get(format!("http://{addr}/hop"))
            .send()
            .await
            .expect_err("redirect to loopback must not be followed");
        assert!(err.is_redirect(), "expected a redirect error, got {err:?}");

        // The guard applies to redirect targets only; a direct request to the
        // same host is still the proxy layer's call, not the client's.
        let ok = client
            .get(format!("http://{addr}/direct"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
    }

    #[test]
    fn pool_caches_per_browser_and_os() {
        let pool = ClientPool::new("chrome_137", "windows", None).unwrap();
        pool.get(None, None).unwrap();
        pool.get(Some("chrome_137"), Some("windows")).unwrap();
        assert_eq!(pool.clients.read().unwrap().len(), 1);

        // Same browser on a different platform is a distinct fingerprint.
        pool.get(Some("chrome_137"), Some("macos")).unwrap();
        pool.get(Some("firefox_136"), Some("windows")).unwrap();
        assert_eq!(pool.clients.read().unwrap().len(), 3);
    }
}

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use wreq::redirect::Policy;
use wreq::Client;
use wreq_util::{Emulation, EmulationOS, EmulationOption};

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
    clients: RwLock<HashMap<String, Client>>,
}

impl ClientPool {
    pub fn new(browser: &str, os: &str) -> Result<Self, String> {
        let pool = Self {
            default_profile: Profile {
                browser: browser.to_string(),
                os: os.to_string(),
            },
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

        let client = build_client(&profile)?;
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

fn build_client(profile: &Profile) -> Result<Client, String> {
    let emulation = EmulationOption::builder()
        .emulation(parse_emulation(&profile.browser)?)
        .emulation_os(parse_emulation_os(&profile.os)?)
        .build();

    Client::builder()
        .emulation(emulation)
        .redirect(Policy::limited(10))
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
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
    fn pool_caches_per_browser_and_os() {
        let pool = ClientPool::new("chrome_137", "windows").unwrap();
        pool.get(None, None).unwrap();
        pool.get(Some("chrome_137"), Some("windows")).unwrap();
        assert_eq!(pool.clients.read().unwrap().len(), 1);

        // Same browser on a different platform is a distinct fingerprint.
        pool.get(Some("chrome_137"), Some("macos")).unwrap();
        pool.get(Some("firefox_136"), Some("windows")).unwrap();
        assert_eq!(pool.clients.read().unwrap().len(), 3);
    }
}

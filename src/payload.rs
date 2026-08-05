use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

/// Everything needed to fetch one upstream resource. Encoded into every URL the
/// proxy hands out, which is what keeps the proxy stateless: a segment request
/// arriving hours later carries its own headers and emulation profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamPayload {
    pub url: String,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// wreq-util profile name, e.g. "chrome_137". None means use the server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulation: Option<String>,

    /// Platform the profile presents as: windows, macos, linux, android, ios.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

impl StreamPayload {
    /// Same headers and emulation, pointing at a different URL. This is how
    /// context is carried forward when rewriting a playlist.
    pub fn with_url(&self, url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: self.headers.clone(),
            emulation: self.emulation.clone(),
            os: self.os.clone(),
        }
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("StreamPayload is always serializable");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Accepts base64url or standard base64, with or without padding, so
    /// payloads pasted from other tools still work.
    pub fn decode(token: &str) -> Result<Self, PayloadError> {
        let trimmed = token.trim_end_matches('=');
        let bytes = URL_SAFE_NO_PAD
            .decode(trimmed)
            .or_else(|_| STANDARD_NO_PAD.decode(trimmed))
            .map_err(|_| PayloadError::Base64)?;
        let payload: StreamPayload =
            serde_json::from_slice(&bytes).map_err(|e| PayloadError::Json(e.to_string()))?;
        Ok(payload)
    }

    pub fn parsed_url(&self) -> Result<Url, PayloadError> {
        let url = Url::parse(&self.url).map_err(|_| PayloadError::BadUrl)?;
        validate_upstream(&url)?;
        Ok(url)
    }
}

#[derive(Debug)]
pub enum PayloadError {
    Base64,
    Json(String),
    BadUrl,
    BadScheme,
    BlockedHost,
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadError::Base64 => write!(f, "payload is not valid base64"),
            PayloadError::Json(e) => write!(f, "payload is not valid JSON: {e}"),
            PayloadError::BadUrl => write!(f, "payload contains an invalid url"),
            PayloadError::BadScheme => write!(f, "only http and https urls are supported"),
            PayloadError::BlockedHost => {
                write!(
                    f,
                    "upstream host is a private, loopback or otherwise reserved address"
                )
            }
        }
    }
}

/// Best-effort SSRF guard. It only inspects IP literals — a hostname that
/// resolves to a private address still gets through, since we don't control
/// resolution inside the HTTP client.
///
/// Applied to the URL in the token *and* to every redirect hop, because an
/// origin that is allowed to redirect us could otherwise bounce the request
/// onto a private address that this check never sees.
pub fn validate_upstream(url: &Url) -> Result<(), PayloadError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PayloadError::BadScheme);
    }
    let blocked = match url.host() {
        Some(url::Host::Ipv4(ip)) => is_blocked_v4(&ip),
        Some(url::Host::Ipv6(ip)) => is_blocked_v6(&ip),
        // Names are not resolved here, but "localhost" means this machine by
        // definition rather than by resolution, so blocking the literals while
        // allowing the name would be incoherent.
        Some(url::Host::Domain(name)) => {
            name.eq_ignore_ascii_case("localhost")
                || name.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    };
    if blocked {
        return Err(PayloadError::BlockedHost);
    }
    Ok(())
}

fn is_blocked_v4(ip: &std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()          // 127.0.0.0/8
        || ip.is_private()    // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254/16, which is where cloud metadata lives
        || ip.is_unspecified()
        || o[0] == 0                            // 0.0.0.0/8, "this network"
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10, carrier NAT
        || o[0] >= 240 // 240.0.0.0/4 reserved, including 255.255.255.255
}

fn is_blocked_v6(ip: &std::net::Ipv6Addr) -> bool {
    // ::ffff:a.b.c.d and ::a.b.c.d reach the IPv4 address they embed, so they
    // have to be judged as that address rather than as opaque IPv6. This also
    // covers ::1 and ::, which map to 0.0.0.1 and 0.0.0.0.
    if let Some(v4) = ip.to_ipv4() {
        return is_blocked_v4(&v4);
    }
    let s = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(url: &str) -> StreamPayload {
        StreamPayload {
            url: url.to_string(),
            headers: BTreeMap::new(),
            emulation: None,
            os: None,
        }
    }

    fn sample() -> StreamPayload {
        let mut headers = BTreeMap::new();
        headers.insert("Referer".to_string(), "https://origin.test/".to_string());
        headers.insert("Origin".to_string(), "https://origin.test".to_string());
        StreamPayload {
            url: "https://origin.test/live/master.m3u8".to_string(),
            headers,
            emulation: Some("chrome_137".to_string()),
            os: Some("windows".to_string()),
        }
    }

    #[test]
    fn round_trips_through_base64() {
        let payload = sample();
        let decoded = StreamPayload::decode(&payload.encode()).unwrap();
        assert_eq!(payload, decoded);
    }

    #[test]
    fn token_is_url_safe() {
        let token = sample().encode();
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));
    }

    #[test]
    fn accepts_padded_standard_base64() {
        let json = br#"{"url":"https://origin.test/a.m3u8"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        let decoded = StreamPayload::decode(&token).unwrap();
        assert_eq!(decoded.url, "https://origin.test/a.m3u8");
        assert!(decoded.headers.is_empty());
    }

    #[test]
    fn with_url_carries_headers_forward() {
        let next = sample().with_url("https://origin.test/seg/1.ts");
        assert_eq!(next.url, "https://origin.test/seg/1.ts");
        assert_eq!(next.headers.len(), 2);
        assert_eq!(next.emulation.as_deref(), Some("chrome_137"));
        assert_eq!(next.os.as_deref(), Some("windows"));
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(bare("file:///etc/passwd").parsed_url().is_err());
    }

    #[test]
    fn rejects_loopback_and_private_hosts() {
        assert!(bare("http://127.0.0.1/x.m3u8").parsed_url().is_err());
        assert!(bare("http://192.168.1.10/x.m3u8").parsed_url().is_err());
        assert!(bare("https://example.com/x.m3u8").parsed_url().is_ok());
    }

    /// An IPv6 literal can name an IPv4 address, so the v6 checks alone are not
    /// enough: `[::ffff:127.0.0.1]` reaches loopback and `[::ffff:169.254.169.254]`
    /// reaches cloud metadata.
    #[test]
    fn rejects_ipv4_mapped_ipv6_literals() {
        assert!(bare("http://[::ffff:127.0.0.1]/x.m3u8")
            .parsed_url()
            .is_err());
        assert!(bare("http://[::ffff:169.254.169.254]/meta")
            .parsed_url()
            .is_err());
        assert!(bare("http://[::ffff:10.0.0.1]/x.m3u8")
            .parsed_url()
            .is_err());
    }

    #[test]
    fn rejects_reserved_ipv6_ranges() {
        assert!(bare("http://[::1]/x.m3u8").parsed_url().is_err());
        assert!(bare("http://[fd00::1]/x.m3u8").parsed_url().is_err()); // unique local
        assert!(bare("http://[fe80::1]/x.m3u8").parsed_url().is_err()); // link local
        assert!(bare("https://[2606:4700::1111]/x.m3u8")
            .parsed_url()
            .is_ok());
    }

    #[test]
    fn rejects_reserved_ipv4_ranges() {
        assert!(bare("http://169.254.169.254/meta").parsed_url().is_err());
        assert!(bare("http://100.64.0.1/x.m3u8").parsed_url().is_err()); // carrier NAT
        assert!(bare("http://0.0.0.0/x.m3u8").parsed_url().is_err());
        assert!(bare("http://255.255.255.255/x.m3u8").parsed_url().is_err());
        // Decimal-encoded 127.0.0.1, which the URL parser normalises for us.
        assert!(bare("http://2130706433/x.m3u8").parsed_url().is_err());
    }

    #[test]
    fn rejects_localhost_by_name() {
        assert!(bare("http://localhost:8080/x.m3u8").parsed_url().is_err());
        assert!(bare("http://LocalHost/x.m3u8").parsed_url().is_err());
        assert!(bare("http://api.localhost/x.m3u8").parsed_url().is_err());
        assert!(bare("https://notlocalhost.com/x.m3u8").parsed_url().is_ok());
    }
}

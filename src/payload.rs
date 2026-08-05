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
                write!(f, "upstream host is a private or loopback address")
            }
        }
    }
}

/// Best-effort SSRF guard. It only inspects IP literals — a hostname that
/// resolves to a private address still gets through, since we don't control
/// resolution inside the HTTP client.
pub fn validate_upstream(url: &Url) -> Result<(), PayloadError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PayloadError::BadScheme);
    }
    if let Some(url::Host::Ipv4(ip)) = url.host() {
        if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() {
            return Err(PayloadError::BlockedHost);
        }
    }
    if let Some(url::Host::Ipv6(ip)) = url.host() {
        if ip.is_loopback() || ip.is_unspecified() {
            return Err(PayloadError::BlockedHost);
        }
    }
    Ok(())
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
}

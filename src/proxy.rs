use crate::payload::{validate_upstream, PayloadError, StreamPayload};
use crate::rewrite::{
    body_looks_like_playlist, has_playlist_extension, is_playlist_content_type, proxied_url,
    rewrite_playlist,
};
use crate::AppState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use url::Url;

/// Above this size an unlabelled body is assumed to be media rather than a
/// playlist, so we never buffer a whole segment just to sniff it.
const MAX_SNIFF_BYTES: u64 = 4 * 1024 * 1024;

pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<PayloadError> for AppError {
    fn from(err: PayloadError) -> Self {
        AppError::bad_request(err.to_string())
    }
}

pub async fn index() -> Json<serde_json::Value> {
    Json(json!({
        "service": "hls-proxy",
        "usage": {
            "proxy": "/proxy/{base64url(json)}",
            "encode": "/encode?url=...&header=Name:Value&header=Name:Value",
            "encode_post": "POST /encode with body {\"url\":\"...\",\"headers\":{...}}",
        },
        "payload": {
            "url": "required, the upstream http(s) url",
            "headers": "optional map of extra request headers",
            "emulation": "optional browser profile, e.g. chrome_137",
            "os": "optional platform: windows, macos, linux, android, ios"
        }
    }))
}

#[derive(Deserialize)]
pub struct EncodeBody {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub emulation: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
}

/// Builds a ready-to-play proxy URL so callers never have to hand-craft base64.
pub async fn encode_get(
    State(state): State<AppState>,
    Query(params): Query<Vec<(String, String)>>,
    req_headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut url = None;
    let mut emulation = None;
    let mut os = None;
    let mut headers = BTreeMap::new();

    for (key, value) in params {
        match key.to_ascii_lowercase().as_str() {
            "url" => url = Some(value),
            "emulation" => emulation = Some(value),
            "os" => os = Some(value),
            "header" | "h" => {
                if let Some((name, header_value)) = value.split_once(':') {
                    headers.insert(name.trim().to_string(), header_value.trim().to_string());
                } else {
                    return Err(AppError::bad_request(format!(
                        "header '{value}' must be in 'Name: Value' form"
                    )));
                }
            }
            _ => {}
        }
    }

    let url = url.ok_or_else(|| AppError::bad_request("missing required 'url' query parameter"))?;
    build_encode_response(
        &state,
        &req_headers,
        EncodeBody {
            url,
            headers,
            emulation,
            os,
        },
    )
}

pub async fn encode_post(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Json(body): Json<EncodeBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    build_encode_response(&state, &req_headers, body)
}

fn build_encode_response(
    state: &AppState,
    req_headers: &HeaderMap,
    body: EncodeBody,
) -> Result<Json<serde_json::Value>, AppError> {
    let parsed = Url::parse(&body.url).map_err(|_| AppError::bad_request("invalid url"))?;
    validate_upstream(&parsed)?;

    if let Some(profile) = &body.emulation {
        crate::client::parse_emulation(profile).map_err(AppError::bad_request)?;
    }
    if let Some(os) = &body.os {
        crate::client::parse_emulation_os(os).map_err(AppError::bad_request)?;
    }

    let payload = StreamPayload {
        url: parsed.to_string(),
        headers: body.headers,
        emulation: body.emulation,
        os: body.os,
    };

    let base = state.resolve_base(req_headers);
    Ok(Json(json!({
        "url": proxied_url(&base, &payload),
        "payload": payload.encode(),
    })))
}

pub async fn proxy(
    State(state): State<AppState>,
    Path(token): Path<String>,
    req_headers: HeaderMap,
) -> Result<Response, AppError> {
    let payload = StreamPayload::decode(&token)?;
    let target = payload.parsed_url()?;

    let client = state
        .clients
        .get(payload.emulation.as_deref(), payload.os.as_deref())
        .map_err(AppError::bad_request)?;

    let mut request = client.get(target.as_str());
    for (name, value) in &payload.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        request = request.header(name.as_str(), value.as_str());
    }

    // Range must be forwarded or seeking in VOD streams breaks.
    if let Some(range) = req_headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        request = request.header("Range", range);
    }

    let upstream = request.send().await.map_err(|e| {
        tracing::warn!(url = %target, error = %e, "upstream request failed");
        AppError::new(
            StatusCode::BAD_GATEWAY,
            format!("upstream request failed: {e}"),
        )
    })?;

    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    // Redirects are followed by the client, so relative URLs inside a playlist
    // must resolve against wherever we actually landed.
    let final_url = Url::parse(upstream.url().as_str()).unwrap_or(target);

    let upstream_headers = copy_headers(upstream.headers());
    let content_type = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_length = upstream_headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let kind = classify(content_type.as_deref(), &final_url, content_length);
    let proxy_base = state.resolve_base(&req_headers);

    match kind {
        Kind::Binary => {
            let mut response = Response::builder().status(status);
            let headers = response.headers_mut().unwrap();
            forward_response_headers(&upstream_headers, headers, false);
            Ok(response
                .body(Body::from_stream(upstream.bytes_stream()))
                .expect("response is well formed"))
        }
        Kind::Playlist | Kind::Sniff => {
            let bytes = upstream.bytes().await.map_err(|e| {
                AppError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read upstream body: {e}"),
                )
            })?;

            let is_playlist = matches!(kind, Kind::Playlist) || body_looks_like_playlist(&bytes);

            if !is_playlist {
                let mut response = Response::builder().status(status);
                let headers = response.headers_mut().unwrap();
                forward_response_headers(&upstream_headers, headers, false);
                return Ok(response
                    .body(Body::from(bytes.to_vec()))
                    .expect("response is well formed"));
            }

            let text = String::from_utf8_lossy(&bytes);
            let rewritten = rewrite_playlist(&text, &final_url, &payload, &proxy_base);

            let mut response = Response::builder().status(status);
            let headers = response.headers_mut().unwrap();
            forward_response_headers(&upstream_headers, headers, true);
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.apple.mpegurl"),
            );
            // Live playlists are rewritten on every refresh, so they must not be cached.
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-store, must-revalidate"),
            );

            Ok(response
                .body(Body::from(rewritten))
                .expect("response is well formed"))
        }
    }
}

enum Kind {
    Playlist,
    Binary,
    /// Neither headers nor extension were conclusive; decide from the body.
    Sniff,
}

fn classify(content_type: Option<&str>, url: &Url, content_length: Option<u64>) -> Kind {
    if let Some(ct) = content_type {
        if is_playlist_content_type(ct) {
            return Kind::Playlist;
        }
    }

    // Checked before the binary content types: plenty of CDNs serve playlists
    // as application/octet-stream.
    if has_playlist_extension(url) {
        return Kind::Playlist;
    }

    if let Some(ct) = content_type {
        let main = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if main.starts_with("video/")
            || main.starts_with("audio/")
            || main.starts_with("image/")
            || main == "application/octet-stream"
            || main == "binary/octet-stream"
            || main == "application/mp4"
        {
            return Kind::Binary;
        }
    }

    if has_binary_extension(url) {
        return Kind::Binary;
    }

    match content_length {
        Some(len) if len > MAX_SNIFF_BYTES => Kind::Binary,
        _ => Kind::Sniff,
    }
}

fn has_binary_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    [
        ".ts", ".m4s", ".mp4", ".m4a", ".m4v", ".aac", ".mp3", ".vtt", ".key", ".cmfv", ".cmfa",
        ".fmp4", ".webm", ".jpg", ".png",
    ]
    .iter()
    .any(|ext| path.ends_with(ext))
}

fn copy_headers(headers: &wreq::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(name, value);
        }
    }
    out
}

fn forward_response_headers(from: &HeaderMap, to: &mut HeaderMap, is_playlist: bool) {
    const PASS_THROUGH: [HeaderName; 6] = [
        header::CONTENT_TYPE,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CACHE_CONTROL,
    ];

    for name in PASS_THROUGH {
        if is_playlist && (name == header::CONTENT_TYPE || name == header::CACHE_CONTROL) {
            continue;
        }
        if let Some(value) = from.get(&name) {
            to.insert(name, value.clone());
        }
    }

    // Content-Length is only meaningful when the body was not decompressed on
    // the way through, and it never survives a playlist rewrite.
    if !is_playlist && from.get(header::CONTENT_ENCODING).is_none() {
        if let Some(value) = from.get(header::CONTENT_LENGTH) {
            to.insert(header::CONTENT_LENGTH, value.clone());
        }
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    const HOP_BY_HOP: [&str; 9] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
    ];
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn playlist_content_type_wins() {
        assert!(matches!(
            classify(
                Some("application/vnd.apple.mpegurl"),
                &url("https://a.test/x"),
                None
            ),
            Kind::Playlist
        ));
    }

    #[test]
    fn m3u8_extension_beats_octet_stream() {
        assert!(matches!(
            classify(
                Some("application/octet-stream"),
                &url("https://a.test/x.m3u8?t=1"),
                None
            ),
            Kind::Playlist
        ));
    }

    #[test]
    fn media_types_and_extensions_stream() {
        assert!(matches!(
            classify(Some("video/mp2t"), &url("https://a.test/1.ts"), None),
            Kind::Binary
        ));
        assert!(matches!(
            classify(None, &url("https://a.test/1.m4s"), None),
            Kind::Binary
        ));
    }

    #[test]
    fn large_unlabelled_bodies_are_not_sniffed() {
        assert!(matches!(
            classify(None, &url("https://a.test/opaque"), Some(50_000_000)),
            Kind::Binary
        ));
        assert!(matches!(
            classify(None, &url("https://a.test/opaque"), Some(1_024)),
            Kind::Sniff
        ));
    }

    #[test]
    fn hop_by_hop_headers_are_filtered() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("HOST"));
        assert!(!is_hop_by_hop("Referer"));
    }
}

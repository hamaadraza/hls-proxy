use crate::payload::{validate_upstream, PayloadError, StreamPayload};
use crate::rewrite::{
    body_looks_like_playlist, has_playlist_extension, is_playlist_content_type, proxied_url,
    rewrite_playlist,
};
use crate::router::{retry_after_from_secs, Decision};
use crate::{AppState, ProxyMode};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use url::Url;

/// Above this size an unlabelled body is assumed to be media rather than a
/// playlist, so we never buffer a whole segment just to sniff it.
const MAX_SNIFF_BYTES: u64 = 4 * 1024 * 1024;

/// Hard ceiling on how much of a body we will hold in memory to rewrite it.
///
/// `MAX_SNIFF_BYTES` is only consulted when the upstream declared a
/// `Content-Length`, and that is both frequently absent (any chunked response)
/// and attacker-controlled. This limit is enforced while reading instead, so it
/// holds no matter what the upstream claimed.
const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// A body read up to a limit, without committing to buffering all of it.
enum Capped {
    /// Fit inside the cap, so it is fully in memory and can be rewritten.
    Complete(Bytes),
    /// Passed the cap. The prefix already read is rejoined to the rest of the
    /// stream, so the response can still be forwarded in full without ever
    /// being held in memory at once.
    Exceeded(Body),
}

async fn read_capped(upstream: wreq::Response, cap: usize) -> Result<Capped, wreq::Error> {
    let mut stream = upstream.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        if buf.len() > cap {
            let prefix = Bytes::from(buf);
            let rejoined =
                futures_util::stream::once(async move { Ok::<Bytes, wreq::Error>(prefix) })
                    .chain(stream);
            return Ok(Capped::Exceeded(Body::from_stream(rejoined)));
        }
    }

    Ok(Capped::Complete(Bytes::from(buf)))
}

#[derive(Debug)]
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

    let upstream = fetch_upstream(&state, &payload, &target, &req_headers).await?;

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

    // Now that conditional headers are forwarded, upstream can answer 304. It
    // carries no body, so there is nothing to classify, sniff or rewrite — and
    // sending one anyway would violate the spec.
    if status == StatusCode::NOT_MODIFIED {
        let mut response = Response::builder().status(status);
        let headers = response.headers_mut().unwrap();
        forward_response_headers(&upstream_headers, headers, false);
        return Ok(response
            .body(Body::empty())
            .expect("response is well formed"));
    }

    let kind = classify(content_type.as_deref(), &final_url, content_length);
    let declared_playlist = matches!(kind, Kind::Playlist);
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
            let capped = read_capped(upstream, MAX_BUFFER_BYTES).await.map_err(|e| {
                AppError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read upstream body: {e}"),
                )
            })?;

            let bytes = match capped {
                Capped::Complete(bytes) => bytes,
                Capped::Exceeded(body) => {
                    // A playlist this large cannot be rewritten, and forwarding
                    // it unrewritten would fail playback with no visible cause,
                    // so say so plainly instead.
                    if declared_playlist {
                        tracing::warn!(url = %final_url, "playlist exceeds the rewrite limit");
                        return Err(AppError::new(
                            StatusCode::BAD_GATEWAY,
                            format!(
                                "playlist is larger than the {MAX_BUFFER_BYTES} byte rewrite limit"
                            ),
                        ));
                    }
                    // Unlabelled and far too large to be a playlist, so it was
                    // media after all. Stream it through.
                    let mut response = Response::builder().status(status);
                    let headers = response.headers_mut().unwrap();
                    forward_response_headers(&upstream_headers, headers, false);
                    return Ok(response.body(body).expect("response is well formed"));
                }
            };

            let is_playlist = declared_playlist || body_looks_like_playlist(&bytes);

            if !is_playlist {
                let mut response = Response::builder().status(status);
                let headers = response.headers_mut().unwrap();
                forward_response_headers(&upstream_headers, headers, false);
                return Ok(response
                    .body(Body::from(bytes))
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

/// Fetch the upstream, applying the configured proxy policy.
///
/// `Off` always goes direct, `Always` always goes through the proxy. `Fallback`
/// asks the router per host: direct until a host rate-limits us, then proxied.
/// A direct 429 in fallback mode transparently retries once through the proxy,
/// so the caller never sees the rate limit.
async fn fetch_upstream(
    state: &AppState,
    payload: &StreamPayload,
    target: &Url,
    req_headers: &HeaderMap,
) -> Result<wreq::Response, AppError> {
    let host = target.host_str().unwrap_or_default().to_string();
    let now = Instant::now();

    let decision = match state.proxy_mode {
        ProxyMode::Off => Decision::Direct { probe: false },
        ProxyMode::Always => Decision::Proxied,
        ProxyMode::Fallback => state.router.route(&host, now),
    };

    let use_proxy = matches!(decision, Decision::Proxied);
    let upstream = send_once(state, payload, target, req_headers, use_proxy).await?;

    // Only fallback mode learns from the response and may retry.
    if state.proxy_mode != ProxyMode::Fallback {
        return Ok(upstream);
    }

    let rate_limited = upstream.status().as_u16() == 429;
    let retry_after = upstream
        .headers()
        .get(wreq::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(retry_after_from_secs)
        .unwrap_or(Duration::ZERO);

    match decision {
        // The direct path got rate-limited. Trip the breaker so later requests
        // to this host skip straight to the proxy, and retry this one through it
        // now so the viewer still gets their segment.
        Decision::Direct { .. } if rate_limited => {
            state
                .router
                .on_result(&host, decision, true, retry_after, now);
            tracing::info!(%host, "direct upstream rate-limited; retrying through proxy");
            match send_once(state, payload, target, req_headers, true).await {
                Ok(retried) => Ok(retried),
                // The proxy retry itself failed; the original 429 is the best
                // answer we still have in hand.
                Err(_) => {
                    tracing::warn!(%host, "proxy retry failed after direct 429");
                    Ok(upstream)
                }
            }
        }
        _ => {
            state
                .router
                .on_result(&host, decision, rate_limited, retry_after, now);
            Ok(upstream)
        }
    }
}

/// Send one upstream request over the direct or proxied client.
async fn send_once(
    state: &AppState,
    payload: &StreamPayload,
    target: &Url,
    req_headers: &HeaderMap,
    use_proxy: bool,
) -> Result<wreq::Response, AppError> {
    let client = state
        .clients
        .get(
            payload.emulation.as_deref(),
            payload.os.as_deref(),
            use_proxy,
        )
        .map_err(AppError::bad_request)?;

    build_upstream_request(&client, target, payload, req_headers)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %target, error = %e, "upstream request failed");
            AppError::new(
                StatusCode::BAD_GATEWAY,
                format!("upstream request failed: {e}"),
            )
        })
}

/// Build the upstream request: payload headers plus the client's conditional and
/// range headers. Kept separate so a fallback retry reconstructs an identical
/// request against a different client.
fn build_upstream_request(
    client: &wreq::Client,
    target: &Url,
    payload: &StreamPayload,
    req_headers: &HeaderMap,
) -> wreq::RequestBuilder {
    let mut request = client.get(target.as_str());
    for (name, value) in &payload.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        request = request.header(name.as_str(), value.as_str());
    }

    // Range must be forwarded or seeking in VOD streams breaks. The validators
    // go with it: we hand clients the upstream `ETag`/`Last-Modified` on segment
    // responses, so dropping the matching conditional headers on the way back up
    // would mean revalidation could never produce a 304.
    for name in [
        header::RANGE,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_RANGE,
    ] {
        if let Some(value) = req_headers.get(&name).and_then(|v| v.to_str().ok()) {
            request = request.header(name.as_str(), value);
        }
    }

    request
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

/// Responses carry whatever the upstream said they were, and this server has no
/// authentication, so anyone could otherwise use it to serve `text/html` from
/// your own origin — a stored-XSS primitive against the proxy's domain and
/// anything sharing it.
///
/// `sandbox` drops a document response into an opaque origin, so script in it
/// can no longer reach this origin, and `nosniff` stops a mislabelled body from
/// being sniffed into one. Neither affects media: the CSP `sandbox` directive
/// applies to documents, not to segments fetched by a player.
fn set_isolation_headers(to: &mut HeaderMap) {
    to.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    to.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
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
        // A rewritten playlist is not the entity upstream described, so its
        // content type and caching directives are replaced by the caller — and
        // its validators have to be dropped outright rather than replaced,
        // since an `ETag` for the original bytes would be a lie about ours.
        if is_playlist
            && (name == header::CONTENT_TYPE
                || name == header::CACHE_CONTROL
                || name == header::ETAG
                || name == header::LAST_MODIFIED)
        {
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

    // Applied here rather than at each call site so no proxied response can be
    // built without it.
    set_isolation_headers(to);
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

    fn upstream_headers() -> HeaderMap {
        let mut from = HeaderMap::new();
        from.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
        from.insert(header::ETAG, HeaderValue::from_static("\"abc123\""));
        from.insert(
            header::LAST_MODIFIED,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        from.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=3600"),
        );
        from.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        from
    }

    /// The rewrite changes the body, so upstream validators no longer describe
    /// what we serve. Keeping them would hand a caching layer in front of the
    /// proxy an `ETag` for bytes it never sees.
    #[test]
    fn playlist_rewrite_drops_stale_validators() {
        let mut to = HeaderMap::new();
        forward_response_headers(&upstream_headers(), &mut to, true);

        assert!(to.get(header::ETAG).is_none());
        assert!(to.get(header::LAST_MODIFIED).is_none());
        assert!(to.get(header::CONTENT_TYPE).is_none()); // caller sets it
        assert!(to.get(header::CACHE_CONTROL).is_none()); // caller sets it
        assert_eq!(to.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    }

    /// Segments are forwarded byte-for-byte, so their validators are still true
    /// and are what makes revalidation possible.
    #[test]
    fn binary_responses_keep_validators() {
        let mut to = HeaderMap::new();
        forward_response_headers(&upstream_headers(), &mut to, false);

        assert_eq!(to.get(header::ETAG).unwrap(), "\"abc123\"");
        assert_eq!(
            to.get(header::LAST_MODIFIED).unwrap(),
            "Wed, 21 Oct 2015 07:28:00 GMT"
        );
        assert_eq!(to.get(header::CONTENT_TYPE).unwrap(), "video/mp2t");
        assert_eq!(to.get(header::CACHE_CONTROL).unwrap(), "max-age=3600");
    }

    #[test]
    fn every_proxied_response_is_isolated() {
        for is_playlist in [true, false] {
            let mut to = HeaderMap::new();
            forward_response_headers(&upstream_headers(), &mut to, is_playlist);
            assert_eq!(to.get(header::CONTENT_SECURITY_POLICY).unwrap(), "sandbox");
            assert_eq!(to.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        }
    }

    /// An upstream that mislabels itself as HTML must not become script running
    /// on this proxy's origin.
    #[test]
    fn html_from_upstream_is_sandboxed() {
        let mut from = HeaderMap::new();
        from.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        let mut to = HeaderMap::new();
        forward_response_headers(&from, &mut to, false);

        assert_eq!(to.get(header::CONTENT_TYPE).unwrap(), "text/html");
        assert_eq!(to.get(header::CONTENT_SECURITY_POLICY).unwrap(), "sandbox");
    }

    #[test]
    fn hop_by_hop_headers_are_filtered() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("HOST"));
        assert!(!is_hop_by_hop("Referer"));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Spin up a local upstream whose first `first_429` responses are 429 and
    /// the rest are 200, counting every hit. Returns its address and the
    /// counter. Loopback is fine here: the SSRF guard lives in the `proxy`
    /// handler, and `fetch_upstream` is handed an already-validated target.
    async fn flaky_upstream(first_429: usize) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let app = axum::Router::new().route(
            "/seg",
            axum::routing::get(move || {
                let seen = seen.clone();
                async move {
                    let n = seen.fetch_add(1, Ordering::SeqCst);
                    if n < first_429 {
                        (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response()
                    } else {
                        (StatusCode::OK, "segment").into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, hits)
    }

    fn fallback_state() -> AppState {
        AppState {
            clients: Arc::new(
                crate::client::ClientPool::new("chrome_137", "windows", None).unwrap(),
            ),
            base_url: None,
            proxy_mode: ProxyMode::Fallback,
            router: Arc::new(crate::router::HostRouter::new()),
        }
    }

    fn payload_for(addr: std::net::SocketAddr) -> (StreamPayload, Url) {
        let url = format!("http://{addr}/seg");
        let target = Url::parse(&url).unwrap();
        let payload = StreamPayload {
            url,
            headers: BTreeMap::new(),
            emulation: None,
            os: None,
        };
        (payload, target)
    }

    /// A direct 429 in fallback mode is retried through the proxy path and the
    /// caller sees the successful response, not the 429. With no proxy actually
    /// configured the retry reuses the direct client — what we assert is the
    /// transparent second attempt, i.e. two upstream hits ending in 200.
    #[tokio::test]
    async fn fallback_retries_after_a_direct_429() {
        let (addr, hits) = flaky_upstream(1).await;
        let state = fallback_state();
        let (payload, target) = payload_for(addr);

        let resp = fetch_upstream(&state, &payload, &target, &HeaderMap::new())
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "expected a direct 429 then a retry"
        );

        // The host is now tripped, so the next request skips straight to proxied.
        assert_eq!(
            state.router.route(&addr.ip().to_string(), Instant::now()),
            Decision::Proxied
        );
    }

    /// A healthy direct response is returned untouched, with no wasted retry.
    #[tokio::test]
    async fn fallback_leaves_healthy_responses_alone() {
        let (addr, hits) = flaky_upstream(0).await;
        let state = fallback_state();
        let (payload, target) = payload_for(addr);

        let resp = fetch_upstream(&state, &payload, &target, &HeaderMap::new())
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a healthy response must not retry"
        );
    }
}

mod client;
mod payload;
mod proxy;
mod rewrite;

use axum::http::{header, HeaderMap, Method};
use axum::routing::get;
use axum::Router;
use client::ClientPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

const DEFAULT_EMULATION: &str = "chrome_137";
const DEFAULT_EMULATION_OS: &str = "windows";

#[derive(Clone)]
pub struct AppState {
    pub clients: Arc<ClientPool>,
    /// Public origin the rewritten URLs point at. None means derive it from the
    /// request, which keeps local runs working with no configuration.
    pub base_url: Option<String>,
}

impl AppState {
    pub fn resolve_base(&self, headers: &HeaderMap) -> String {
        if let Some(base) = &self.base_url {
            return base.clone();
        }

        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");

        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim())
            .filter(|v| *v == "http" || *v == "https")
            .unwrap_or("http");

        format!("{scheme}://{host}")
    }
}

#[tokio::main]
async fn main() {
    // A .env file is convenient locally; real deployments inject env directly.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hls_proxy=info,tower_http=warn".into()),
        )
        .init();

    let base_url = std::env::var("BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty());

    let default_emulation =
        std::env::var("DEFAULT_EMULATION").unwrap_or_else(|_| DEFAULT_EMULATION.to_string());
    let default_emulation_os =
        std::env::var("DEFAULT_EMULATION_OS").unwrap_or_else(|_| DEFAULT_EMULATION_OS.to_string());

    let clients = match ClientPool::new(&default_emulation, &default_emulation_os) {
        Ok(pool) => Arc::new(pool),
        Err(err) => {
            eprintln!("failed to initialise http client: {err}");
            std::process::exit(1);
        }
    };

    let state = AppState { clients, base_url };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::HEAD, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .expose_headers(Any);

    let app = Router::new()
        .route("/", get(proxy::index))
        .route("/encode", get(proxy::encode_get).post(proxy::encode_post))
        .route("/proxy/{token}", get(proxy::proxy))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .unwrap_or_else(|_| panic!("invalid bind address {bind}:{port}"));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    tracing::info!(
        %addr,
        base_url = state.base_url.as_deref().unwrap_or("<from request host>"),
        emulation = state.clients.default_profile().browser,
        emulation_os = state.clients.default_profile().os,
        "hls-proxy listening"
    );

    axum::serve(listener, app).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn state(base: Option<&str>) -> AppState {
        AppState {
            clients: Arc::new(ClientPool::new(DEFAULT_EMULATION, DEFAULT_EMULATION_OS).unwrap()),
            base_url: base.map(str::to_string),
        }
    }

    #[test]
    fn configured_base_url_wins() {
        let state = state(Some("https://hls-proxy.xyz"));
        assert_eq!(
            state.resolve_base(&headers(&[("host", "internal:8080")])),
            "https://hls-proxy.xyz"
        );
    }

    #[test]
    fn falls_back_to_request_host() {
        let state = state(None);
        assert_eq!(
            state.resolve_base(&headers(&[("host", "localhost:8080")])),
            "http://localhost:8080"
        );
    }

    #[test]
    fn honours_forwarded_proto() {
        let state = state(None);
        assert_eq!(
            state.resolve_base(&headers(&[
                ("host", "hls-proxy.xyz"),
                ("x-forwarded-proto", "https"),
            ])),
            "https://hls-proxy.xyz"
        );
    }
}

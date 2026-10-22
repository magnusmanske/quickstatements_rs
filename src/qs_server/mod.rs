//! The web service: the Vue frontend and `api.php`, a port of the PHP
//! QuickStatements web service. Users log in with OAuth 1.0a.
//!
//! Configured by the frontend config (`config_file`, or `config` inline in
//! `config_rs.json`) and these keys of `config_rs.json`:
//! - `cookie_secret`: at least 32 characters, to encrypt session cookies;
//!   without it, users are logged out on every restart
//! - `tmp_dir`: where temporary batches are kept

mod api;
mod assets;
mod batches;
mod import;
mod oauth;
mod session;
mod single_command;

use crate::error::{QsError, QsResult};
use crate::qs_config::QuickStatements;
use axum::extract::{DefaultBodyLimit, FromRef, Request, State};
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::{Json, Response};
use axum::routing::{any, get};
use axum::Router;
use axum_extra::extract::cookie::Key;
use oauth::OAuth;
use rand::distr::{Alphanumeric, SampleString};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tower_http::catch_panic::CatchPanicLayer;
use wikimisc::mediawiki::reqwest::Url;

/// Imports and batches can be large; PHP allowed 64 MB too
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// The site config the frontend gets; `oauth` and the like stay private
const PUBLIC_SITE_KEYS: &[&str] = &[
    "label",
    "server",
    "api",
    "publicApi",
    "pageBase",
    "entityBase",
    "toolBase",
    "types",
];

#[derive(Clone)]
pub struct AppState {
    qs: Arc<QuickStatements>,
    /// None if no OAuth consumer is configured: then no one can log in
    oauth: Option<Arc<OAuth>>,
    cookie_key: Key,
    /// Only send cookies over HTTPS, if the tool is served over HTTPS
    secure_cookies: bool,
    /// Origins that may make changes for the logged-in user (CSRF protection)
    origins: Vec<String>,
    tmp_dir: PathBuf,
    default_site: String,
    default_api: String,
    /// The tool's URL, with a trailing slash
    tool_base: String,
    /// The default site's entity URI prefix
    entity_base: String,
    public_config: Arc<Value>,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}

impl AppState {
    pub fn new(qs: Arc<QuickStatements>) -> QsResult<Self> {
        let config = qs.frontend_config();
        let default_site = config["site"]
            .as_str()
            .ok_or_else(|| QsError::ConfigError("No `site` in the frontend config".into()))?
            .to_string();
        let site = &config["sites"][&default_site];
        let default_api = site["api"]
            .as_str()
            .ok_or_else(|| QsError::ConfigError(format!("No API for site '{default_site}'")))?
            .to_string();
        let tool_base = match site["toolBase"].as_str().map(|b| b.trim_end_matches('/')) {
            Some(base) if !base.is_empty() => format!("{base}/"),
            _ => "./".to_string(),
        };
        let oauth = OAuth::from_site(site, &tool_base)?.map(Arc::new);
        if oauth.is_none() {
            log::warn!("No OAuth consumer configured for site '{default_site}': login is disabled");
        }
        let cookie_key = match qs.cookie_secret() {
            Some(secret) if secret.len() >= 32 => Key::derive_from(secret.as_bytes()),
            Some(_) => {
                return Err(QsError::ConfigError(
                    "cookie_secret must have at least 32 characters".into(),
                ))
            }
            None => {
                log::warn!("No cookie_secret configured: users are logged out on restart");
                Key::generate()
            }
        };
        let tmp_dir = qs
            .tmp_dir()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("quickstatements"));
        Ok(Self {
            oauth,
            cookie_key,
            secure_cookies: tool_base.starts_with("https://"),
            origins: allowed_origins(config, &tool_base),
            tmp_dir,
            entity_base: site["entityBase"].as_str().unwrap_or_default().to_string(),
            public_config: Arc::new(public_config(config)),
            default_site,
            default_api,
            tool_base,
            qs,
        })
    }
}

/// `valid_origin` from the frontend config (a string or a list), and the tool's own origin
fn allowed_origins(config: &Value, tool_base: &str) -> Vec<String> {
    let mut origins: Vec<String> = match &config["valid_origin"] {
        Value::String(origin) => vec![origin.clone()],
        Value::Array(origins) => origins
            .iter()
            .filter_map(|o| Some(o.as_str()?.to_string()))
            .collect(),
        _ => vec![],
    };
    if let Ok(url) = Url::parse(tool_base) {
        origins.push(url.origin().ascii_serialization());
    }
    origins.retain(|o| !o.is_empty());
    origins
}

fn public_config(config: &Value) -> Value {
    let sites: Map<String, Value> = config["sites"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, site)| {
            let site: Map<String, Value> = PUBLIC_SITE_KEYS
                .iter()
                .filter_map(|key| Some((key.to_string(), site.get(*key)?.clone())))
                .collect();
            (name.clone(), site.into())
        })
        .collect();
    json!({ "site": config["site"], "sites": sites })
}

/// A random string of letters and digits, e.g. for tokens and nonces
fn random_string(len: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), len)
}

/// Runs the web service on `port`
pub async fn serve(qs: Arc<QuickStatements>, port: u16) -> QsResult<()> {
    let state = AppState::new(qs)?;
    import::expire_temporary_files(state.tmp_dir.clone());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    log::info!("QuickStatements web service listening on port {port}");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/api.php", any(api::handle))
        .route("/config.json", get(serve_public_config))
        .route("/tmp/{name}", get(import::serve_temporary))
        .fallback(assets::serve)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::from_fn(security_headers));
    add_middleware(router).with_state(state)
}

async fn serve_public_config(State(state): State<AppState>) -> Json<Value> {
    Json(state.public_config.as_ref().clone())
}

/// A panicking handler answers 500 instead of dropping the connection. Later
/// layers wrap earlier ones, so `record_request` still sees that 500.
fn add_middleware<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn(record_request))
}

/// Reports every request's status and duration to telemetry (a no-op without it).
async fn record_request(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let response = next.run(request).await;
    wikimisc::telemetry::record_http_request(response.status().as_u16(), start.elapsed());
    response
}

/// No content sniffing, and no framing (against clickjacking)
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_config_hides_private_settings() {
        let config = json!({
            "site": "wikidata",
            "bot_config_file": "/secret/bot.ini",
            "sites": {"wikidata": {"label": "Wikidata", "api": "https://w/api.php", "oauth": {"ini_file": "/secret/oauth.ini"}}},
        });
        assert_eq!(
            public_config(&config),
            json!({"site": "wikidata", "sites": {"wikidata": {"label": "Wikidata", "api": "https://w/api.php"}}})
        );
    }

    #[test]
    fn allowed_origins_include_the_tool() {
        let config = json!({"valid_origin": ["https://a.example", ""]});
        assert_eq!(
            allowed_origins(&config, "https://qs.example/"),
            ["https://a.example", "https://qs.example"]
        );
        assert!(allowed_origins(&json!({}), "./").is_empty());
    }

    #[test]
    fn random_strings_differ() {
        assert_eq!(random_string(20).len(), 20);
        assert_ne!(random_string(20), random_string(20));
    }

    #[tokio::test]
    async fn panicking_handler_returns_500() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn panics() -> &'static str {
            panic!("test panic")
        }
        let router: Router = add_middleware(Router::new().route("/panic", get(panics)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /panic HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    }
}

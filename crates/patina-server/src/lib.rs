//! HTTP server: API under `/api/v1`, the SSE event stream, auth, and the embedded UI.
pub mod api;
pub mod auth;
pub mod error;
pub mod runs;
pub mod state;

use axum::{
    Router,
    http::{StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use rust_embed::RustEmbed;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::compression::CompressionLayer;

pub use state::{ProviderFactory, genai_factory};

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
struct Assets;

pub fn router(st: Arc<AppState>) -> Router {
    // Everything here needs a session; unsafe methods also need the CSRF header and a matching Origin.
    let protected = Router::new()
        .route("/system", get(api::system))
        .route("/provider-presets", get(api::list_presets))
        .route(
            "/providers",
            get(api::list_providers).post(api::create_provider),
        )
        .route(
            "/providers/{id}",
            put(api::update_provider).delete(api::delete_provider),
        )
        .route("/providers/{id}/secret", put(api::put_secret))
        .route("/providers/{id}/test", post(api::test_provider))
        .route("/providers/{id}/models", get(api::provider_models))
        .route("/agents", get(api::list_agents).post(api::create_agent))
        .route(
            "/agents/{id}",
            put(api::update_agent).delete(api::delete_agent),
        )
        .route("/runs", get(runs::list_runs).post(runs::create_run))
        .route("/runs/{id}", get(runs::get_run))
        .route("/runs/{id}/cancel", post(runs::cancel_run))
        .route("/runs/{id}/events", get(runs::run_events))
        .layer(middleware::from_fn_with_state(
            st.clone(),
            auth::require_session,
        ));
    let public = Router::new()
        .route("/auth/setup", post(auth::setup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session_state));
    let api = public
        .merge(protected)
        .fallback(|| async { error::ApiError::not_found("route") });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/healthz", get(healthz))
        .nest("/api/v1", api)
        .fallback(static_asset)
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(auth::security_headers))
        .layer(middleware::from_fn_with_state(st.clone(), auth::host_guard))
        .with_state(st)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            // Hashed build assets never change; the page itself must always be fresh.
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, mime_for(path)),
                    (header::CACHE_CONTROL, cache),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// If no owner password exists, issue a fresh one-time setup token and return it for printing.
pub async fn ensure_setup_token(
    db: &patina_store::Db,
) -> Result<Option<String>, patina_store::StoreError> {
    use patina_store::auth as a;
    if a::get_setting(db, a::PASSWORD_HASH).await?.is_some() {
        return Ok(None);
    }
    let token = a::new_token();
    a::set_setting(db, a::SETUP_TOKEN_HASH, Some(&a::hash_token(&token))).await?;
    Ok(Some(token))
}

/// Forget the password and every session, and return a new setup token.
pub async fn reset_password(db: &patina_store::Db) -> Result<String, patina_store::StoreError> {
    use patina_store::auth as a;
    a::set_setting(db, a::PASSWORD_HASH, None).await?;
    a::delete_all_sessions(db).await?;
    Ok(ensure_setup_token(db).await?.unwrap_or_default())
}

pub async fn has_password(db: &patina_store::Db) -> Result<bool, patina_store::StoreError> {
    Ok(
        patina_store::auth::get_setting(db, patina_store::auth::PASSWORD_HASH)
            .await?
            .is_some(),
    )
}

/// Bind the listener. Split from `serve` so callers (and tests) can learn the real port.
pub async fn bind(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    st: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let runs_state = st.clone();
    let shutdown = async move {
        shutdown.await;
        // Stop model streams so open SSE connections finish and the listener can drain.
        for h in runs_state.runs.lock().unwrap().values() {
            h.cancel.cancel();
        }
    };
    axum::serve(listener, router(st))
        .with_graceful_shutdown(shutdown)
        .await
}

//! HTTP server: API under `/api/v1`, the SSE event stream, auth, and the embedded UI.
pub mod api;
pub mod auth;
pub mod changes;
pub mod engine;
pub mod error;
pub mod files;
pub mod preview;
pub mod projects;
pub mod runs;
pub mod state;
pub mod teams;

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
        .route("/providers", get(api::list_providers).post(api::create_provider))
        .route("/providers/{id}", put(api::update_provider).delete(api::delete_provider))
        .route("/providers/{id}/secret", put(api::put_secret))
        .route("/providers/{id}/test", post(api::test_provider))
        .route("/providers/{id}/models", get(api::provider_models))
        .route("/agents", get(api::list_agents).post(api::create_agent))
        .route("/agents/{id}", put(api::update_agent).delete(api::delete_agent))
        .route("/teams", get(teams::list).post(teams::create))
        .route("/teams/import", post(teams::import))
        .route("/teams/{id}", put(teams::update).delete(teams::delete))
        .route("/teams/{id}/export", get(teams::export))
        .route("/team-templates", get(teams::templates))
        .route("/team-templates/{id}", post(teams::from_template))
        .route("/runs", get(runs::list_runs).post(runs::create_run))
        .route("/runs/{id}", get(runs::get_run))
        .route("/runs/{id}/cancel", post(runs::cancel_run))
        .route("/runs/{id}/pause", post(runs::pause_run))
        .route("/runs/{id}/resume", post(runs::resume_run))
        .route("/runs/{id}/messages", post(runs::send_message))
        .route("/runs/{id}/events", get(runs::run_events))
        .route("/runs/{id}/budgets", axum::routing::patch(runs::patch_budgets))
        .route("/approvals", get(runs::list_approvals))
        .route("/runs/{id}/approvals/{approval_id}", post(runs::answer_approval))
        .route("/runs/{id}/diff", get(changes::diff))
        .route("/runs/{id}/checkpoints", get(changes::checkpoints))
        .route("/runs/{id}/rollback", post(changes::rollback))
        .route("/runs/{id}/merge", post(changes::merge))
        .route("/runs/{id}/discard", post(changes::discard))
        .route("/runs/{id}/diff/file", get(files::file_diff))
        .route("/runs/{id}/export", get(files::export))
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/{id}", axum::routing::patch(projects::update).delete(projects::delete))
        .route("/projects/{id}/tree", get(files::tree))
        .route("/projects/{id}/file", get(files::read).put(files::write))
        .route("/preview", get(preview::get).put(preview::select))
        .route("/preview/start", post(preview::start))
        .route("/preview/stop", post(preview::stop))
        .layer(middleware::from_fn_with_state(st.clone(), auth::require_session));
    let public = Router::new()
        .route("/auth/setup", post(auth::setup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session_state));
    let api = public.merge(protected).fallback(|| async { error::ApiError::not_found("route") });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/healthz", get(healthz))
        .nest("/api/v1", api)
        .fallback(static_asset)
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn_with_state(st.clone(), auth::security_headers))
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
                [(header::CONTENT_TYPE, mime_for(path)), (header::CACHE_CONTROL, cache)],
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
pub async fn ensure_setup_token(db: &patina_store::Db) -> Result<Option<String>, patina_store::StoreError> {
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
    Ok(patina_store::auth::get_setting(db, patina_store::auth::PASSWORD_HASH)
        .await?
        .is_some())
}

/// Serve the preview on its own listener until shutdown. It binds to loopback only; see `preview`.
pub async fn serve_preview(
    listener: tokio::net::TcpListener,
    st: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let _ = st.preview.addr.set(addr);
    tracing::info!(%addr, "preview listening");
    axum::serve(listener, preview::router(st)).with_graceful_shutdown(shutdown).await
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
    match engine::recover(&st).await {
        Ok(ids) if !ids.is_empty() => tracing::info!(runs = ids.len(), "found runs that were in progress when Patina stopped"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e.message, "could not check for interrupted runs"),
    }
    // Retention runs at start and every six hours.
    let pruner = {
        let st = st.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            loop {
                every.tick().await;
                match engine::prune(&st).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(runs = n, "removed the logs of old runs (retention)"),
                    Err(e) => tracing::warn!(error = %e.message, "retention failed"),
                }
            }
        })
    };
    let runs_state = st.clone();
    let shutdown = async move {
        shutdown.await;
        // Runs record themselves as interrupted, so they can be resumed; their streams then end and the listener
        // can drain.
        engine::shutdown(&runs_state, SHUTDOWN_LIMIT).await;
    };
    let served = axum::serve(listener, router(st.clone())).with_graceful_shutdown(shutdown).await;
    pruner.abort();
    // A run that was still recording when the connections drained gets the rest of its time.
    engine::shutdown(&st, SHUTDOWN_LIMIT).await;
    served
}

/// How long a shutdown waits for runs to record that they were interrupted.
const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

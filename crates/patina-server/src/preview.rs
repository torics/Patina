//! Preview: a second listener, on loopback only, that reverse-proxies the dev server of the run the owner selected.
//!
//! Each run gets its own port. Its commands see it as `PORT`, and `{port}` in the project's `dev_command` becomes
//! it. The listener has its own origin, so the page it serves cannot reach the app's cookies or API, and the UI
//! loads it in a sandboxed iframe. It forwards no cookie and passes on no `Set-Cookie` or CORS header.
use crate::engine;
use crate::error::{ApiError, ApiResult};
use crate::runs::load;
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};
use futures::TryStreamExt;
use patina_core::api::{PreviewSelect, PreviewView};
use patina_core::sandbox::Sandbox;
use patina_tools::process::{self, Running};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

/// The longest a request to the dev server may take before the preview gives up.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A dev command that Patina started for a run.
struct Dev {
    run_id: String,
    running: Running,
    sandbox: Arc<dyn Sandbox>,
}

pub struct Preview {
    /// The run whose dev server the listener serves.
    selected: Mutex<Option<String>>,
    /// Each run's port, chosen when first needed. They live as long as the daemon.
    ports: Mutex<HashMap<String, u16>>,
    dev: tokio::sync::Mutex<Option<Dev>>,
    /// Where the listener is, once it is bound.
    pub addr: OnceLock<SocketAddr>,
    client: reqwest::Client,
}

impl Default for Preview {
    fn default() -> Self {
        Preview {
            selected: Mutex::default(),
            ports: Mutex::default(),
            dev: Default::default(),
            addr: OnceLock::new(),
            // The dev server is on this machine: never through a proxy, and redirects go back to the browser.
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .expect("an HTTP client with no TLS settings builds"),
        }
    }
}

impl Preview {
    /// The run's preview port. A free port is picked the first time and kept for the daemon's lifetime.
    pub fn port_for(&self, run_id: &str) -> Option<u16> {
        let mut ports = self.ports.lock().unwrap();
        if let Some(p) = ports.get(run_id) {
            return Some(*p);
        }
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?.local_addr().ok()?.port();
        ports.insert(run_id.to_string(), port);
        Some(port)
    }

    fn selected(&self) -> Option<String> {
        self.selected.lock().unwrap().clone()
    }

    /// Stop the dev command Patina started, if any.
    pub async fn stop_dev(&self) {
        if let Some(mut dev) = self.dev.lock().await.take() {
            dev.running.stop(dev.sandbox.as_ref()).await;
            dev.sandbox.shutdown().await;
        }
    }

    /// The run's worktree is going away: stop its dev command and stop previewing it.
    pub async fn forget(&self, run_id: &str) {
        let mine = self.dev.lock().await.as_ref().is_some_and(|d| d.run_id == run_id);
        if mine {
            self.stop_dev().await;
        }
        let mut selected = self.selected.lock().unwrap();
        if selected.as_deref() == Some(run_id) {
            *selected = None;
        }
    }
}

type St = State<Arc<AppState>>;

async fn view(st: &AppState) -> ApiResult<PreviewView> {
    let Some(run_id) = st.preview.selected() else {
        return Ok(PreviewView {
            run_id: None,
            port: None,
            dev_command: None,
            running: false,
            output: String::new(),
        });
    };
    let port = st.preview.port_for(&run_id);
    let run = load(st, &run_id).await?;
    let dev_command = st
        .project(&run.project_id)
        .ok()
        .and_then(|p| p.dev_command)
        .zip(port)
        .map(|(c, p)| c.replace("{port}", &p.to_string()));
    let secrets = st.redactor().await;
    let mut dev = st.preview.dev.lock().await;
    let (running, output) = match dev.as_mut().filter(|d| d.run_id == run_id) {
        Some(d) => {
            let alive = matches!(d.running.child.try_wait(), Ok(None));
            let (text, _) = d.running.output();
            let text = secrets.redact(&text).into_owned();
            // The last part is what the owner needs: the address it listens on, or why it stopped.
            let tail: String = text.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
            (alive, tail)
        }
        None => (false, String::new()),
    };
    Ok(PreviewView {
        run_id: Some(run_id),
        port,
        dev_command,
        running,
        output,
    })
}

pub async fn get(State(st): St) -> ApiResult<Json<PreviewView>> {
    Ok(Json(view(&st).await?))
}

/// Choose the run the preview shows. A dev command started for another run is stopped.
pub async fn select(State(st): St, Json(input): Json<PreviewSelect>) -> ApiResult<Json<PreviewView>> {
    if st.preview.addr.get().is_none() {
        return Err(ApiError::conflict(
            "preview_off",
            "the preview listener is off; set preview_port in config.toml",
        ));
    }
    if let Some(id) = &input.run_id {
        let run = load(&st, id).await?;
        if run.worktree_path.is_none() {
            return Err(ApiError::conflict("no_worktree", "this run has no worktree to preview"));
        }
    }
    let other = st
        .preview
        .dev
        .lock()
        .await
        .as_ref()
        .is_some_and(|d| Some(&d.run_id) != input.run_id.as_ref());
    if other {
        st.preview.stop_dev().await;
    }
    *st.preview.selected.lock().unwrap() = input.run_id;
    Ok(Json(view(&st).await?))
}

/// Start the project's dev command in the selected run's worktree, inside the run's sandbox.
pub async fn start(State(st): St) -> ApiResult<Json<PreviewView>> {
    let run_id = st
        .preview
        .selected()
        .ok_or_else(|| ApiError::conflict("no_preview", "choose a run to preview first"))?;
    let run = load(&st, &run_id).await?;
    if run.worktree_path.is_none() {
        return Err(ApiError::conflict("no_worktree", "this run has no worktree"));
    }
    let project = st.project(&run.project_id)?;
    let port = st
        .preview
        .port_for(&run_id)
        .ok_or_else(|| ApiError::internal("no free port for the preview"))?;
    let command = project
        .dev_command
        .clone()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| ApiError::conflict("no_dev_command", "the project has no dev_command; set one in its settings"))?
        .replace("{port}", &port.to_string());
    st.preview.stop_dev().await;
    let sandbox = engine::sandbox_for(&st, &project, &run_id, true).await?;
    let marker = format!("{run_id}-preview");
    let fail = |e: String| ApiError::conflict("dev_command_failed", format!("the dev command could not be started: {e}"));
    sandbox.ensure_ready().await.map_err(|e| fail(e.to_string()))?;
    let prepared = sandbox.prepare(&command, &marker).map_err(|e| fail(e.to_string()))?;
    let running = process::spawn(&prepared, &marker).map_err(|e| fail(e.to_string()))?;
    *st.preview.dev.lock().await = Some(Dev { run_id, running, sandbox });
    Ok(Json(view(&st).await?))
}

pub async fn stop(State(st): St) -> ApiResult<Json<PreviewView>> {
    st.preview.stop_dev().await;
    Ok(Json(view(&st).await?))
}

/// The preview listener's routes: everything goes to the selected run's dev server.
pub fn router(st: Arc<AppState>) -> Router {
    Router::new()
        .fallback(proxy)
        .layer(middleware::from_fn_with_state(st.clone(), crate::auth::host_guard))
        .with_state(st)
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Request headers that stay behind: hop-by-hop ones, and everything that identifies the owner to the app.
fn forwarded(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == "host" || n == "cookie" || n == "authorization" || n.starts_with("x-patina") {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Response headers that stay behind: hop-by-hop ones, cookies (they would be set for the app's host name too),
/// and CORS grants, so no other web page can read the preview.
fn returned(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == "set-cookie" || n.starts_with("access-control-") || n == "content-length" {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn page(status: StatusCode, text: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        text.to_string(),
    )
        .into_response()
}

async fn proxy(State(st): St, req: Request) -> Response {
    let Some(run_id) = st.preview.selected() else {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Nothing to preview. Choose a run in Patina's Preview tab.",
        );
    };
    let Some(port) = st.preview.port_for(&run_id) else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The run has no preview port.");
    };
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut headers = forwarded(req.headers());
    // Dev servers check the host name they are reached by; this is the one they listen on.
    if let Ok(h) = HeaderValue::from_str(&format!("localhost:{port}")) {
        headers.insert(header::HOST, h);
    }
    let method = req.method().clone();
    let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());
    let upstream = st.preview.client.request(method, &url).headers(headers).body(body).send().await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(_) => {
            return page(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "Nothing answers on the run's port {port}. Start the dev server from Patina's Preview tab, or have the agent start it on $PORT."
                ),
            );
        }
    };
    let status = upstream.status();
    let headers = returned(upstream.headers());
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cookie_or_cors_grant_crosses_the_proxy() {
        let mut req = HeaderMap::new();
        req.insert(header::COOKIE, "patina_session=abc".parse().unwrap());
        req.insert("x-patina-csrf", "t".parse().unwrap());
        req.insert(header::ACCEPT, "text/html".parse().unwrap());
        req.insert(header::CONNECTION, "keep-alive".parse().unwrap());
        let out = forwarded(&req);
        assert_eq!(out.keys().map(|k| k.as_str()).collect::<Vec<_>>(), ["accept"]);

        let mut res = HeaderMap::new();
        res.insert(header::SET_COOKIE, "patina_session=evil".parse().unwrap());
        res.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
        res.insert(header::CONTENT_TYPE, "text/html".parse().unwrap());
        let out = returned(&res);
        assert_eq!(out.keys().map(|k| k.as_str()).collect::<Vec<_>>(), ["content-type"]);
    }
}

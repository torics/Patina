//! Authentication, sessions, CSRF, and the Host and Origin checks.
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use patina_core::events::now_ms;
use patina_store::auth as store;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "patina_session";
pub const CSRF_HEADER: &str = "x-patina-csrf";
const SESSION_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
pub const MIN_PASSWORD_LEN: usize = 10;

/// Five failures lock sign-in for a minute; each further lockout doubles the wait.
#[derive(Default)]
pub struct Limiter {
    failures: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
}

impl Limiter {
    pub const MAX_FAILURES: u32 = 5;
    pub const BASE_LOCK: Duration = Duration::from_secs(60);

    /// `Err(wait)` while locked out.
    pub fn check(&self, now: Instant) -> Result<(), Duration> {
        match self.locked_until {
            Some(t) if t > now => Err(t - now),
            _ => Ok(()),
        }
    }

    pub fn fail(&mut self, now: Instant) {
        self.failures += 1;
        if self.failures >= Self::MAX_FAILURES {
            self.failures = 0;
            let wait = Self::BASE_LOCK * 2u32.saturating_pow(self.lockouts.min(16));
            self.lockouts += 1;
            self.locked_until = Some(now + wait);
        }
    }

    pub fn succeed(&mut self) {
        *self = Limiter::default();
    }
}

fn locked(wait: Duration) -> ApiError {
    let mut e = ApiError::new(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "locked_out",
        format!("too many failed attempts; try again in {} seconds", wait.as_secs() + 1),
    );
    e.retry_after_s = Some(wait.as_secs() + 1);
    e
}

/// Hostname of a `Host` header value, without the port (IPv6 literals keep their brackets).
fn host_name(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    if let Some(end) = h.find(']') {
        return h[..=end].to_string();
    }
    h.split(':').next().unwrap_or("").to_string()
}

/// Reject requests whose `Host` is not on the allowed list (DNS rebinding defense).
pub async fn host_guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).map(host_name);
    let ok = host
        .as_ref()
        .is_some_and(|h| st.config.allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(h)));
    if !ok {
        return ApiError::forbidden("host_not_allowed", "this host name is not in allowed_hosts").into_response();
    }
    next.run(req).await
}

/// The origins the preview may be framed from: the configured one, or the app's host name on the preview port.
fn preview_sources(st: &AppState, host: Option<&str>) -> String {
    let Some(addr) = st.preview.addr.get() else {
        return String::new();
    };
    if let Some(origin) = &st.config.preview_origin {
        return format!("; frame-src {origin}");
    }
    let Some(host) = host.map(host_name).filter(|h| !h.is_empty()) else {
        return String::new();
    };
    let port = addr.port();
    format!("; frame-src http://{host}:{port} https://{host}:{port}")
}

/// Security headers on every response. Frames may come only from the preview origin.
pub async fn security_headers(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut r = next.run(req).await;
    let csp = format!(
        "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'{}",
        preview_sources(&st, host.as_deref())
    );
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp)
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'")),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if is_api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    r
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

fn is_unsafe(m: &Method) -> bool {
    !matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// A state-changing request must carry an `Origin` that names this same host.
fn check_origin(headers: &HeaderMap) -> ApiResult<()> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let authority = origin
        .and_then(|o| o.split_once("://"))
        .map(|(_, a)| a.trim_end_matches('/').to_ascii_lowercase());
    match (authority, host) {
        (Some(a), Some(h)) if a == h.to_ascii_lowercase() => Ok(()),
        _ => Err(ApiError::forbidden("bad_origin", "missing or mismatched Origin header")),
    }
}

#[derive(Clone)]
pub struct SessionInfo {
    pub token: String,
    pub csrf: String,
}

/// Guards the protected API: a valid session, and for unsafe methods a matching CSRF header and Origin.
pub async fn require_session(State(st): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    // Copy what is needed first: a borrowed request cannot be held across an await.
    let (headers, method) = (req.headers().clone(), req.method().clone());
    match authorize(&st, &headers, &method).await {
        Ok(info) => {
            req.extensions_mut().insert(info);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

async fn authorize(st: &AppState, headers: &HeaderMap, method: &Method) -> ApiResult<SessionInfo> {
    let token = cookie_value(headers, COOKIE).ok_or_else(ApiError::unauthorized)?;
    let session = store::lookup_session(&st.db, &token, now_ms())
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if is_unsafe(method) {
        check_origin(headers)?;
        let sent = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !store::constant_time_eq(sent, &session.csrf_token) {
            return Err(ApiError::forbidden("bad_csrf", "missing or invalid CSRF token"));
        }
    }
    Ok(SessionInfo {
        token,
        csrf: session.csrf_token,
    })
}

fn session_cookie(token: &str, headers: &HeaderMap, max_age_s: i64) -> String {
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_s}{}",
        if secure { "; Secure" } else { "" }
    )
}

async fn start_session(st: &AppState, headers: &HeaderMap) -> ApiResult<Response> {
    let (token, csrf) = store::create_session(&st.db, now_ms(), SESSION_TTL_MS).await?;
    let mut r = Json(json!({"csrf": csrf})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&token, headers, SESSION_TTL_MS / 1000)).map_err(ApiError::internal)?,
    );
    Ok(r)
}

async fn hash_password(pw: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || Argon2::default().hash_password(pw.as_bytes()).map(|h| h.to_string()))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
}

async fn verify_password(pw: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&stored).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
    })
    .await
    .unwrap_or(false)
}

#[derive(Deserialize)]
pub struct SetupBody {
    token: String,
    password: String,
}

pub async fn setup(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(b): Json<SetupBody>) -> ApiResult<Response> {
    check_origin(&headers)?;
    st.limiter.lock().unwrap().check(Instant::now()).map_err(locked)?;
    let Some(expected) = store::get_setting(&st.db, store::SETUP_TOKEN_HASH).await? else {
        return Err(ApiError::conflict(
            "already_set_up",
            "an owner password is already set; use `patina reset-password`",
        ));
    };
    if !store::constant_time_eq(&store::hash_token(b.token.trim()), &expected) {
        st.limiter.lock().unwrap().fail(Instant::now());
        return Err(ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "bad_setup_token",
            "the setup token is wrong",
        ));
    }
    if b.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(ApiError::bad_request(format!(
            "the password needs at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let hash = hash_password(b.password).await?;
    store::set_setting(&st.db, store::PASSWORD_HASH, Some(&hash)).await?;
    store::set_setting(&st.db, store::SETUP_TOKEN_HASH, None).await?;
    store::delete_all_sessions(&st.db).await?;
    st.limiter.lock().unwrap().succeed();
    start_session(&st, &headers).await
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(State(st): State<Arc<AppState>>, headers: HeaderMap, Json(b): Json<LoginBody>) -> ApiResult<Response> {
    check_origin(&headers)?;
    st.limiter.lock().unwrap().check(Instant::now()).map_err(locked)?;
    let Some(stored) = store::get_setting(&st.db, store::PASSWORD_HASH).await? else {
        return Err(ApiError::conflict("setup_required", "set the owner password first"));
    };
    if !verify_password(b.password, stored).await {
        st.limiter.lock().unwrap().fail(Instant::now());
        return Err(ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "wrong password",
        ));
    }
    st.limiter.lock().unwrap().succeed();
    start_session(&st, &headers).await
}

pub async fn logout(State(st): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    check_origin(&headers)?;
    let token = cookie_value(&headers, COOKIE).ok_or_else(ApiError::unauthorized)?;
    let session = store::lookup_session(&st.db, &token, now_ms())
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let sent = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !store::constant_time_eq(sent, &session.csrf_token) {
        return Err(ApiError::forbidden("bad_csrf", "missing or invalid CSRF token"));
    }
    store::delete_session(&st.db, &token).await?;
    let mut r = Json(json!({"ok": true})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie("", &headers, 0)).map_err(ApiError::internal)?,
    );
    Ok(r)
}

/// Tells the UI which screen to show. It reveals no secret and the CSRF token only to a signed-in caller.
pub async fn session_state(State(st): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<serde_json::Value>> {
    let setup_required = store::get_setting(&st.db, store::SETUP_TOKEN_HASH).await?.is_some();
    let session = match cookie_value(&headers, COOKIE) {
        Some(t) => store::lookup_session(&st.db, &t, now_ms()).await?,
        None => None,
    };
    Ok(Json(json!({
        "setup_required": setup_required,
        "authenticated": session.is_some(),
        "csrf": session.map(|s| s.csrf_token),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_locks_after_five_and_doubles() {
        let mut l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..4 {
            l.fail(t0);
        }
        assert!(l.check(t0).is_ok());
        l.fail(t0);
        assert_eq!(l.check(t0).unwrap_err(), Duration::from_secs(60));
        assert!(l.check(t0 + Duration::from_secs(61)).is_ok());
        for _ in 0..5 {
            l.fail(t0 + Duration::from_secs(61));
        }
        assert_eq!(l.check(t0 + Duration::from_secs(61)).unwrap_err(), Duration::from_secs(120));
        l.succeed();
        assert!(l.check(t0 + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn host_names_drop_ports_and_keep_ipv6_brackets() {
        assert_eq!(host_name("LocalHost:7878"), "localhost");
        assert_eq!(host_name("[::1]:7878"), "[::1]");
        assert_eq!(host_name("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn origin_must_match_host() {
        let mk = |o: Option<&str>, h: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, h.parse().unwrap());
            if let Some(o) = o {
                m.insert(header::ORIGIN, o.parse().unwrap());
            }
            m
        };
        assert!(check_origin(&mk(Some("http://localhost:7878"), "localhost:7878")).is_ok());
        assert!(check_origin(&mk(Some("https://evil.example"), "localhost:7878")).is_err());
        assert!(check_origin(&mk(None, "localhost:7878")).is_err());
        assert!(check_origin(&mk(Some("null"), "localhost:7878")).is_err());
    }

    #[test]
    fn cookie_parsing_finds_the_session() {
        let mut m = HeaderMap::new();
        m.insert(header::COOKIE, "a=1; patina_session=tok; b=2".parse().unwrap());
        assert_eq!(cookie_value(&m, COOKIE).as_deref(), Some("tok"));
        assert_eq!(cookie_value(&m, "zzz"), None);
    }
}

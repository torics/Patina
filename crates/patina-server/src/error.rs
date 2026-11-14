use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Errors leave the API as `{ "error": { "code", "message" } }`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after_s: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status, code, message: message.into(), retry_after_s: None }
    }
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn not_found(what: &str) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("{what} not found"))
    }
    pub fn unauthorized() -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "sign in required")
    }
    pub fn forbidden(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::FORBIDDEN, code, message)
    }
    pub fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, code, message)
    }
    /// Internal details go to the log, never to the client.
    pub fn internal(detail: impl std::fmt::Display) -> ApiError {
        tracing::error!(error = %detail, "internal error");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.status, Json(json!({"error": {"code": self.code, "message": self.message}}))).into_response();
        if let Some(s) = self.retry_after_s {
            if let Ok(v) = s.to_string().parse() {
                r.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        r
    }
}

impl From<patina_store::StoreError> for ApiError {
    fn from(e: patina_store::StoreError) -> ApiError {
        ApiError::internal(e)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

//! HTTP server: health check and the embedded UI.
use axum::{
    Router,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use std::net::SocketAddr;
use tower_http::compression::CompressionLayer;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
struct Assets;

pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(static_asset)
        .layer(CompressionLayer::new())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            ([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
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

/// Bind the listener. Split from `serve` so callers (and tests) can learn the real port.
pub async fn bind(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(port: u16, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn serves_health_page_and_404() {
        let l = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, std::future::pending()));
        assert!(get(port, "/healthz").await.starts_with("HTTP/1.1 200"));
        let page = get(port, "/").await;
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Patina"));
        assert!(get(port, "/nope.js").await.starts_with("HTTP/1.1 404"));
    }
}

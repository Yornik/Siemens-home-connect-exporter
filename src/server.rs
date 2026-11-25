//! HTTP server: /metrics and /healthz.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::error::Result;
use crate::metrics::Metrics;

/// Serves the metrics on the bound listener.
///
/// # Errors
/// Returns an error when the accept loop fails.
pub async fn serve(listener: tokio::net::TcpListener, metrics: Arc<Metrics>) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_endpoint))
        .route("/healthz", get(healthz))
        .with_state(metrics);
    axum::serve(listener, app)
        .await
        .map_err(crate::error::Error::Io)
}

/// Answers a scrape with the full exposition, or with a short error line
/// when the registry cannot be encoded.
async fn metrics_endpoint(State(metrics): State<Arc<Metrics>>) -> Response {
    match metrics.encode() {
        Ok(exposition) => {
            // The content type tells scrapers which exposition dialect to parse.
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            );
            (StatusCode::OK, headers, exposition).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encoding failed: {err}"),
        )
            .into_response(),
    }
}

/// Liveness probe: any answer at all means the server is up.
async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use crate::metrics::Metrics;
    use crate::server::serve;

    /// Binds to an ephemeral port, so parallel test runs never collide.
    #[tokio::test]
    async fn the_server_serves_metrics_and_healthz() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Arc::new(Metrics::new());
        metrics.set_up(true);
        let task = tokio::spawn(serve(listener, metrics));

        let response = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("homeconnect_up 1"));

        let response = reqwest::get(format!("http://{addr}/healthz"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");

        task.abort();
    }
}

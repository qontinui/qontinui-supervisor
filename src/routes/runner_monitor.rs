//! Runner Monitor proxy routes.
//!
//! Forwards all `/runner-api/*` requests to the runner's HTTP API at port 9876.
//! This gives the supervisor frontend access to runner endpoints like task-runs,
//! workflow-state, and output for monitoring running workflows.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use tracing::{debug, warn};

use crate::config::RUNNER_API_PORT;
use crate::state::SharedState;

/// Default timeout for runner monitor proxy requests (seconds).
const PROXY_TIMEOUT_SECS: u64 = 15;

/// Proxy handler for all runner monitor requests.
///
/// Forwards the request to `http://127.0.0.1:9876/{path}` (stripping the
/// `/runner-api` prefix), preserving method, query string, headers, and body.
pub async fn proxy(State(state): State<SharedState>, req: Request) -> Response {
    // Check runner health from cache first
    let cached = state.cached_health.read().await;
    if !cached.runner_responding {
        drop(cached);
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "Runner is not responding. Is qontinui-runner running?",
                "runner_port": RUNNER_API_PORT,
            })),
        )
            .into_response();
    }
    drop(cached);

    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();

    // Strip the /runner-api prefix to get the actual runner path
    let runner_path = path.strip_prefix("/runner-api").unwrap_or(path);

    let target_url = format!(
        "http://127.0.0.1:{}{}{}",
        RUNNER_API_PORT, runner_path, query
    );
    debug!(
        "Runner monitor proxy: {} {} -> {}",
        method, path, target_url
    );

    let client = &state.http_client;

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Failed to read request body: {e}")})),
            )
                .into_response();
        }
    };

    let mut outgoing = client
        .request(method, &target_url)
        .timeout(std::time::Duration::from_secs(PROXY_TIMEOUT_SECS));
    if let Some(ct) = content_type {
        outgoing = outgoing.header("content-type", ct);
    }
    if !body_bytes.is_empty() {
        outgoing = outgoing.body(body_bytes);
    }

    match outgoing.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

            let resp_content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            match resp.bytes().await {
                Ok(bytes) => {
                    let mut builder = Response::builder().status(status);
                    if let Some(ct) = resp_content_type {
                        builder = builder.header("content-type", ct);
                    }
                    builder
                        .body(Body::from(bytes))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("Failed to read runner response: {e}")})),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            let status = if e.is_connect() {
                StatusCode::BAD_GATEWAY
            } else if e.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };

            let msg = if e.is_connect() {
                format!("Cannot connect to runner at {target_url}. Is qontinui-runner running?")
            } else if e.is_timeout() {
                format!("Runner request timed out after {PROXY_TIMEOUT_SECS}s")
            } else {
                format!("Runner request failed: {e}")
            };

            warn!("Runner monitor proxy error: {}", msg);
            (status, Json(json!({"error": msg}))).into_response()
        }
    }
}

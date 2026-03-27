//! GraphQL proxy routes.
//!
//! Forwards `/graphql` POST requests to the runner's GraphQL endpoint at port 9876.
//! This enables the supervisor dashboard to access real-time graph data via the runner's
//! async-graphql schema.
//!
//! Note: WebSocket proxying for GraphQL subscriptions (`/graphql/ws`) requires
//! tokio-tungstenite wiring and is not yet implemented. The HTTP POST proxy covers
//! all query and mutation operations.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use tracing::{debug, warn};

use crate::config::RUNNER_API_PORT;
use crate::state::SharedState;

/// Default timeout for GraphQL proxy requests (seconds).
/// GraphQL queries may be heavier than simple REST calls, so allow more time.
const PROXY_TIMEOUT_SECS: u64 = 30;

/// Proxy handler for GraphQL POST requests.
///
/// Forwards the request to `http://127.0.0.1:9876/graphql` preserving
/// the JSON body and content-type headers.
pub async fn graphql_proxy(State(state): State<SharedState>, req: Request) -> Response {
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

    let target_url = format!("http://127.0.0.1:{}/graphql", RUNNER_API_PORT);
    debug!("GraphQL proxy: POST /graphql -> {}", target_url);

    let client = &state.http_client;

    // Extract content-type header from the incoming request
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Read the request body
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

    // Build the outgoing request
    let mut outgoing = client
        .post(&target_url)
        .timeout(std::time::Duration::from_secs(PROXY_TIMEOUT_SECS));
    if let Some(ct) = content_type {
        outgoing = outgoing.header("content-type", ct);
    } else {
        // Default to application/json for GraphQL
        outgoing = outgoing.header("content-type", "application/json");
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
                format!(
                    "Cannot connect to runner GraphQL at {target_url}. Is qontinui-runner running?"
                )
            } else if e.is_timeout() {
                format!("GraphQL request timed out after {PROXY_TIMEOUT_SECS}s")
            } else {
                format!("GraphQL proxy request failed: {e}")
            };

            warn!("GraphQL proxy error: {}", msg);
            (status, Json(json!({"error": msg}))).into_response()
        }
    }
}

/// Placeholder for GraphQL WebSocket subscription proxy.
///
/// Full WebSocket proxying requires tokio-tungstenite to upgrade the connection
/// and relay frames between the client and runner. For now, returns a helpful
/// error message directing clients to connect to the runner directly.
pub async fn graphql_ws_proxy() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "GraphQL WebSocket proxy not yet implemented. Connect directly to ws://127.0.0.1:9876/graphql/ws for subscriptions.",
            "runner_ws_url": format!("ws://127.0.0.1:{}/graphql/ws", RUNNER_API_PORT),
        })),
    )
}

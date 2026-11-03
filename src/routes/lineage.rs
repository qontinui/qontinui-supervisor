//! Commit ↔ session lineage proxy.
//!
//! Forwards the supervisor dashboard's lineage requests to qontinui-coord's
//! `/coord/lineage/*`, `/coord/commits/{sha}/session`, and
//! `/coord/sessions/{id}/commits` endpoints. The supervisor holds no coord
//! credential of its own; if the caller (the dashboard) attaches an
//! `Authorization` header it is forwarded verbatim — the coord lineage
//! endpoints are tenant/operator scoped and may require one on a deployed
//! target. Local dev coord typically reads without a token, so the header is
//! optional (consistent with `fleet.rs`'s unauthenticated budget publish).
//!
//! The coord base is [`crate::coord_client::coord_base`] under the config's
//! `ambient_coord`: `COORD_HTTP_URL`, else the active profile's `coord_url`, in
//! production; no coord at all under `AmbientCoord::Ignore`.
//!
//! When coord can't be resolved or is unreachable the proxy returns a
//! `502`/`503` with a descriptive `{error}` body so the page renders a clean
//! empty-state rather than hanging.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::config::AmbientCoord;
use crate::coord_client::coord_base;
use crate::state::SharedState;

/// Timeout for the outbound coord request. Lineage reads are small aggregate
/// queries; coord may be remote (staging), so allow a little headroom.
const LINEAGE_TIMEOUT_SECS: u64 = 10;

/// Upper bound on a forwarded `Authorization` header value.
const MAX_HEADER_VALUE_LEN: usize = 4096;

// ---------------------------------------------------------------------------
// Query types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    /// Max rows to return; forwarded to coord verbatim. Optional.
    pub limit: Option<u32>,
}

// ---------------------------------------------------------------------------
// Shared forwarder
// ---------------------------------------------------------------------------

/// Forward a GET to `{coord_base}{path}` with the caller's `Authorization`
/// header (if any). Returns coord's status + body verbatim; maps resolution
/// and transport failures to descriptive `5xx` JSON.
async fn forward_get(state: &SharedState, headers: &HeaderMap, coord_path: &str) -> Response {
    let base = match coord_base(&state.config.ambient_coord) {
        Some(b) => b,
        None => {
            let error = match state.config.ambient_coord {
                AmbientCoord::Ignore => "No coord: this supervisor's ambient coord is Ignore.",
                _ => {
                    "Could not resolve coord URL. Set COORD_HTTP_URL or configure \
                     a coord_url in ~/.qontinui/profiles.json."
                }
            };
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": error })),
            )
                .into_response();
        }
    };

    let url = format!("{base}{coord_path}");
    debug!("lineage proxy: GET {url}");

    let mut req = state
        .http_client
        .get(&url)
        .header(axum::http::header::ACCEPT, "application/json")
        .timeout(std::time::Duration::from_secs(LINEAGE_TIMEOUT_SECS));

    // Forward the caller's Authorization header verbatim if present and sane.
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        if auth.as_bytes().len() > MAX_HEADER_VALUE_LEN {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Authorization header exceeds maximum accepted length" })),
            )
                .into_response();
        }
        req = req.header(axum::http::header::AUTHORIZATION, auth);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            match resp.bytes().await {
                Ok(bytes) => {
                    let mut builder = Response::builder().status(status);
                    if let Some(ct) = content_type {
                        builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
                    }
                    builder
                        .body(Body::from(bytes))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                Err(e) => {
                    warn!("lineage proxy: failed to read coord response body: {e}");
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(
                            json!({ "error": format!("Failed to read coord response body: {e}") }),
                        ),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            let (status, msg) = if e.is_timeout() {
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("coord did not respond within {LINEAGE_TIMEOUT_SECS}s at {url}"),
                )
            } else if e.is_connect() {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Cannot connect to coord at {url}: {e}"),
                )
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("coord lineage request failed: {e}"),
                )
            };
            warn!("lineage proxy error: {msg}");
            (status, Json(json!({ "error": msg }))).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// `GET /lineage/recent?limit=N` → coord `/coord/lineage/recent`.
pub async fn recent(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(params): Query<RecentQuery>,
) -> Response {
    let path = match params.limit {
        Some(n) => format!("/coord/lineage/recent?limit={n}"),
        None => "/coord/lineage/recent".to_string(),
    };
    forward_get(&state, &headers, &path).await
}

/// `GET /lineage/stats` → coord `/coord/lineage/stats`.
pub async fn stats(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    forward_get(&state, &headers, "/coord/lineage/stats").await
}

/// `GET /lineage/sessions/{id}/commits` → coord `/coord/sessions/{id}/commits`.
pub async fn session_commits(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = match sanitize_id(&id) {
        Some(s) => s,
        None => return invalid_id_response("session id"),
    };
    forward_get(&state, &headers, &format!("/coord/sessions/{id}/commits")).await
}

/// `GET /lineage/commits/{sha}/session` → coord `/coord/commits/{sha}/session`.
pub async fn commit_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(sha): Path<String>,
) -> Response {
    let sha = match sanitize_id(&sha) {
        Some(s) => s,
        None => return invalid_id_response("commit sha"),
    };
    forward_get(&state, &headers, &format!("/coord/commits/{sha}/session")).await
}

/// SHAs (hex) and session UUIDs are the only path params, so restrict to the
/// safe identifier charset `[A-Za-z0-9._-]`. This both validates the input and
/// makes percent-encoding unnecessary (no external crate). Returns `None` for
/// empty or out-of-charset input.
fn sanitize_id(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 128 {
        return None;
    }
    if t.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        Some(t.to_string())
    } else {
        None
    }
}

fn invalid_id_response(what: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("invalid {what}: only [A-Za-z0-9._-] allowed") })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lineage proxy obeys the ambient-coord switch: a 503 with no
    /// connection under `Ignore`, coord's answer through one connection under
    /// `At`.
    #[tokio::test]
    async fn lineage_proxy_reaches_coord_only_under_at() {
        use crate::coord_client::counting_listener;
        let coord = counting_listener::serve().await;
        let state_with = |access: AmbientCoord| {
            let (mut config, root) = crate::config::config_over_an_empty_runner_tree();
            config.ambient_coord = access;
            let state: SharedState =
                std::sync::Arc::new(crate::state::SupervisorState::new(config));
            (state, root)
        };

        let (state, _root) = state_with(AmbientCoord::Ignore);
        let resp = forward_get(&state, &HeaderMap::new(), "/coord/lineage/stats").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(coord.connections(), 0, "Ignore reached coord");

        let (state, _root) = state_with(AmbientCoord::At(coord.base.clone()));
        let resp = forward_get(&state, &HeaderMap::new(), "/coord/lineage/stats").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(coord.connections(), 1);
    }
}

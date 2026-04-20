//! Web Fleet proxy route.
//!
//! Forwards `GET /web-fleet?backend_url=<url>` to `{backend_url}/api/v1/runners`
//! at the user-supplied qontinui-web backend, with the caller's `Authorization`
//! header attached verbatim.
//!
//! The supervisor does NOT hold any qontinui-web credentials. The dashboard is
//! responsible for collecting the backend URL and JWT from the user (persisted
//! to browser localStorage) and passing them on every request. This keeps the
//! supervisor a local dev tool while still letting the dashboard display the
//! team-wide runner registry that qontinui-web owns (Phase 3 of
//! `plans/restate-port-part-b-server-runner.md`).
//!
//! Read-only by design: there is no corresponding mutation proxy. Fleet
//! lifecycle (register, heartbeat, delete) belongs to qontinui-web.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::state::SharedState;

/// Timeout for the outbound request to the user-supplied web backend.
/// Remote fleet registries may be slower than a localhost proxy, so allow a
/// bit more headroom than the UI Bridge proxy while still bounding the wait.
const WEB_FLEET_TIMEOUT_SECS: u64 = 10;

/// Upper bound on the total size of a single header value accepted from the
/// caller. `Authorization: Bearer <jwt>` is the only header forwarded, and JWTs
/// fit comfortably in 4 KiB — anything larger is almost certainly abuse.
const MAX_HEADER_VALUE_LEN: usize = 4096;

#[derive(Debug, Deserialize)]
pub struct WebFleetQuery {
    /// Base URL of the qontinui-web backend, e.g. `https://api.qontinui.io`
    /// or `http://127.0.0.1:8000`. Only scheme + authority are used; any path
    /// or query string is rejected so the caller cannot turn this into an
    /// open proxy.
    pub backend_url: String,
}

/// `GET /web-fleet?backend_url=<url>` — proxy the qontinui-web fleet listing
/// using the caller's `Authorization` header.
pub async fn list_web_fleet(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(params): Query<WebFleetQuery>,
) -> Response {
    // Require an Authorization header. The supervisor holds no credentials;
    // the dashboard is expected to attach one per request.
    let auth_header = match headers.get(axum::http::header::AUTHORIZATION) {
        Some(v) => v,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "Missing Authorization header. Configure a JWT in the Fleet tab.",
                })),
            )
                .into_response();
        }
    };

    // Reject absurdly large header values before touching reqwest.
    if auth_header.as_bytes().len() > MAX_HEADER_VALUE_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Authorization header exceeds maximum accepted length",
            })),
        )
            .into_response();
    }

    // Validate backend_url: scheme http/https, no trailing path segments.
    let target_url = match validate_backend_url(&params.backend_url) {
        Ok(u) => u,
        Err(msg) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
        }
    };

    debug!("Web fleet proxy: GET {}", target_url);

    let client = &state.http_client;

    let outgoing = client
        .get(&target_url)
        .header(axum::http::header::AUTHORIZATION, auth_header)
        .header(axum::http::header::ACCEPT, "application/json")
        .timeout(std::time::Duration::from_secs(WEB_FLEET_TIMEOUT_SECS));

    match outgoing.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

            let resp_content_type = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            match resp.bytes().await {
                Ok(bytes) => {
                    let mut builder = Response::builder().status(status);
                    if let Some(ct) = resp_content_type {
                        builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
                    }
                    builder
                        .body(Body::from(bytes))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                Err(e) => {
                    warn!(
                        "Web fleet proxy: failed to read backend response body: {}",
                        e
                    );
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": format!("Failed to read backend response body: {e}"),
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            let (status, msg) = if e.is_timeout() {
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!(
                        "Web backend did not respond within {WEB_FLEET_TIMEOUT_SECS}s at {target_url}"
                    ),
                )
            } else if e.is_connect() {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Cannot connect to web backend at {target_url}: {e}"),
                )
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Web fleet request failed: {e}"),
                )
            };
            warn!("Web fleet proxy error: {}", msg);
            (status, Json(json!({ "error": msg }))).into_response()
        }
    }
}

/// Validate and normalize the user-supplied backend URL. Accepts any URL whose
/// scheme is `http` or `https` and whose path is empty or `/`; everything else
/// is rejected so the endpoint cannot be turned into a general-purpose proxy.
///
/// Returns the fully-qualified URL to hit on the web backend,
/// i.e. `{backend_url_trimmed}/api/v1/runners`.
fn validate_backend_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("backend_url is required".to_string());
    }

    let parsed =
        reqwest::Url::parse(trimmed).map_err(|e| format!("backend_url is not a valid URL: {e}"))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "backend_url scheme must be http or https (got `{scheme}`)"
        ));
    }

    // Reject anything beyond a bare origin — no path segments, no query, no
    // fragment. This prevents `?backend_url=https://evil/?a=b` from being
    // used to smuggle data through the supervisor.
    let path = parsed.path();
    if !(path.is_empty() || path == "/") {
        return Err(
            "backend_url must not include a path (got a non-root path segment)".to_string(),
        );
    }
    if parsed.query().is_some() {
        return Err("backend_url must not include a query string".to_string());
    }
    if parsed.fragment().is_some() {
        return Err("backend_url must not include a fragment".to_string());
    }

    // Build the final target by trimming any trailing slash and appending
    // the fleet listing path.
    let base = trimmed.trim_end_matches('/');
    Ok(format!("{base}/api/v1/runners"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_backend_url_accepts_https_origin() {
        let got = validate_backend_url("https://api.qontinui.io").unwrap();
        assert_eq!(got, "https://api.qontinui.io/api/v1/runners");
    }

    #[test]
    fn validate_backend_url_accepts_http_localhost_with_port() {
        let got = validate_backend_url("http://127.0.0.1:8000").unwrap();
        assert_eq!(got, "http://127.0.0.1:8000/api/v1/runners");
    }

    #[test]
    fn validate_backend_url_strips_trailing_slash() {
        let got = validate_backend_url("https://api.qontinui.io/").unwrap();
        assert_eq!(got, "https://api.qontinui.io/api/v1/runners");
    }

    #[test]
    fn validate_backend_url_rejects_non_http_scheme() {
        let err = validate_backend_url("ftp://api.qontinui.io").unwrap_err();
        assert!(err.contains("http or https"), "got: {err}");
    }

    #[test]
    fn validate_backend_url_rejects_path_segments() {
        let err = validate_backend_url("https://api.qontinui.io/api/v1").unwrap_err();
        assert!(err.contains("path"), "got: {err}");
    }

    #[test]
    fn validate_backend_url_rejects_query() {
        let err = validate_backend_url("https://api.qontinui.io?a=b").unwrap_err();
        assert!(err.contains("query"), "got: {err}");
    }

    #[test]
    fn validate_backend_url_rejects_fragment() {
        let err = validate_backend_url("https://api.qontinui.io#frag").unwrap_err();
        assert!(err.contains("fragment"), "got: {err}");
    }

    #[test]
    fn validate_backend_url_rejects_empty() {
        let err = validate_backend_url("   ").unwrap_err();
        assert!(err.contains("required"), "got: {err}");
    }

    #[test]
    fn validate_backend_url_rejects_garbage() {
        assert!(validate_backend_url("not-a-url").is_err());
    }
}

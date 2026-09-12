//! Web Fleet proxy route.
//!
//! Forwards `GET /web-fleet?backend_url=<url>` to `{backend_url}/api/v1/devices`
//! at the user-supplied qontinui-web backend, with the caller's `Authorization`
//! header attached verbatim.
//!
//! ## Why `/api/v1/devices` and not `/api/v1/runners`
//!
//! This proxy targeted `/api/v1/runners` until 2026-09-12 and had been dead for
//! months: qontinui-web deleted the legacy fleet endpoints in `ad3692e6c`
//! (2026-04-28) and removed the whole `/api/v1/runners` router in `1574bd036`
//! (2026-05-19, Phase 5 of `2026-05-18-unified-devices-registry`) — a rename
//! with NO deprecation alias. Every request this route made 404'd, so the Fleet
//! tab rendered an error for every operator who opened it.
//!
//! The replacement list route is `GET /api/v1/devices` (no trailing slash;
//! declared `@router.get("")` on the web side). Its `response_model` is the same
//! generated wire entity the retired route served — `qontinui-schemas`' `Runner`
//! — so the row shape is unchanged apart from an added `tenant_bindings`.
//!
//! ## Status codes a caller must NOT misread
//!
//! `GET /api/v1/devices` does not read qontinui-web's own database: it proxies
//! to coord (`GET /coord/devices/by-user`) on a 5s timeout and translates
//! failures. So on THIS route:
//!
//! - `502` means coord is unreachable, or returned a 5xx, NOT that the web
//!   backend is down.
//! - `504` means the web backend timed out waiting for coord.
//! - `503` means exactly one thing: the device row has a NULL `ws_session_id`,
//!   i.e. the runner is not currently connected.
//!
//! The supervisor forwards the upstream status verbatim, so the dashboard sees
//! these unchanged. Do not translate them into "backend down".
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
/// i.e. `{backend_url_trimmed}/api/v1/devices`.
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
    // the fleet listing path. NOTE: no trailing slash on `/devices` — the web
    // route is declared `@router.get("")`, and FastAPI would 307-redirect a
    // `/devices/` form, which reqwest follows but which drops the
    // `Authorization` header on a cross-origin hop.
    let base = trimmed.trim_end_matches('/');
    Ok(format!("{base}/api/v1/devices"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_backend_url_accepts_https_origin() {
        let got = validate_backend_url("https://api.qontinui.io").unwrap();
        assert_eq!(got, "https://api.qontinui.io/api/v1/devices");
    }

    #[test]
    fn validate_backend_url_accepts_http_localhost_with_port() {
        let got = validate_backend_url("http://127.0.0.1:8000").unwrap();
        assert_eq!(got, "http://127.0.0.1:8000/api/v1/devices");
    }

    #[test]
    fn validate_backend_url_strips_trailing_slash() {
        let got = validate_backend_url("https://api.qontinui.io/").unwrap();
        assert_eq!(got, "https://api.qontinui.io/api/v1/devices");
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

    /// Regression pin for the 2026-09-12 repoint. `/api/v1/runners` was deleted
    /// from qontinui-web in `1574bd036` with no alias, so a caller that drifts
    /// back to it 404s on every request with no local error — exactly the
    /// silent failure this route shipped with for months. Pin the live path and
    /// the absence of the dead one.
    #[test]
    fn target_is_the_devices_route_never_the_retired_runners_route() {
        let got = validate_backend_url("https://api.qontinui.io").unwrap();
        assert!(
            got.ends_with("/api/v1/devices"),
            "fleet listing must target the devices route, got: {got}"
        );
        assert!(
            !got.contains("/api/v1/runners"),
            "/api/v1/runners was deleted from qontinui-web (no alias); got: {got}"
        );
    }

    /// A trailing slash would make FastAPI 307 to the canonical form, and a
    /// redirect hop is where an `Authorization` header gets dropped.
    #[test]
    fn target_has_no_trailing_slash() {
        let got = validate_backend_url("http://127.0.0.1:8000").unwrap();
        assert!(!got.ends_with('/'), "got: {got}");
    }
}

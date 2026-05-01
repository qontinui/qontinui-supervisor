//! Debug-only HTTP endpoints, gated by
//! [`crate::state::DEBUG_ENDPOINTS_ENV`]
//! (`QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS=1`).
//!
//! These endpoints exist solely to let manual tests (and the
//! `/manual-test` slash command) exercise edge cases that would otherwise
//! require expensive setup or side effects on the supervisor process. They
//! are NOT intended to be used in production / shared deployments — when
//! the env gate is unset, every endpoint in this module returns 403 with
//! body `{"error": "debug_endpoints_disabled"}`.
//!
//! ## Endpoints
//!
//! | Method | Path | Purpose |
//! |--------|------|---------|
//! | POST   | `/control/dev/emit-build-id` | Inject a synthetic `buildId` into the live `/health/stream` SSE without rebuilding the supervisor. |
//!
//! ## Adding a new debug endpoint
//!
//! 1. Implement the handler in this file. The first thing the handler must
//!    do is call [`require_debug_enabled`] — never bypass the gate.
//! 2. Register the route in [`router`] below.
//! 3. Document it in `qontinui-supervisor/CLAUDE.md` under "Debug
//!    endpoints (gated)" so callers know it exists and how to enable it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::state::SharedState;

/// Build the dev-endpoint sub-router. Always merged into the main router —
/// the gate happens *inside* each handler so the env var can be flipped
/// without rebuilding the supervisor (the gate is read once at startup and
/// cached on `SupervisorState`, so this means the gate is set by restart
/// rather than per-request, which matches the rest of the supervisor's
/// process-startup-bound config).
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/control/dev/emit-build-id", post(emit_build_id))
        .with_state(state)
}

/// Body for `POST /control/dev/emit-build-id`. The `buildId` field is the
/// value that will be injected into the next SSE health event so connected
/// dashboards see a divergence from the `<meta name="build-id">` they
/// captured at mount.
#[derive(Deserialize)]
pub struct EmitBuildIdRequest {
    #[serde(rename = "buildId")]
    pub build_id: String,
}

/// `POST /control/dev/emit-build-id`
///
/// Sends `body.buildId` on `state.synthetic_build_id_tx`. The
/// `/health/stream` handler is subscribed to that channel — when it
/// receives a message, it emits a one-shot `event: health` whose JSON
/// payload is identical to a normal tick *except* its `buildId` field is
/// the supplied value. The on-disk `state.build_id` is NOT modified; this
/// is purely a transport-layer injection, so the next regular tick reverts
/// to the real build id.
///
/// **Gate.** When [`SupervisorState::debug_endpoints_enabled`] is false, the
/// handler returns `403 Forbidden` with body
/// `{"error": "debug_endpoints_disabled"}` *without* touching the channel.
///
/// **Best-effort.** If no SSE consumer is currently connected, the
/// broadcast send returns `Err(SendError(_))` and we still return 200 to
/// the caller — the manual test workflow may legitimately call this before
/// opening the dashboard. The response includes
/// `{"ok": true, "emitted": "<buildId>", "subscribers": <usize>}` so the
/// caller can tell if anyone was actually listening.
pub async fn emit_build_id(
    State(state): State<SharedState>,
    Json(body): Json<EmitBuildIdRequest>,
) -> impl IntoResponse {
    if let Err(resp) = require_debug_enabled(&state) {
        return resp.into_response();
    }

    let trimmed = body.build_id.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "build_id_empty"})),
        )
            .into_response();
    }
    // Cap the size so a misbehaving caller can't push a multi-megabyte
    // string through every connected SSE stream. 1 KiB is far more than any
    // real build id (RFC3339 timestamps are ~25 chars).
    if trimmed.len() > 1024 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "build_id_too_long", "max_bytes": 1024})),
        )
            .into_response();
    }

    let payload = trimmed.to_string();
    // `send` returns Err(SendError) only when there are zero receivers —
    // that's fine, we still report success and let the caller decide.
    let subscribers: usize = state
        .synthetic_build_id_tx
        .send(payload.clone())
        .unwrap_or_default();

    info!(
        target: "qontinui_supervisor::dev",
        build_id = %payload,
        subscribers,
        "synthetic build-id emitted via /control/dev/emit-build-id"
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "emitted": payload,
            "subscribers": subscribers,
        })),
    )
        .into_response()
}

/// Return `Ok(())` when the debug-endpoints gate is enabled, otherwise an
/// `IntoResponse` for `403 Forbidden` with the standard
/// `debug_endpoints_disabled` body. Handlers that join this module's router
/// must call this as their first step.
fn require_debug_enabled(state: &SharedState) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if state.debug_endpoints_enabled {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "debug_endpoints_disabled"})),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig, DEFAULT_SUPERVISOR_PORT};
    use crate::state::SupervisorState;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    fn make_test_config() -> SupervisorConfig {
        SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: DEFAULT_SUPERVISOR_PORT,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 8081,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
        }
    }

    /// Build a SupervisorState with the debug gate forcibly set to `enabled`.
    /// We bypass `read_debug_endpoints_env` here because tests share a
    /// process and races on `std::env::set_var` are unsound across threads
    /// on some platforms.
    fn state_with_debug(enabled: bool) -> SharedState {
        let mut s = SupervisorState::new(make_test_config());
        s.debug_endpoints_enabled = enabled;
        Arc::new(s)
    }

    async fn body_to_json(body: Body) -> serde_json::Value {
        // 1 MiB is plenty for these small JSON responses.
        let bytes = to_bytes(body, 1 << 20).await.expect("read body");
        serde_json::from_slice(&bytes).expect("body is JSON")
    }

    #[tokio::test]
    async fn emit_build_id_returns_200_when_debug_enabled() {
        let state = state_with_debug(true);
        let app = router(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/control/dev/emit-build-id")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"buildId":"synthetic-2026-05-01"}"#))
            .expect("build req");

        let response = app.oneshot(req).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::OK);

        let json = body_to_json(response.into_body()).await;
        assert_eq!(json["ok"], serde_json::Value::Bool(true));
        assert_eq!(json["emitted"], "synthetic-2026-05-01");
        // No SSE consumer is connected in this unit test, so subscribers == 0.
        assert_eq!(json["subscribers"], 0);
    }

    #[tokio::test]
    async fn emit_build_id_returns_403_when_debug_disabled() {
        let state = state_with_debug(false);
        let app = router(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/control/dev/emit-build-id")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"buildId":"synthetic-build"}"#))
            .expect("build req");

        let response = app.oneshot(req).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let json = body_to_json(response.into_body()).await;
        assert_eq!(json["error"], "debug_endpoints_disabled");
    }

    #[tokio::test]
    async fn emit_build_id_rejects_empty_string() {
        // Even with the gate open, an empty buildId is a 400. Protects the
        // SSE consumer from emitting a degenerate payload.
        let state = state_with_debug(true);
        let app = router(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/control/dev/emit-build-id")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"buildId":""}"#))
            .expect("build req");

        let response = app.oneshot(req).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let json = body_to_json(response.into_body()).await;
        assert_eq!(json["error"], "build_id_empty");
    }

    #[tokio::test]
    async fn emit_build_id_publishes_to_broadcast_channel() {
        // Subscribe BEFORE the request so the broadcast catches our receiver.
        // This is the path the SSE handler exercises in production.
        let state = state_with_debug(true);
        let mut rx = state.synthetic_build_id_tx.subscribe();
        let app = router(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/control/dev/emit-build-id")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"buildId":"channel-test-42"}"#))
            .expect("build req");

        let response = app.oneshot(req).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::OK);

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("recv must not time out")
            .expect("recv must yield Ok");
        assert_eq!(received, "channel-test-42");
    }
}

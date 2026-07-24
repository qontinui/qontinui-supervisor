//! Phase 2c — `POST /runners/pair-with-token` integration tests.
//!
//! Covers:
//!
//! 1. Request validation (empty token, malformed tenant UUID, missing
//!    tenant_id) returns HTTP 400 with the `validation_error` discriminator.
//! 2. Missing CLI binary returns HTTP 500 with `qontinui_profile_not_found`.
//! 3. CLI override pointing at a script that exits non-zero returns
//!    HTTP 502 with `pair_cli_failed` and forwards the captured stderr.
//! 4. On-disk artifact readers (`read_machine_device_id`,
//!    `read_paired_user`) round-trip correctly — exercised by the inline
//!    `#[cfg(test)] mod tests` in `runners_pair.rs` itself.
//! 5. `target_runner_id` validation: an unregistered id is a structured 404
//!    (`runner_not_found`); the primary and External (user-started) runners
//!    are each a 400 (neither reads a per-instance dir). Only Temp/Named
//!    runners are valid targets. Instance-dir resolution + read-back path
//!    selection are covered by the inline unit tests in `runners_pair.rs`.
//!
//! The "CLI succeeds + on-disk artifacts present" happy-path test would
//! require either modifying the production handler to inject the
//! `machine_json` / `paired_user.json` paths or shipping a host-stub
//! binary that writes those files at the system data_local_dir on test
//! machines. Neither is appealing — the host-data-dir contamination is
//! worse than partial coverage. The validation-and-failure surfaces are
//! the high-value test targets; the success path is covered by the
//! /manual-test-coord skill's first live run.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use tower::ServiceExt;

use qontinui_supervisor::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
use qontinui_supervisor::routes::runners_pair::pair_with_token;
use qontinui_supervisor::state::{SharedState, SupervisorState};

fn test_config() -> SupervisorConfig {
    // The project_dir path is required to exist for `runner_npm_dir` to
    // canonicalize successfully. Use the workspace's own root so any
    // file-resolution that walks up the tree still succeeds even though
    // we never actually shell out to a real qontinui_profile here.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    std::fs::create_dir_all(&manifest_dir).ok();
    SupervisorConfig {
        project_dir: manifest_dir.clone(),
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9875,
        dev_logs_dir: manifest_dir.join(".dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: BuildPoolConfig { pool_size: 1 },
        no_prewarm: true,
        no_webview: true,
    }
}

fn build_app() -> (Router, SharedState) {
    let state = Arc::new(SupervisorState::new(test_config()));
    let router = Router::new()
        .route("/runners/pair-with-token", post(pair_with_token))
        .with_state(state.clone());
    (router, state)
}

async fn post_json(app: Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/runners/pair-with-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("response body is valid JSON");
    (status, json)
}

#[tokio::test]
async fn rejects_empty_token() {
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation_error");
    assert!(body["message"].as_str().unwrap().contains("token"));
}

#[tokio::test]
async fn rejects_whitespace_only_token() {
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "   \n  ",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation_error");
}

#[tokio::test]
async fn rejects_malformed_tenant_uuid() {
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-bearer-token-1234",
            "tenant_id": "not-a-uuid",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation_error");
    assert!(body["message"].as_str().unwrap().contains("tenant_id"));
}

#[tokio::test]
async fn unknown_target_runner_id_is_a_structured_404() {
    // The registry only holds the default primary; an unknown id must be a
    // structured 404 BEFORE any CLI resolution/spawn happens (the fixture has
    // no qontinui_profile binary, so reaching the resolver would 500 instead).
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
            "target_runner_id": "test-9877",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "runner_not_found");
    assert!(body["message"].as_str().unwrap().contains("test-9877"));
}

#[tokio::test]
async fn primary_target_runner_id_is_rejected() {
    // The primary reads the SHARED secure-storage dir, not an instance dir —
    // targeting it would report success while the primary never sees the
    // credentials. Must be a 400, not a silent misdirected write.
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
            "target_runner_id": "primary",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation_error");
    assert!(body["message"].as_str().unwrap().contains("primary"));
}

#[tokio::test]
async fn external_target_runner_id_is_rejected() {
    // External (user-started) runners are launched outside supervisor control
    // and never receive the `QONTINUI_SECURE_STORAGE_DIR` export that
    // Temp/Named children get — so they read the shared dir, not an instance
    // dir. Targeting one would report success while the runner never sees the
    // credentials (the same divergence class as the primary). Must be a 400.
    let mut config = test_config();
    config.runners.push(RunnerConfig {
        id: "user-runner-1".to_string(),
        name: "User Runner".to_string(),
        port: 9890,
        kind: qontinui_types::wire::runner_kind::RunnerKind::External,
        ..RunnerConfig::default_primary()
    });
    let state = Arc::new(SupervisorState::new(config));
    let app = Router::new()
        .route("/runners/pair-with-token", post(pair_with_token))
        .with_state(state);
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
            "target_runner_id": "user-runner-1",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "validation_error");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("user-runner-1"), "message: {msg}");
    assert!(msg.contains("External"), "message: {msg}");
}

#[tokio::test]
async fn surfaces_qontinui_profile_not_found_when_no_runner_build() {
    // No `qontinui_profile` binary lives under our test fixture
    // project_dir, so `resolve_qontinui_profile_path` must return the
    // structured 500 error rather than silently spawning something.
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "qontinui_profile_not_found");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("qontinui_profile"));
}

#[tokio::test]
async fn override_path_must_exist() {
    // Pointing the override at a non-existent path must surface a clear
    // 500 — silent fallback to the resolver would defeat the purpose of
    // the override (tests want to know they're driving the stub).
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
            "qontinui_profile_path_override": "/nonexistent/qontinui_profile_stub",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "qontinui_profile_not_found");
    assert!(body["message"].as_str().unwrap().contains("override"));
}

#[tokio::test]
async fn cli_exit_nonzero_surfaces_as_pair_cli_failed() {
    // Use a built-in OS command that always exits non-zero so we
    // exercise the failure-path branch without needing to build a stub
    // binary. On Windows we use `cmd /c exit 2`; on Unix `/bin/false`.
    //
    // The handler treats this as a CLI invocation: it passes `device
    // pair --auth-token ... --tenant-id ...` as additional arguments,
    // which the stub command ignores. The stub exits non-zero, and we
    // assert the response carries the discriminator + exit_code + the
    // captured (empty) stderr.
    let stub_path = pick_failing_stub();
    let (app, _state) = build_app();
    let (status, body) = post_json(
        app,
        serde_json::json!({
            "token": "some-token",
            "tenant_id": "11111111-1111-4111-8111-111111111111",
            "qontinui_profile_path_override": stub_path.to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "pair_cli_failed");
    // exit_code must be present (Some(_), surfaced as a number); shape
    // doesn't pin a specific value because `cmd /c exit 2` vs
    // `/bin/false` differ.
    assert!(
        body["exit_code"].is_number(),
        "exit_code must be a number (got {:?})",
        body["exit_code"]
    );
    // stdout/stderr must always be strings (possibly empty) so callers
    // can render them without extra null-handling.
    assert!(body["stdout"].is_string(), "stdout must be a string");
    assert!(body["stderr"].is_string(), "stderr must be a string");
}

#[cfg(windows)]
fn pick_failing_stub() -> PathBuf {
    // On Windows there's no `/bin/false`. Use a small .bat file written to
    // a tempdir that exits with code 2 regardless of args. We can't use
    // `cmd /c exit 2` directly as the override because tokio's Command
    // expects a binary, not a shell built-in (it would try to exec
    // "/bin/cmd" or similar).
    let tmp = std::env::temp_dir().join("qontinui_supervisor_failing_stub.cmd");
    std::fs::write(&tmp, b"@exit /b 2\r\n").expect("write stub");
    tmp
}

#[cfg(not(windows))]
fn pick_failing_stub() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join("qontinui_supervisor_failing_stub.sh");
    std::fs::write(&tmp, b"#!/bin/sh\nexit 2\n").expect("write stub");
    let mut perms = std::fs::metadata(&tmp).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms).expect("set perms");
    tmp
}

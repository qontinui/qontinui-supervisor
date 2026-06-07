//! Integration tests for the build-artifact footprint surface, disk guard,
//! and prune endpoints (plan `2026-06-05-supervisor-build-artifact-footprint`).

use std::sync::{Arc, OnceLock};

use qontinui_supervisor::build_monitor::{check_disk_guard, disk_guard_allows};
use qontinui_supervisor::config::{RunnerConfig, SupervisorConfig};
use qontinui_supervisor::error::SupervisorError;
use qontinui_supervisor::state::SupervisorState;

/// Serializes the tests that mutate the process-global
/// `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB` env var. Without this, the parallel
/// test runner races set/remove across tests and one observes the other's
/// value (or its removal), producing a flaky wrong-disk-threshold verdict. A
/// tokio mutex (not `std::sync`) so it can be held across the `.await` on
/// `check_disk_guard` without tripping `clippy::await_holding_lock`.
fn min_free_disk_env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Build a SupervisorConfig whose project_dir lives under `base/qontinui-runner/
/// src-tauri`, so `runner_npm_dir()` resolves to `base/qontinui-runner` and the
/// pool/lkg roots are inside the tempdir.
fn config_under(base: &std::path::Path) -> SupervisorConfig {
    let src_tauri = base.join("qontinui-runner").join("src-tauri");
    std::fs::create_dir_all(&src_tauri).unwrap();
    SupervisorConfig {
        project_dir: src_tauri,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9899,
        dev_logs_dir: base.join(".dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: qontinui_supervisor::config::BuildPoolConfig { pool_size: 2 },
        no_prewarm: true,
        no_webview: true,
    }
}

#[test]
fn disk_guard_pure_threshold() {
    let gb = 1024u64 * 1024 * 1024;
    // 0 disables the guard regardless of free space.
    assert!(disk_guard_allows(Some(0), 0));
    // None (probe failed) fails open.
    assert!(disk_guard_allows(None, 30));
    // Below threshold → blocked.
    assert!(!disk_guard_allows(Some(10 * gb), 30));
    // At/above threshold → allowed.
    assert!(disk_guard_allows(Some(30 * gb), 30));
    assert!(disk_guard_allows(Some(100 * gb), 30));
}

#[tokio::test]
async fn footprint_cache_starts_empty_then_refreshes() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));

    // Empty until first refresh.
    assert!(state.footprint.read().await.is_none());

    // Seed a slot with a known number of bytes.
    let slot0 = state.config.runner_slot_target_dir(0).join("debug");
    std::fs::create_dir_all(&slot0).unwrap();
    std::fs::write(slot0.join("junk.bin"), vec![0u8; 4096]).unwrap();

    let snap = state.refresh_footprint().await;
    // Cache is now populated and matches the returned snapshot.
    let cached = state.footprint.read().await.clone().unwrap();
    assert_eq!(cached.computed_at, snap.computed_at);
    assert_eq!(snap.slots.len(), 2);
    let slot0_bytes = snap.slots.iter().find(|s| s.id == 0).unwrap().bytes;
    assert!(
        slot0_bytes >= 4096,
        "slot-0 footprint must include the seeded junk (got {})",
        slot0_bytes
    );
}

#[tokio::test]
async fn disk_guard_refuses_with_structured_footprint() {
    // Force the guard to fire by setting an absurd minimum.
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));
    // Populate the footprint cache so the refusal embeds it.
    let _ = state.refresh_footprint().await;

    // Scope the env var so it can't bleed into parallel tests.
    let _env = min_free_disk_env_lock().lock().await;
    std::env::set_var("QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB", "999999999");
    let result = check_disk_guard(&state).await;
    std::env::remove_var("QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB");
    drop(_env);

    let err = result.expect_err("guard must refuse at an absurd minimum");
    match &err {
        SupervisorError::InsufficientDisk {
            required_bytes,
            footprint,
            ..
        } => {
            assert!(*required_bytes > 0);
            // The cached snapshot must be embedded (not None — we refreshed).
            assert!(footprint.is_some(), "refusal must embed the footprint");
        }
        other => panic!("expected InsufficientDisk, got {other:?}"),
    }

    // The wire shape names both prune endpoints.
    let (status, body) = err.to_status_body();
    assert_eq!(status.as_u16(), 507);
    assert_eq!(body["error"], "insufficient_disk");
    let endpoints = body["prune_endpoints"].as_array().unwrap();
    let joined = endpoints
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(joined.contains("/spawn-worktrees"));
    assert!(joined.contains("/builds/slots/"));
}

#[tokio::test]
async fn builds_endpoint_includes_footprint_after_refresh() {
    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));

    let app = Router::new()
        .route(
            "/builds",
            get(qontinui_supervisor::routes::runners::list_builds),
        )
        .with_state(state.clone());

    // `?refresh_footprint=1` forces a synchronous walk so `footprint` is
    // present (and not null) even on the very first call.
    let req = Request::builder()
        .uri("/builds?refresh_footprint=1")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let fp = &v["footprint"];
    assert!(fp.is_object(), "footprint must be present after refresh");
    assert!(fp["computed_at"].is_string());
    assert!(fp["slots"].is_array());
    assert_eq!(fp["slots"].as_array().unwrap().len(), 2);
    assert!(fp.get("lkg_bytes").is_some());
    assert!(fp.get("spawn_containers").is_some());
    assert!(fp.get("exe_copies").is_some());
}

#[tokio::test]
async fn clean_slot_refuses_active_build() {
    use qontinui_supervisor::state::BuildInfo;

    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));

    // Mark slot 0 busy with an active build.
    {
        let slot = &state.build_pool.slots[0];
        let mut busy = slot.busy.write().await;
        *busy = Some(BuildInfo {
            started_at: chrono::Utc::now(),
            requester_id: Some("test".to_string()),
            rebuild_kind: "exe".to_string(),
        });
    }

    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    let app = Router::new()
        .route(
            "/builds/slots/{id}/clean",
            post(qontinui_supervisor::routes::runners::clean_slot_endpoint),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/builds/slots/0/clean")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "an active build must yield a 409 refusal"
    );
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"], "slot_busy");
}

#[tokio::test]
async fn clean_slot_empties_idle_slot_and_reports_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));

    // Seed slot 1 with junk; slot exe absent so no holder refusal.
    let slot1_debug = state.config.runner_slot_target_dir(1).join("debug");
    std::fs::create_dir_all(&slot1_debug).unwrap();
    std::fs::write(slot1_debug.join("a.bin"), vec![0u8; 8192]).unwrap();

    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    let app = Router::new()
        .route(
            "/builds/slots/{id}/clean",
            post(qontinui_supervisor::routes::runners::clean_slot_endpoint),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/builds/slots/1/clean")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v["bytes_freed"].as_u64().unwrap() >= 8192,
        "bytes_freed must cover the seeded junk, got {}",
        v["bytes_freed"]
    );
    // Dir is empty afterward.
    assert_eq!(
        qontinui_supervisor::footprint::dir_size_bytes(&state.config.runner_slot_target_dir(1)),
        0
    );
}

#[tokio::test]
async fn disk_guard_zero_disables() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(SupervisorState::new(config_under(tmp.path())));
    let _env = min_free_disk_env_lock().lock().await;
    std::env::set_var("QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB", "0");
    let result = check_disk_guard(&state).await;
    std::env::remove_var("QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB");
    drop(_env);
    assert!(result.is_ok(), "min=0 must disable the guard");
}

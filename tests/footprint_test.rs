//! Integration tests for the build-artifact footprint surface, disk guard,
//! and prune endpoints (plan `2026-06-05-supervisor-build-artifact-footprint`).

use std::sync::{Arc, OnceLock};

use qontinui_supervisor::build_monitor::{
    available_commit_bytes, available_phys_bytes, check_disk_guard, disk_guard_allows,
    format_pool_reading, ram_guard_allows, total_phys_bytes,
};
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

/// The commit arm alone, with the physical arm disabled (`0`) so it cannot
/// contribute. This is the pre-2026-08-29 contract, unchanged.
#[test]
fn ram_guard_pure_threshold() {
    let gb = 1024u64 * 1024 * 1024;
    // 0 disables the guard regardless of free commit.
    assert!(ram_guard_allows(Some(0), 0, None, 0));
    // None (probe failed) fails open — a telemetry gap must never brick the
    // build lane, same contract as the disk guard.
    assert!(ram_guard_allows(None, 5, None, 0));
    // Below the floor → defer. 3 GB free is roughly where the 2026-07-30
    // rustc abort (0xc0000409) happened on the MSI box.
    assert!(!ram_guard_allows(Some(3 * gb), 5, None, 0));
    // At/above the floor → build.
    assert!(ram_guard_allows(Some(5 * gb), 5, None, 0));
    assert!(ram_guard_allows(Some(32 * gb), 5, None, 0));
}

/// The two arms are INDEPENDENT: each floor is checked against its own
/// reading, and the guard allows only when both agree.
///
/// The row that matters most is the last one — the 2026-08-29 measurement:
/// 30 GB free commit (6x the 5 GB floor, so the commit arm passes instantly)
/// with 629 MB free physical. A commit-only guard let that build through and
/// it died twice.
#[test]
fn ram_guard_checks_commit_and_physical_independently() {
    let gb = 1024u64 * 1024 * 1024;
    let mb = 1024u64 * 1024;

    // Both healthy → build.
    assert!(ram_guard_allows(Some(30 * gb), 5, Some(20 * gb), 3));
    // Commit short, physical fine → defer.
    assert!(!ram_guard_allows(Some(2 * gb), 5, Some(20 * gb), 3));
    // Physical short, commit fine → defer. THE DEFECT: this is the case every
    // memory floor in this fleet used to pass.
    assert!(!ram_guard_allows(Some(30 * gb), 5, Some(629 * mb), 3));
    // Both short → defer.
    assert!(!ram_guard_allows(Some(2 * gb), 5, Some(629 * mb), 3));
    // Exactly at each floor → build (>=, not >).
    assert!(ram_guard_allows(Some(5 * gb), 5, Some(3 * gb), 3));
    // One byte under the physical floor → defer.
    assert!(!ram_guard_allows(Some(5 * gb), 5, Some(3 * gb - 1), 3));
}

/// The full `Option` matrix. **`None` must never read as `0`** — an absent
/// reading contributes nothing to the verdict, in either direction. If it were
/// folded into a number, a box whose physical probe went dark would defer
/// every build for the whole wait window and then build anyway.
#[test]
fn ram_guard_fails_open_per_arm_and_never_reads_none_as_zero() {
    let gb = 1024u64 * 1024 * 1024;

    // Neither sensor readable → allowed. Not "0 bytes free"; no reading.
    assert!(ram_guard_allows(None, 5, None, 3));
    // Commit readable and short, physical dark → the commit arm still holds.
    assert!(!ram_guard_allows(Some(gb), 5, None, 3));
    // Commit readable and healthy, physical dark → EXACTLY today's behaviour on
    // a box that can read commit but not physical (every non-Windows host).
    assert!(ram_guard_allows(Some(30 * gb), 5, None, 3));
    // Physical readable and short, commit dark → the physical arm alone holds.
    assert!(!ram_guard_allows(None, 5, Some(gb), 3));
    // Physical readable and healthy, commit dark → allowed.
    assert!(ram_guard_allows(None, 5, Some(30 * gb), 3));

    // A genuine Some(0) is NOT the same as None: it is a real extreme-pressure
    // reading and must defer. This is the pair that proves the distinction is
    // carried through rather than collapsed.
    assert!(!ram_guard_allows(Some(0), 5, None, 3));
    assert!(!ram_guard_allows(None, 5, Some(0), 3));
    assert!(ram_guard_allows(None, 5, None, 3));
}

/// Each `0` disables ONLY its own arm; both `0` disables the guard entirely.
#[test]
fn ram_guard_zero_floors_disable_one_arm_each() {
    let gb = 1024u64 * 1024 * 1024;

    // Physical floor 0: a starved physical pool cannot hold the build.
    assert!(ram_guard_allows(Some(30 * gb), 5, Some(0), 0));
    // ...but the commit floor is untouched by that 0.
    assert!(!ram_guard_allows(Some(gb), 5, Some(0), 0));

    // Commit floor 0: a starved commit pool cannot hold the build.
    assert!(ram_guard_allows(Some(0), 0, Some(30 * gb), 3));
    // ...but the physical floor is untouched by that 0.
    assert!(!ram_guard_allows(Some(0), 0, Some(gb), 3));

    // Both 0 → fully disabled, whatever the readings say.
    assert!(ram_guard_allows(Some(0), 0, Some(0), 0));
    assert!(ram_guard_allows(None, 0, None, 0));
}

/// A reading is always rendered against its own total. A bare "1.05 GB" is
/// unreadable; "1.05 of 31.71 GB" is the whole message.
#[test]
fn pool_readings_render_against_their_own_total() {
    let gb = 1024u64 * 1024 * 1024;

    assert_eq!(
        format_pool_reading("physical", Some(gb / 2), Some(32 * gb)),
        "physical 0.50 of 32.00 GB"
    );
    // An unknown total is SAID, not defaulted to something that reads as fact.
    assert_eq!(
        format_pool_reading("commit", Some(5 * gb), None),
        "commit 5.00 GB of an unknown total"
    );
    // An unknown reading is "unavailable", never a zero.
    assert_eq!(
        format_pool_reading("physical", None, Some(32 * gb)),
        "physical unavailable"
    );
    assert_eq!(
        format_pool_reading("physical", None, None),
        "physical unavailable"
    );
}

/// The physical probe is Windows-only by design; off Windows it is inert
/// because `available_commit_bytes` already reads physical-available there.
#[test]
fn phys_probe_answers_on_windows_and_is_inert_elsewhere() {
    let avail = available_phys_bytes();
    assert_eq!(
        avail.is_some(),
        total_phys_bytes().is_some(),
        "the physical reading and its ceiling must come from the same probe"
    );
    if cfg!(windows) {
        assert!(avail.is_some(), "physical probe returned None on Windows");
        assert!(
            avail.unwrap() > 64 * 1024 * 1024,
            "implausible free physical: {:?}",
            avail
        );
    } else {
        assert!(
            avail.is_none(),
            "off Windows the physical arm must stay inert, not double-count MemAvailable"
        );
    }
}

#[test]
fn ram_probe_reports_a_plausible_value() {
    // The probe must return Some on a supported platform (it is the input the
    // guard fails OPEN on, so a silently-None probe would disable the guard on
    // exactly the box it exists to protect — the `free_mem_gb` MSYS trap that
    // cargo-guard.sh documents).
    let free = available_commit_bytes();
    assert!(free.is_some(), "memory probe returned None");
    // Sanity: a machine that can build this workspace has more than 64 MB of
    // commit available; anything less means we read the wrong field.
    assert!(
        free.unwrap() > 64 * 1024 * 1024,
        "implausible free commit: {:?}",
        free
    );
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

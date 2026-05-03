//! Integration test for the parallel build pool mechanics.
//!
//! Validates semaphore permits, slot claiming, history recording, and RAII
//! guard behaviour WITHOUT running real cargo builds.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Semaphore;

use qontinui_supervisor::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
use qontinui_supervisor::state::{BuildInfo, SupervisorState};

fn pool_test_config(pool_size: usize) -> SupervisorConfig {
    SupervisorConfig {
        project_dir: PathBuf::from("/tmp/test/src-tauri"),
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9875,
        dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: BuildPoolConfig { pool_size },
        no_prewarm: false,
        no_webview: true,
    }
}

fn fake_build_info(tag: &str) -> BuildInfo {
    BuildInfo {
        started_at: Utc::now(),
        requester_id: Some(tag.to_string()),
        rebuild_kind: "dev".to_string(),
    }
}

/// Three concurrent tasks each acquire a permit, claim a unique slot, record
/// history, and release. After all complete the pool should be fully free.
#[tokio::test]
async fn test_concurrent_slot_acquisition() {
    let state = Arc::new(SupervisorState::new(pool_test_config(3)));

    let mut handles = Vec::new();
    for i in 0..3 {
        let st = state.clone();
        handles.push(tokio::spawn(async move {
            // 1. Acquire permit
            let permit = st
                .build_pool
                .permits
                .clone()
                .acquire_owned()
                .await
                .expect("permit acquire failed");

            // 2. Claim idle slot
            let slot = st
                .build_pool
                .claim_idle_slot(fake_build_info(&format!("task-{i}")))
                .await;

            let slot_id = slot.id;

            // 3. Simulate build
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            // 4. Record history
            {
                let mut h = slot.history.write().await;
                h.record(0.1, true, None, None);
            }

            // 5. Mark slot as not busy and update last_successful_slot
            {
                *slot.busy.write().await = None;
            }
            {
                *st.build_pool.last_successful_slot.write().await = Some(slot_id);
            }

            // 6. Drop permit
            drop(permit);

            slot_id
        }));
    }

    // Collect results
    let mut slot_ids = HashSet::new();
    for h in handles {
        let id = h.await.expect("task panicked");
        slot_ids.insert(id);
    }

    // All 3 slot ids must be unique
    assert_eq!(
        slot_ids.len(),
        3,
        "expected 3 unique slot ids, got {slot_ids:?}"
    );

    // All permits free
    assert_eq!(state.build_pool.permits.available_permits(), 3);

    // Each slot has exactly 1 build in history
    for slot in &state.build_pool.slots {
        let h = slot.history.read().await;
        assert_eq!(h.total_builds, 1, "slot {} should have 1 build", slot.id);
    }

    // last_successful_slot is set
    assert!(state.build_pool.last_successful_slot.read().await.is_some());

    // 4th acquire should succeed immediately (all free)
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(10),
        state.build_pool.permits.clone().acquire_owned(),
    )
    .await;
    assert!(result.is_ok(), "4th permit should be immediately available");
    // Drop so we don't leak
    drop(result.unwrap());
}

/// When all permits are held, try_acquire should fail. After release all
/// should be available again.
#[tokio::test]
async fn test_no_wait_exhaustion() {
    let state = Arc::new(SupervisorState::new(pool_test_config(3)));

    // Hold all 3 permits
    let p1 = state
        .build_pool
        .permits
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let p2 = state
        .build_pool
        .permits
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let p3 = state
        .build_pool
        .permits
        .clone()
        .acquire_owned()
        .await
        .unwrap();

    // Non-blocking try should fail
    let try_result = Semaphore::try_acquire_owned(state.build_pool.permits.clone());
    assert!(
        try_result.is_err(),
        "try_acquire should fail when exhausted"
    );

    assert_eq!(state.build_pool.permits.available_permits(), 0);

    // Release all
    drop(p1);
    drop(p2);
    drop(p3);

    assert_eq!(state.build_pool.permits.available_permits(), 3);
}

/// Claiming a slot and then clearing its `busy` field simulates RAII-like
/// cleanup. After clearing, the slot should be idle again.
#[tokio::test]
async fn test_slot_busy_cleanup() {
    let state = Arc::new(SupervisorState::new(pool_test_config(3)));

    let _permit = state
        .build_pool
        .permits
        .clone()
        .acquire_owned()
        .await
        .unwrap();

    let slot = state
        .build_pool
        .claim_idle_slot(fake_build_info("guard-test"))
        .await;

    // Slot should be busy
    assert!(slot.busy.read().await.is_some());

    // Simulate guard drop — clear busy
    *slot.busy.write().await = None;

    // Slot should now be idle
    assert!(slot.busy.read().await.is_none());
}

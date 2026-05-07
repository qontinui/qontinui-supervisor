//! HTTP-handler-level integration tests for `GET /lkg/coverage`.
//!
//! The route's pure logic (path parsing, resolution, mtime → coverage decision,
//! response shape) is unit-tested exhaustively in
//! `src/routes/lkg_coverage.rs::tests`. What's NOT covered there is the
//! handler itself — specifically the `state.build_pool.last_known_good` lock
//! read, the `state.config.project_dir` read, and the
//! `{success, data, timestamp}` supervisor-bridge response envelope. These
//! tests close that gap by constructing a real `SupervisorState`, seeding the
//! LKG (or leaving it empty), and invoking the axum handler directly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::{RawQuery, State};
use chrono::{DateTime, Utc};
use tempfile::TempDir;

use qontinui_supervisor::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
use qontinui_supervisor::routes::lkg_coverage::lkg_coverage;
use qontinui_supervisor::state::{LkgInfo, SharedState, SupervisorState};

/// Build a minimal `SupervisorConfig` whose `project_dir` is `dir`. Mirrors
/// the helpers in `tests/state_test.rs` and `tests/build_pool_integration.rs`,
/// but parameterized on the project_dir so tests can drop real files into it.
fn config_with_project_dir(dir: PathBuf) -> SupervisorConfig {
    SupervisorConfig {
        project_dir: dir,
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
        build_pool: BuildPoolConfig { pool_size: 1 },
        no_prewarm: false,
        no_webview: true,
    }
}

/// Build a `SharedState` rooted at `project_dir`, optionally seeding the LKG.
async fn state_for(project_dir: PathBuf, lkg_at: Option<DateTime<Utc>>) -> SharedState {
    let state = Arc::new(SupervisorState::new(config_with_project_dir(project_dir)));
    if let Some(built_at) = lkg_at {
        *state.build_pool.last_known_good.write().await = Some(LkgInfo {
            built_at,
            source_slot: 0,
            exe_size: 0,
        });
    }
    state
}

/// Force a file's mtime to a known wall-clock value.
fn touch(path: &std::path::Path, when: SystemTime) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open file");
    f.set_modified(when).expect("set_modified");
}

/// Drive the handler and unwrap the JSON to a `serde_json::Value` for
/// inspection. The handler returns `Json<serde_json::Value>`; `.0` peels off
/// the axum wrapper.
async fn call(state: SharedState, query: Option<&str>) -> serde_json::Value {
    let raw = RawQuery(query.map(str::to_string));
    let response = lkg_coverage(State(state), raw).await;
    response.0
}

// -----------------------------------------------------------------------
// Envelope shape — `{success, data, timestamp}` is the supervisor-bridge
// contract. Every endpoint follows this; a regression here would silently
// break dashboard and agent consumers.
// -----------------------------------------------------------------------

#[tokio::test]
async fn envelope_shape_is_success_data_timestamp() {
    let dir = TempDir::new().expect("tempdir");
    let state = state_for(dir.path().to_path_buf(), None).await;

    let body = call(state, None).await;

    assert_eq!(body["success"], true, "envelope.success must be true");
    assert!(body["data"].is_object(), "envelope.data must be an object");
    assert!(
        body["timestamp"].is_i64(),
        "envelope.timestamp must be an i64 epoch-millis: {body}"
    );
}

#[tokio::test]
async fn data_block_carries_files_and_all_covered_keys() {
    // Even with an empty request, the `data` block must always carry the
    // documented keys so JSON consumers don't have to branch on presence.
    let dir = TempDir::new().expect("tempdir");
    let state = state_for(dir.path().to_path_buf(), None).await;

    let body = call(state, None).await;
    let data = &body["data"];

    assert!(data.get("lkg_built_at").is_some(), "missing lkg_built_at");
    assert!(data.get("files").is_some(), "missing files");
    assert!(data.get("all_covered").is_some(), "missing all_covered");
    assert!(data["files"].is_array(), "files must be an array");
    assert_eq!(
        data["all_covered"], false,
        "vacuous request must not be reported as all_covered"
    );
}

// -----------------------------------------------------------------------
// State integration — handler must read both `state.config.project_dir`
// (for relative-path resolution) and `state.build_pool.last_known_good`
// (the lock read in the handler itself).
// -----------------------------------------------------------------------

#[tokio::test]
async fn handler_reads_project_dir_for_relative_paths() {
    // A file dropped into `project_dir` is resolved correctly when the
    // request submits the relative name. Without the handler reading
    // `state.config.project_dir`, this would either 404 or escape the
    // tempdir.
    let dir = TempDir::new().expect("tempdir");
    let project = dir.path();
    let file = project.join("hello.rs");
    std::fs::write(&file, "fn main() {}").expect("write");
    touch(&file, SystemTime::UNIX_EPOCH + Duration::from_secs(100));

    let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(300));
    let state = state_for(project.to_path_buf(), Some(lkg_at)).await;

    let body = call(state, Some("path=hello.rs")).await;

    assert_eq!(body["data"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["data"]["files"][0]["path"], "hello.rs",
        "submitted path is echoed back verbatim"
    );
    assert_eq!(body["data"]["files"][0]["exists"], true);
    assert_eq!(body["data"]["files"][0]["covered"], true);
    assert_eq!(body["data"]["all_covered"], true);
}

#[tokio::test]
async fn handler_reads_lkg_lock_when_seeded() {
    // The handler does an `await`-ed read on the LKG `RwLock`. This test
    // asserts the lock value flows through end-to-end as RFC3339.
    let dir = TempDir::new().expect("tempdir");
    let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(300));
    let state = state_for(dir.path().to_path_buf(), Some(lkg_at)).await;

    let body = call(state, None).await;

    let serialized = body["data"]["lkg_built_at"]
        .as_str()
        .expect("lkg_built_at must serialize as string when LKG present");
    let parsed: DateTime<Utc> = DateTime::parse_from_rfc3339(serialized)
        .expect("RFC3339")
        .into();
    assert_eq!(parsed, lkg_at, "lock value round-trips through the handler");
}

#[tokio::test]
async fn handler_reads_lkg_lock_when_empty() {
    // No LKG seeded → handler reads `None` from the lock → JSON `null`.
    // Counterpart to the seeded test; without both, you can't tell whether
    // the lock-read path is exercised at all.
    let dir = TempDir::new().expect("tempdir");
    let state = state_for(dir.path().to_path_buf(), None).await;

    let body = call(state, None).await;

    assert!(
        body["data"]["lkg_built_at"].is_null(),
        "no-LKG case must serialize as JSON null, got {}",
        body["data"]["lkg_built_at"]
    );
}

// -----------------------------------------------------------------------
// Query-string handling — the handler accepts `RawQuery`, not
// `Query<HashMap>`. Multi-`path=` and percent-decoding ride through the
// full handler path; pure-function unit tests can't catch a regression in
// how axum surfaces `RawQuery`.
// -----------------------------------------------------------------------

#[tokio::test]
async fn multiple_path_params_processed_in_order() {
    let dir = TempDir::new().expect("tempdir");
    let project = dir.path();

    let old = project.join("old.rs");
    std::fs::write(&old, "x").unwrap();
    touch(&old, SystemTime::UNIX_EPOCH + Duration::from_secs(100));

    let new = project.join("new.rs");
    std::fs::write(&new, "x").unwrap();
    touch(&new, SystemTime::UNIX_EPOCH + Duration::from_secs(500));

    let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(300));
    let state = state_for(project.to_path_buf(), Some(lkg_at)).await;

    let body = call(state, Some("path=old.rs&path=new.rs&path=missing.rs")).await;

    let files = body["data"]["files"].as_array().expect("files array");
    assert_eq!(files.len(), 3, "three paths in, three results out");
    assert_eq!(files[0]["path"], "old.rs");
    assert_eq!(files[0]["covered"], true);
    assert_eq!(files[1]["path"], "new.rs");
    assert_eq!(files[1]["covered"], false);
    assert_eq!(files[2]["path"], "missing.rs");
    assert_eq!(files[2]["exists"], false);
    assert_eq!(
        body["data"]["all_covered"], false,
        "any uncovered file ⇒ all_covered: false"
    );
}

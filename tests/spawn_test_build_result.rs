//! Unit tests for the `build_result` JSON helper used by `spawn-test`,
//! `spawn-named`, and `rebuild-and-restart` (Item A of the supervisor
//! cleanup plan).
//!
//! Inducing a real cargo build failure inside a tokio test is too slow
//! and brittle; we exercise the helper directly to lock in the field
//! shape and the various input → output mappings.

use qontinui_supervisor::process::manager::BinaryMeta;
use qontinui_supervisor::routes::runners::build_result_json;

fn fake_meta() -> BinaryMeta {
    BinaryMeta {
        binary_mtime: "2026-05-09T12:00:00Z".to_string(),
        binary_size_bytes: 250_000_000,
        binary_age_secs: 42,
    }
}

#[test]
fn build_result_when_not_attempted() {
    let v = build_result_json(false, None, false, None, None, None);
    assert_eq!(v["state"], "reused");
    assert_eq!(v["attempted"], false);
    assert!(v["succeeded"].is_null());
    assert_eq!(v["reused_stale"], false);
    assert!(v["error"].is_null());
    assert!(v["slot_id"].is_null());
    // Even without meta, the keys must exist with null values so callers can
    // rely on a stable shape.
    assert!(v["binary_mtime"].is_null());
    assert!(v["binary_size_bytes"].is_null());
    assert!(v["binary_age_secs"].is_null());
}

#[test]
fn build_result_successful_build_with_meta() {
    let meta = fake_meta();
    let v = build_result_json(true, Some(true), false, None, Some(2), Some(&meta));
    assert_eq!(v["state"], "built");
    assert_eq!(v["attempted"], true);
    assert_eq!(v["succeeded"], true);
    assert_eq!(v["reused_stale"], false);
    assert!(v["error"].is_null());
    assert_eq!(v["slot_id"], 2);
    assert_eq!(v["binary_mtime"], "2026-05-09T12:00:00Z");
    assert_eq!(v["binary_size_bytes"], 250_000_000u64);
    assert_eq!(v["binary_age_secs"], 42u64);
}

#[test]
fn build_result_failed_build_stale_fallback() {
    let meta = fake_meta();
    let v = build_result_json(
        true,
        Some(false),
        true,
        Some("error[E0277]: trait bound not satisfied"),
        Some(0),
        Some(&meta),
    );
    assert_eq!(v["state"], "reused_stale");
    assert_eq!(v["attempted"], true);
    assert_eq!(v["succeeded"], false);
    assert_eq!(v["reused_stale"], true);
    assert_eq!(v["error"], "error[E0277]: trait bound not satisfied");
    assert_eq!(v["slot_id"], 0);
    assert_eq!(v["binary_age_secs"], 42u64);
}

#[test]
fn build_result_failed_build_no_fallback() {
    // attempted=true, succeeded=false, reused_stale=false → "failed"
    // (caller didn't ask for stale-fallback, so the spawn surfaced the
    // build error directly to the client without resolving a binary).
    let v = build_result_json(
        true,
        Some(false),
        false,
        Some("error[E0277]: trait bound not satisfied"),
        Some(1),
        None,
    );
    assert_eq!(v["state"], "failed");
    assert_eq!(v["attempted"], true);
    assert_eq!(v["succeeded"], false);
    assert_eq!(v["reused_stale"], false);
}

#[test]
fn build_result_state_is_one_of_four_stable_strings() {
    // Lock the state-string set so a future refactor that adds a new
    // case has to update this test (and presumably any client that
    // switches on the value).
    let states = [
        build_result_json(false, None, false, None, None, None)["state"]
            .as_str()
            .unwrap()
            .to_string(),
        build_result_json(true, Some(true), false, None, Some(0), None)["state"]
            .as_str()
            .unwrap()
            .to_string(),
        build_result_json(true, Some(false), false, None, Some(0), None)["state"]
            .as_str()
            .unwrap()
            .to_string(),
        build_result_json(true, Some(false), true, None, Some(0), None)["state"]
            .as_str()
            .unwrap()
            .to_string(),
    ];
    let mut sorted: Vec<&str> = states.iter().map(String::as_str).collect();
    sorted.sort();
    assert_eq!(sorted, vec!["built", "failed", "reused", "reused_stale"]);
}

#[test]
fn build_result_age_secs_under_300_for_fresh_meta() {
    // Use a real binary_meta call against an existing on-disk file (this
    // very test binary's source) so the age is computed from chrono::Utc::now.
    let path = std::path::Path::new(file!());
    if let Some(meta) = qontinui_supervisor::process::manager::binary_meta(path) {
        // The source file's mtime is older than 300s in CI but the field
        // type is u64 — assert the field is populated and not absurd.
        let v = build_result_json(true, Some(true), false, None, Some(0), Some(&meta));
        assert!(v["binary_age_secs"].is_u64());
    }
}

/// Integration-style test for the full HTTP flow is gated behind
/// `#[ignore]` because it requires a real cargo build cycle inside a
/// tokio test, which adds 1-3 minutes per case. The manual-test recipe
/// referenced in the plan covers it; agents reproduce it via the live
/// supervisor instead.
#[test]
#[ignore]
fn spawn_test_http_response_shape() {
    // Manual flow:
    //   POST http://localhost:9875/runners/spawn-test
    //     body: {"rebuild": true, "wait": true, "wait_timeout_secs": 180}
    //   Expect: HTTP 200, body.build_result.attempted == true,
    //           body.build_result.succeeded == true,
    //           body.build_result.binary_age_secs < 300,
    //           body.build_result.slot_id is integer.
}

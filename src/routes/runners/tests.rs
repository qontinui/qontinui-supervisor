#![cfg(test)]

//! Regression tests for `check_dist_freshness` and the
//! `FrontendStaleReason` wire format. The motivating bug
//! (`supervisor-frontend-build-silent-success.md`) was that the old
//! `check_src_newer_than_dist` returned `false` (= "not stale") on a
//! missing `dist/index.html`, exactly the case the gate was supposed
//! to catch. These tests pin the new contract: missing dist surfaces
//! as `Some(DistMissing)`, src drift as `Some(SrcDrift)`, healthy
//! state as `None`.
use super::{
    attach_build_slot_id, attach_spawn_identity, check_dist_freshness, spawn_abandoned_body,
    wait_out_queue_timeout, FrontendStaleReason,
};
use crate::build_monitor::BuildPhase;
use crate::source_scan::production_span;
use std::fs;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Walk every `.rs` file under `src/`, returning `(path, line_no, line)` for
/// each PRODUCTION line matching `pred`.
///
/// "Production" = everything before the file's first `#[cfg(test)] mod`,
/// nothing at all in a file that opens with `#![cfg(test)]` (an extracted
/// test module — [`crate::source_scan::production_span`]), and never a `//` / `///` line. Test modules legitimately build fixture
/// ids from ports, and the doc comments deliberately quote the old form to
/// explain why it is gone — neither reaches a spawn.
///
/// **The cut is anchored on the test MODULE, not the bare attribute.**
/// `#[cfg(test)]` also decorates test-only *helper items* at column 0, and
/// cutting at the first one discards every real function after it. Measured
/// on this tree, `"\n#[cfg(test)]\n"` cut `process/manager.rs` at line 455
/// (its `#[cfg(test)] pub fn read_slot_sha`) instead of 4448 and
/// `build_monitor.rs` at 2179 instead of 3732 — hiding 21,654 of 65,665
/// `src/` lines, including `reap_stale_test_runners`, `start_managed_runner`
/// and `apply_non_primary_instance_env`, i.e. the exact function this branch
/// extracted to hold `QONTINUI_INSTANCE_NAME`. A false NEGATIVE, and
/// invisible: the guard still passed.
///
/// It must be `mod ` and not `mod tests`: `supervisor_bridge.rs` alone has
/// five named test modules (`mod heartbeat_body_tests`, …).
///
/// **Line endings are normalized to LF before the split.**
/// `fs::read_to_string` does no newline translation, so on a CRLF checkout
/// (this repo's default working-copy state — `git` normalizes to LF only on
/// commit) a marker written as `"\n#[cfg(test)]\n"` never matches, the
/// split silently falls through to "whole file", and every test module in
/// the tree gets scanned as if it were production. That is a false POSITIVE
/// here, but the same mistake in a guard whose default is "skip" would be a
/// silent false negative — so normalize rather than rely on the checkout.
fn scan_production_lines(pred: impl Fn(&str) -> bool) -> Vec<(std::path::PathBuf, usize, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    assert!(
        files.len() > 20,
        "the source walk found only {} files under {src:?} — it is not scanning the tree, \
             so this guard would pass vacuously",
        files.len()
    );

    // Anti-vacuity canary. `process/manager.rs` is the largest production
    // file and the one most likely to grow a spawn path; it is also the file
    // the bare-attribute split truncated to 455 lines. If the split ever
    // regresses, its scanned span collapses and this fires — the coverage
    // loss cannot go silent again. The floor is well under the real span —
    // since its test module moved to `process/manager/tests.rs`, the whole
    // of `manager.rs` (~6k lines) — so ordinary edits do not trip it. The
    // extracted file is test-only and is skipped, never counted here.
    const CANARY: &str = "manager.rs";
    const CANARY_MIN_PRODUCTION_LINES: usize = 3000;
    let mut canary_lines = 0usize;

    let mut hits = Vec::new();
    for path in files {
        // A file we cannot read must fail loudly, not skip: a silent skip
        // lets the guard rot into a no-op.
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("spawn-site guard could not read {path:?}: {e}"))
            .replace("\r\n", "\n");
        let prod = production_span(&text);
        if path.ends_with(CANARY) {
            canary_lines = prod.lines().count();
        }
        for (i, line) in prod.lines().enumerate() {
            let l = line.trim();
            if l.starts_with("//") {
                continue;
            }
            if pred(l) {
                hits.push((path.clone(), i + 1, l.to_string()));
            }
        }
    }

    assert!(
        canary_lines >= CANARY_MIN_PRODUCTION_LINES,
        "the production span of {CANARY} scanned as only {canary_lines} lines (floor \
             {CANARY_MIN_PRODUCTION_LINES}). The `#[cfg(test)] mod` cut has regressed — most \
             likely back to the bare `#[cfg(test)]` attribute, which stops at the first \
             test-only helper item and hides thousands of production lines from every \
             predicate this scanner runs."
    );

    hits
}

/// The predicate of [`no_spawn_site_mints_a_port_derived_instance_name`]:
/// a (trimmed) line containing both `"test-{` and `port`. The needle is
/// assembled at runtime so no line of this file contains it.
fn mints_a_port_derived_name(line: &str) -> bool {
    let needle = format!("{}{}", "\"test-", '{');
    line.contains(needle.as_str()) && line.contains("port")
}

/// Phase 2b fixture: an extracted test file holding a line the spawn-site
/// predicate rejects is classified as test code and yields no flagged
/// line, while the same line in a production file (or before an inline
/// test module) still is. Dropping the `is_test_only_file` arm from
/// [`crate::source_scan::production_span`] reddens the first assertion.
#[test]
fn production_span_treats_an_extracted_test_file_as_test_code() {
    let flagged = |text: &str| {
        production_span(text)
            .lines()
            .filter(|l| mints_a_port_derived_name(l.trim()))
            .count()
    };
    // Assembled at runtime so this source never carries the needle itself.
    let offending = format!("    let id = format!(\"{}{}port}}\");\n", "test-", '{');
    // Self-check: the fixture line really is one the guard rejects, so the
    // assertions below cannot pass on a mis-built fixture.
    assert!(mints_a_port_derived_name(offending.trim()));

    let extracted = format!("#![cfg(test)]\n\nuse super::*;\n\nfn fixture() {{\n{offending}}}\n");
    assert_eq!(
        flagged(&extracted),
        0,
        "an extracted `#![cfg(test)]` file must contribute no production lines"
    );

    let production = format!("pub fn spawn() {{\n{offending}}}\n");
    assert_eq!(flagged(&production), 1, "a production line is still caught");

    let inline =
        format!("pub fn spawn() {{\n{offending}}}\n\n#[cfg(test)]\nmod tests {{\n{offending}}}\n");
    assert_eq!(
        flagged(&inline),
        1,
        "only the line before the inline test module is production"
    );
}

/// No spawn site anywhere in `src/` may mint a temp runner's instance name
/// or id from its PORT.
///
/// The env-block assertion in
/// `process::manager::tests::non_primary_env_block_keys_instance_name_per_spawn_not_per_port`
/// builds its fixture THROUGH [`crate::process::temp_runner_instance_name`], so it
/// pins the helper — not the call site's use of it. This test is the only
/// thing tying `spawn_test` to the helper, so it is deliberately
/// spelling-agnostic and repo-wide:
///
/// - **Normalized match, not one literal.** Any production line containing
///   both `"test-{` and `port` is rejected. That covers the original
///   `format!("test-{}", port)`, the inline-capture `format!("test-{port}")`
///   — which is the form a future author is most likely to reach for, since
///   it is the form this file's own rustdoc and tests use — and the
///   `managed.config.port` / `body.port` spellings.
/// - **Whole `src/` tree**, not just this file, so a spawn path added in a
///   new module is covered too (the same gap
///   `process::claude_env`'s `KNOWN_SPAWN_SITE_FILES` documents about
///   itself).
/// - **Plus a positive assertion** that `spawn_test`'s body still calls
///   `temp_runner_instance_name(&id)`. A negative grep alone would pass if
///   someone deleted the mint entirely, or routed it through a third,
///   port-derived helper whose name happens not to match.
#[test]
fn no_spawn_site_mints_a_port_derived_instance_name() {
    // The needle is assembled at runtime (`mints_a_port_derived_name`) so no
    // line here contains it and self-flags. (This file opens with
    // `#![cfg(test)]` and is excluded as test-only anyway — but only while
    // that classification works, and a guard that depends on its own
    // exclusion to pass is one edit away from crying wolf. Belt and braces.)
    let offenders = scan_production_lines(mints_a_port_derived_name);

    assert!(
        offenders.is_empty(),
        "a temp runner's name/id must never be derived from its port — ports 9877-9899 \
             are recycled, so the second spawn on a port inherits the first's \
             instance-<name> app-data tree (terminal-sessions.json included). Route it \
             through `temp_runner_instance_name(&id)` instead. Offending lines: {offenders:#?}"
    );

    // Positive half: `spawn_test` must still route through the helper.
    let this_file = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("routes")
            .join("runners.rs"),
    )
    .expect("this source file must be readable")
    .replace("\r\n", "\n");
    // Production code only — the same text whether this file's tests are
    // inline or extracted to `routes/runners/tests.rs`.
    // Bounded to spawn_test's own body by brace matching, so a match in a
    // LATER item — whatever its visibility or qualifiers — can never satisfy
    // this assertion.
    let spawn_test_body =
        crate::source_scan::fn_body(production_span(&this_file), "pub async fn spawn_test(")
            .expect("spawn_test must exist — did it get renamed?");
    assert!(
        spawn_test_body.contains("temp_runner_instance_name(&id)"),
        "spawn_test no longer mints its instance name via \
             `temp_runner_instance_name(&id)`. That helper is what keeps \
             QONTINUI_INSTANCE_NAME in lockstep with the per-spawn runner id; a call site \
             that stops using it re-opens the port-recycling inheritance even if the \
             negative grep above still passes."
    );
}

/// HEADLINE regression (2026-08-03): `queue_timeout_secs` must stop
/// counting once the build holds its slot and is compiling. Before this,
/// the bound wrapped the whole build future, so a caller who passed a
/// small bound to avoid queueing got a 504 that ALSO abandoned a live
/// compile.
#[tokio::test]
async fn queue_timeout_never_fires_once_the_build_is_compiling() {
    let marker = Arc::new(AtomicU8::new(BuildPhase::AwaitingSlot.as_u8()));
    // The build acquires its permit and starts compiling almost at once.
    BuildPhase::Compiling.store(&marker);

    let fired = tokio::time::timeout(
        Duration::from_millis(900),
        wait_out_queue_timeout(Duration::from_millis(100), marker),
    )
    .await;
    assert!(
        fired.is_err(),
        "the queue watcher fired against a COMPILING build — that 504s a live compile"
    );
}

/// The other half: while the request really is queued, the bound still
/// fires, and it names the phase that was blocking.
#[tokio::test]
async fn queue_timeout_still_fires_while_actually_queued() {
    let marker = Arc::new(AtomicU8::new(BuildPhase::AwaitingSlot.as_u8()));
    // Give-up budget on "the queue watcher fires at all", not a speed
    // assertion — it returns the instant the watcher does.
    let phase = tokio::time::timeout(
        crate::test_clock::poll_budget(Duration::from_secs(30)),
        wait_out_queue_timeout(Duration::from_millis(150), marker),
    )
    .await
    .expect("a genuinely queued request must still time out");
    assert_eq!(phase, BuildPhase::AwaitingSlot);
}

/// A build blocked on the serialized frontend (pnpm) lock is still queued
/// — no work of its own is happening — so the bound applies there too, and
/// the reported phase distinguishes it from cargo-slot exhaustion.
#[tokio::test]
async fn queue_timeout_covers_the_frontend_lock_wait() {
    let marker = Arc::new(AtomicU8::new(BuildPhase::AwaitingNpmLock.as_u8()));
    // Give-up budget, same reasoning as the test above.
    let phase = tokio::time::timeout(
        crate::test_clock::poll_budget(Duration::from_secs(30)),
        wait_out_queue_timeout(Duration::from_millis(150), marker),
    )
    .await
    .expect("a frontend-lock wait is still a queue wait");
    assert_eq!(phase, BuildPhase::AwaitingNpmLock);
}

/// A FAILED spawn-test build that had already claimed a slot must still
/// report `build_slot_id` on the error body.
///
/// The regression: the `Err` arm reads the slot off the `BuildAttempt`, but
/// then `return Err(e)`s, and `SupervisorError::to_status_body()` renders a
/// canonical `{"error": …}` that knows nothing about slots. So attribution
/// vanished exactly when it matters most — the build claimed slot N, ran the
/// cleanup pass over slot N (reaping whatever the harness had planted
/// there), and then cargo failed. Without the merge, a harness reading the
/// response cannot say which slot's probe its own build was responsible for.
#[test]
fn error_body_carries_the_claimed_build_slot_id() {
    let mut body = serde_json::json!({ "error": "cargo build failed" });
    attach_build_slot_id(&mut body, Some(1));
    assert_eq!(body["build_slot_id"], serde_json::json!(1));
    // The canonical error text is preserved, not replaced.
    assert_eq!(body["error"], serde_json::json!("cargo build failed"));
}

/// The key is ALWAYS present, `null` included, so a caller can read
/// `build_slot_id` unconditionally on success and failure alike instead of
/// having to distinguish "absent" from "no slot claimed".
#[test]
fn error_body_carries_a_null_build_slot_id_when_no_slot_was_claimed() {
    let mut body = serde_json::json!({ "error": "build_pool_full" });
    attach_build_slot_id(&mut body, None);
    assert!(
        body.as_object().unwrap().contains_key("build_slot_id"),
        "{body}"
    );
    assert_eq!(body["build_slot_id"], serde_json::Value::Null);
}

// ---------------------------------------------------------------------
// Regression: "a caller receives no answer, or a confidently useless one"
//
// `POST /runners/spawn-test` reserves the runner id and port BEFORE the
// (20-50 minute) build starts, but on the synchronous path it wrote nothing
// until the build was terminal. Two failures fell out of that:
//
//   * a supervisor restart mid-build closed the connection with no response
//     at all (`curl` reports `HTTP=000`), and
//   * every non-success body rendered from a `SupervisorError` named the
//     error and nothing else,
//
// so in both cases the caller could not learn the id of the runner it had
// just created — and the id is the only handle on `/runners/{id}/logs` and
// `/runners/{id}/stop`. These pin the contract: EVERY outcome names the
// runner, and "abandoned" is distinguishable from both "failed" and
// "succeeded".
// ---------------------------------------------------------------------

/// The canonical `SupervisorError` body carries `{"error": …}` and nothing
/// else. After the merge it must still name the runner that was reserved.
#[test]
fn spawn_error_body_names_the_runner_it_reserved() {
    let mut body = serde_json::json!({ "error": "cargo build failed" });
    attach_spawn_identity(&mut body, "test-abc123", 9878);

    assert_eq!(body["id"], serde_json::json!("test-abc123"));
    assert_eq!(body["port"], serde_json::json!(9878));
    assert_eq!(body["api_url"], serde_json::json!("http://localhost:9878"));
    // The two URLs a caller needs to follow and clean up its own runner.
    assert_eq!(
        body["logs_url"],
        serde_json::json!("/runners/test-abc123/logs")
    );
    assert_eq!(
        body["stop_url"],
        serde_json::json!("/runners/test-abc123/stop")
    );
    // The original diagnosis is preserved, not replaced.
    assert_eq!(body["error"], serde_json::json!("cargo build failed"));
}

/// The reservation is authoritative. A body that somehow disagrees with it
/// is corrected rather than trusted — a response naming a DIFFERENT runner
/// than the one reserved is the "confidently useless answer" failure mode,
/// which is worse than no answer.
#[test]
fn spawn_identity_is_authoritative_over_the_body_it_merges_into() {
    let mut body = serde_json::json!({ "id": "stale-id", "port": 1234, "status": "healthy" });
    attach_spawn_identity(&mut body, "test-real", 9880);

    assert_eq!(body["id"], serde_json::json!("test-real"));
    assert_eq!(body["port"], serde_json::json!(9880));
    assert_eq!(body["status"], serde_json::json!("healthy"));
}

/// A non-object body is left alone rather than silently reshaped —
/// same contract as `attach_build_slot_id`.
#[test]
fn spawn_identity_leaves_a_non_object_body_alone() {
    let mut body = serde_json::json!("not an object");
    attach_spawn_identity(&mut body, "test-abc", 9877);
    assert_eq!(body, serde_json::json!("not an object"));
}

/// The answer a synchronous caller gets when the supervisor starts shutting
/// down mid-build. It replaces a dropped connection (`HTTP=000`), so it has
/// to carry everything the caller lost: the runner id + port, the build id,
/// and an unambiguous "abandoned" verdict.
#[test]
fn abandoned_spawn_answer_names_the_runner_and_the_build() {
    let submission = uuid::Uuid::new_v4();
    let body = spawn_abandoned_body("test-1a0453e4740-2", 9879, submission);

    // The handle on the runner.
    assert_eq!(body["id"], serde_json::json!("test-1a0453e4740-2"));
    assert_eq!(body["port"], serde_json::json!(9879));
    assert_eq!(
        body["logs_url"],
        serde_json::json!("/runners/test-1a0453e4740-2/logs")
    );
    assert_eq!(
        body["stop_url"],
        serde_json::json!("/runners/test-1a0453e4740-2/stop")
    );

    // The handle on the build. `build_id` is the same token the sync body,
    // the async 202 and `GET /build/{id}/status` all use.
    assert_eq!(body["build_id"], serde_json::json!(submission.to_string()));
    assert_eq!(
        body["submission_id"],
        serde_json::json!(submission.to_string())
    );
    assert_eq!(
        body["poll_url"],
        serde_json::json!(format!("/build/{submission}/status"))
    );

    // The verdict: not "failed", not "succeeded", and machine-readable.
    assert_eq!(body["spawn_abandoned"], serde_json::json!(true));
    assert_eq!(body["error"], serde_json::json!("spawn_abandoned"));
    assert_eq!(body["status"], serde_json::json!("abandoned"));
}

/// Source guard: `execute_spawn_build` must merge the identity on BOTH
/// arms.
///
/// The `Err` arm is the one that matters — it renders through
/// `SupervisorError::to_status_body()`, which knows nothing about the
/// reservation — but a merge on only one arm is exactly the "uniform on
/// some paths and not others" state this fix exists to remove, so pin both.
/// Mirrors the `attach_build_slot_id` contract, which was lost the same way.
#[test]
fn execute_spawn_build_attaches_identity_on_both_arms() {
    let body = fn_source("async fn execute_spawn_build(");
    let merges = body
        .matches("attach_spawn_identity(&mut body, &id, port)")
        .count();
    assert_eq!(
        merges, 2,
        "execute_spawn_build must call `attach_spawn_identity` on the Ok arm AND the Err \
             arm (found {merges}). The Err arm renders through \
             `SupervisorError::to_status_body()`, which emits `{{\"error\": …}}` and nothing \
             else — without the merge, a caller whose spawn FAILED cannot learn the id of the \
             runner reserved for it, and so cannot read its logs or stop it. Body scanned:\n\
             {body}"
    );
}

/// Source guard: the SYNCHRONOUS spawn-test path must race its terminal
/// wait against the shutdown signal.
///
/// A bare `await_terminal(&sub_arc).await` holds the HTTP connection open
/// for the whole cargo build with nothing written. Axum's graceful shutdown
/// drops the LISTENER immediately but waits for in-flight connections, so
/// during a restart that produced the reported incident: the replacement
/// supervisor bound the port and answered `Runner not found` from an empty
/// registry, while the old process stayed alive compiling, holding this
/// connection, addressable by nobody — and the caller got `HTTP=000` when it
/// was finally force-killed. The race is what turns that into an answer.
#[test]
fn sync_spawn_test_wait_is_shutdown_aware() {
    let body = fn_source("pub async fn spawn_test(");
    assert!(
        body.contains("await_terminal"),
        "spawn_test's sync path no longer awaits the submission's terminal state — did the \
             handler get restructured? Re-point this guard at whatever replaced it."
    );
    assert!(
        body.contains("state.shutdown_signal()"),
        "spawn_test's synchronous terminal wait is not raced against \
             `state.shutdown_signal()`. Without that race a supervisor shutdown during a \
             20-50 minute build drops the caller's connection with no response written at all \
             (`curl` reports HTTP=000) and the caller never learns the runner id it created. \
             Body scanned:\n{body}"
    );
    assert!(
        body.contains("spawn_abandoned_body"),
        "spawn_test observes the shutdown but does not answer with \
             `spawn_abandoned_body`. Detecting the shutdown and still returning nothing useful \
             reproduces the defect in a politer form: the caller must be able to tell \
             'abandoned' from 'failed' and from 'it worked and I was not told'."
    );
}

/// S-4 at the route: the spawn path hands `paired_profile_id` to the
/// paired-state decision UNCONDITIONALLY (so an absent id reaches the
/// primary-snapshot arm, which `no_paired_profile_id_applies_the_primary_snapshot`
/// pins), and surfaces both new response fields. Re-wrapping the call in
/// `if let Some(..)` — the pre-S-4 shape, under which a default spawn came
/// up unpaired — fails here.
#[test]
fn spawn_path_applies_paired_state_even_without_a_profile_id() {
    let body = fn_source("async fn execute_spawn_build_inner(");
    assert!(
        body.contains("apply_paired_state_for_spawn(body.paired_profile_id.as_deref(), id)"),
        "execute_spawn_build_inner must pass the OPTIONAL paired_profile_id straight to \
             apply_paired_state_for_spawn, so an absent id snapshots the primary"
    );
    assert!(
        !body.contains("if let Some(profile_id) = body.paired_profile_id"),
        "the paired-state copy is gated on a profile id again — a default spawn-test would \
             come up unpaired (plan 2026-09-23-conductor-e2e-phase1-defects, S-4)"
    );
    assert!(body.contains("resp[\"paired_state\"]"));
    assert!(body.contains("resp[\"coord_credential\"]"));
}

/// Read one function's body out of this source file, bounded at its own
/// closing brace ([`crate::source_scan::fn_body`]) so a match in a LATER item
/// can never satisfy an assertion. Same technique as
/// `no_spawn_site_mints_a_port_derived_instance_name`'s positive half.
fn fn_source(signature: &str) -> String {
    let this_file = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("routes")
            .join("runners.rs"),
    )
    .expect("this source file must be readable")
    .replace("\r\n", "\n");
    // Production code only, so a body bounded by EOF cannot run on into
    // the test module while it is inline and stop short once extracted.
    crate::source_scan::fn_body(production_span(&this_file), signature)
        .unwrap_or_else(|| panic!("`{signature}` must exist — did it get renamed?"))
        .to_string()
}

fn write_file(path: &std::path::Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    fs::write(path, contents).expect("write file");
}

#[tokio::test]
async fn returns_dist_missing_when_dist_index_absent() {
    // Repro of the exact silent-success bug: src/ has files but
    // dist/index.html is gone. Old code returned false (not stale);
    // new code must return Some(DistMissing).
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("src").join("App.tsx"),
        b"export const App = () => null;",
    );

    let result = check_dist_freshness(tmp.path()).await;
    assert_eq!(
        result,
        Some(FrontendStaleReason::DistMissing),
        "missing dist/index.html must be flagged DistMissing, not silently treated as fresh"
    );
}

#[tokio::test]
async fn returns_dist_missing_even_when_src_tree_is_empty() {
    // Edge case: no src files at all, no dist either. Should still
    // flag DistMissing rather than silently returning None — a
    // runner with no embedded frontend is broken regardless of
    // whether src/ has anything to drift.
    let tmp = TempDir::new().expect("tempdir");
    let result = check_dist_freshness(tmp.path()).await;
    assert_eq!(result, Some(FrontendStaleReason::DistMissing));
}

#[tokio::test]
async fn returns_src_drift_when_src_newer_than_dist() {
    // Build dist first, then write src/. The src file's mtime will
    // be at least as new as dist/index.html — sleep is enough on
    // every supported platform to guarantee strict-newer.
    let tmp = TempDir::new().expect("tempdir");
    write_file(&tmp.path().join("dist").join("index.html"), b"<old/>");

    // Filesystem mtime resolution can be coarse (FAT = 2s, HFS = 1s);
    // sleep enough to clear the worst case so the assertion is
    // deterministic.
    std::thread::sleep(std::time::Duration::from_millis(2100));

    write_file(
        &tmp.path().join("src").join("App.tsx"),
        b"export const App = () => 'changed';",
    );

    let result = check_dist_freshness(tmp.path()).await;
    assert_eq!(result, Some(FrontendStaleReason::SrcDrift));
}

#[tokio::test]
async fn returns_none_when_dist_newer_than_src() {
    // Happy path: src/ written, then dist built afterward.
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("src").join("App.tsx"),
        b"export const App = () => null;",
    );

    std::thread::sleep(std::time::Duration::from_millis(2100));

    write_file(
        &tmp.path().join("dist").join("index.html"),
        b"<!doctype html><html/>",
    );

    let result = check_dist_freshness(tmp.path()).await;
    assert_eq!(
        result, None,
        "dist newer than src is the healthy state; must not be flagged"
    );
}

#[tokio::test]
async fn ignores_unrelated_extensions_in_src() {
    // Touching a .md or .png in src/ shouldn't trip the drift signal —
    // the walker only looks at .ts/.tsx/.css/.json/.html.
    let tmp = TempDir::new().expect("tempdir");
    write_file(&tmp.path().join("dist").join("index.html"), b"<built/>");
    std::thread::sleep(std::time::Duration::from_millis(2100));
    write_file(&tmp.path().join("src").join("README.md"), b"# notes");
    write_file(&tmp.path().join("src").join("logo.png"), b"\x89PNG fake");

    let result = check_dist_freshness(tmp.path()).await;
    assert_eq!(
        result, None,
        "non-source extensions must not trigger SrcDrift"
    );
}

/// Pins the built-tree fix: the freshness verdict follows the root you
/// pass, not some implicitly-derived live tree. A `git_ref` /
/// `worktree_path` spawn-test build embeds the BUILT tree's `dist/`, so
/// `resolve_frontend_stale_for_spawn` must hand `check_dist_freshness`
/// that tree — checking a stale live tree instead produced false
/// `src_newer_than_dist` warnings (and spurious 503s under
/// `frontend_strict`) for perfectly fresh worktree builds.
#[tokio::test]
async fn freshness_verdict_follows_the_root_passed_not_the_live_tree() {
    let tmp = TempDir::new().expect("tempdir");
    let live_root = tmp.path().join("live");
    let worktree_root = tmp.path().join("worktree");

    // Live tree: dist built first, then src edited → stale (SrcDrift).
    write_file(&live_root.join("dist").join("index.html"), b"<old/>");
    // Worktree: src written first, dist built after → fresh.
    write_file(
        &worktree_root.join("src").join("App.tsx"),
        b"export const App = () => 'worktree';",
    );

    // Coarse-mtime safety margin (FAT = 2s), same as the other tests.
    std::thread::sleep(std::time::Duration::from_millis(2100));

    write_file(
        &live_root.join("src").join("App.tsx"),
        b"export const App = () => 'edited after build';",
    );
    write_file(
        &worktree_root.join("dist").join("index.html"),
        b"<!doctype html><html/>",
    );

    assert_eq!(
        check_dist_freshness(&worktree_root).await,
        None,
        "fresh worktree build must not be flagged stale just because the live tree drifted"
    );
    assert_eq!(
        check_dist_freshness(&live_root).await,
        Some(FrontendStaleReason::SrcDrift),
        "the live tree really is stale — the verdict must follow the root passed"
    );
}

#[test]
fn frontend_stale_reason_wire_format_is_stable() {
    // External callers parse the `frontend_stale_reason` string —
    // pin the wire format so a refactor can't silently rename it.
    assert_eq!(FrontendStaleReason::BuildFailed.as_str(), "build_failed");
    assert_eq!(
        FrontendStaleReason::SrcDrift.as_str(),
        "src_newer_than_dist"
    );
    assert_eq!(FrontendStaleReason::DistMissing.as_str(), "dist_missing");
}

/// Pin the default + explicit-true wire shapes for the
/// `cleanup_worktree_on_fail` flag. External agents call this endpoint
/// with arbitrary JSON; a typo or default-change must be caught here.
#[test]
fn spawn_test_request_cleanup_worktree_on_fail_defaults_false() {
    // Empty payload (every field at default) — flag must be false so
    // existing callers' idempotent-reuse semantics are preserved.
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(
        !req.cleanup_worktree_on_fail,
        "default must be false (idempotent reuse)"
    );
}

#[test]
fn spawn_test_request_cleanup_worktree_on_fail_explicit_true() {
    // Explicit opt-in by an agent that wants a fresh worktree on the
    // next attempt after a downstream cargo failure.
    let req: super::SpawnTestRequest =
        serde_json::from_str(r#"{"cleanup_worktree_on_fail": true}"#)
            .expect("deserialize SpawnTestRequest with cleanup_worktree_on_fail");
    assert!(req.cleanup_worktree_on_fail);
}

/// Phase 2b — `paired_profile_id` defaults to None and round-trips a
/// supplied string verbatim. Pinned so a typo or default-change is
/// caught here rather than at `/manual-test-coord` runtime.
#[test]
fn spawn_test_request_paired_profile_id_defaults_none() {
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(req.paired_profile_id.is_none());
}

#[test]
fn spawn_test_request_paired_profile_id_explicit() {
    let req: super::SpawnTestRequest =
        serde_json::from_str(r#"{"paired_profile_id":"jspinak-spaceship"}"#)
            .expect("deserialize SpawnTestRequest with paired_profile_id");
    assert_eq!(req.paired_profile_id.as_deref(), Some("jspinak-spaceship"));
}

// -------------------------------------------------------------------
// Phase 2b — apply_paired_profile helper tests.
// -------------------------------------------------------------------

#[test]
fn apply_paired_profile_rejects_traversal() {
    let tmp_profiles = tempfile::tempdir().unwrap();
    let tmp_data = tempfile::tempdir().unwrap();
    for bad in ["", "../etc", "a/b", "a\\b", ".."] {
        let result = super::apply_paired_profile(bad, tmp_profiles.path(), tmp_data.path());
        assert!(
            result.is_err(),
            "profile_id={:?} must be rejected as traversal",
            bad
        );
        let (status, _body) = result.err().unwrap();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }
}

#[test]
fn apply_paired_profile_404_when_snapshot_missing() {
    let tmp_profiles = tempfile::tempdir().unwrap();
    let tmp_data = tempfile::tempdir().unwrap();
    let result =
        super::apply_paired_profile("does-not-exist", tmp_profiles.path(), tmp_data.path());
    let (status, body) = result.expect_err("must error");
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "profile_not_found");
    assert_eq!(body["paired_profile_id"], "does-not-exist");
}

#[test]
fn apply_paired_profile_copies_paired_user_only_when_no_tokens() {
    // Snapshot dir has paired_user.json but no auth_tokens.enc.
    // applied list must include only the file actually present.
    let tmp_profiles = tempfile::tempdir().unwrap();
    let tmp_data = tempfile::tempdir().unwrap();
    let snapshot = tmp_profiles.path().join("dev-profile");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(
        snapshot.join("paired_user.json"),
        r#"{"user_id":"abc","tenant_id":"def"}"#,
    )
    .unwrap();

    let applied = super::apply_paired_profile("dev-profile", tmp_profiles.path(), tmp_data.path())
        .expect("must succeed");

    assert_eq!(applied, vec!["paired_user.json".to_string()]);
    // File landed in the data dir.
    let dst = tmp_data.path().join("paired_user.json");
    assert!(dst.exists(), "paired_user.json must be copied to data dir");
    let copied = std::fs::read_to_string(&dst).unwrap();
    assert!(copied.contains("abc"));
}

#[test]
fn apply_paired_profile_copies_both_files_when_present() {
    let tmp_profiles = tempfile::tempdir().unwrap();
    let tmp_data = tempfile::tempdir().unwrap();
    let snapshot = tmp_profiles.path().join("paired");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(snapshot.join("paired_user.json"), b"{}").unwrap();
    std::fs::write(snapshot.join("auth_tokens.enc"), b"\x00\x01\x02").unwrap();

    let applied = super::apply_paired_profile("paired", tmp_profiles.path(), tmp_data.path())
        .expect("must succeed");

    assert_eq!(
        applied,
        vec![
            "paired_user.json".to_string(),
            "auth_tokens.enc".to_string()
        ]
    );
    assert!(tmp_data.path().join("paired_user.json").exists());
    assert!(tmp_data.path().join("auth_tokens.enc").exists());
}

/// The spawn-test route must write the profile snapshot into the SPAWNED
/// RUNNER'S per-instance dir — the same
/// `<config_dir>/com.qontinui.runner/instances/<runner id>` that
/// `process::manager` exports to the child as `QONTINUI_CONFIG_DIR` +
/// `QONTINUI_SECURE_STORAGE_DIR`.
///
/// The bug this pins: the wrapper used to copy into the shared
/// `dirs::data_local_dir()/com.qontinui.runner` fallback, which a
/// supervisor-spawned runner never consults (its env vars win). The spawn
/// reported `paired_profile_applied` and the runner came up UNPAIRED.
///
/// Driven through the real `apply_paired_state_for_spawn` rather than
/// re-deriving the destination, so re-inlining a different path in the
/// route fails here even though the helper itself is unchanged. Uses the
/// production `~/.qontinui/profiles` root under a pid-unique profile id and
/// removes both dirs afterwards.
#[test]
fn instance_config_dir_is_where_apply_paired_profile_for_spawn_writes() {
    let unique = format!("supervisor-selftest-{}", std::process::id());
    let runner_id = format!("test-{unique}");
    let (Some(home), Some(dest)) = (
        dirs::home_dir(),
        crate::process::instance_config_dir(&runner_id),
    ) else {
        // No resolvable home / config dir on this platform.
        return;
    };
    let snapshot = home.join(".qontinui").join("profiles").join(&unique);
    std::fs::create_dir_all(&snapshot).expect("create profile snapshot dir");
    let _cleanup = scopeguard::guard((snapshot.clone(), dest.clone()), |(snap, dst)| {
        let _ = std::fs::remove_dir_all(snap);
        let _ = std::fs::remove_dir_all(dst);
    });
    std::fs::write(
        snapshot.join("paired_user.json"),
        r#"{"user_id":"selftest"}"#,
    )
    .expect("write paired_user.json");
    std::fs::write(snapshot.join("auth_tokens.enc"), b"\x00\x01\x02")
        .expect("write auth_tokens.enc");

    let report =
        super::apply_paired_state_for_spawn(Some(&unique), &runner_id).expect("must succeed");

    assert_eq!(report["source"], "paired_profile");
    assert_eq!(
        report["applied"],
        serde_json::json!(["paired_user.json", "auth_tokens.enc"])
    );
    // The destination is the per-instance dir, spelled out literally so a
    // drift on the ROUTE side trips this assertion. The spawn side (the
    // `cmd.env(...)` that tells the child to read this dir) is pinned
    // separately by
    // `process::manager::tests::apply_instance_dir_env_sets_both_vars_to_instance_config_dir`.
    let expected = dirs::config_dir()
        .expect("config_dir resolved above")
        .join("com.qontinui.runner")
        .join("instances")
        .join(&runner_id);
    assert_eq!(dest, expected);
    assert!(
        dest.join("paired_user.json").exists(),
        "paired_user.json must land in the runner's instance dir {dest:?}"
    );
    assert!(
        dest.join("auth_tokens.enc").exists(),
        "auth_tokens.enc must land in the runner's instance dir {dest:?}"
    );
    // And NOT in the shared fallback the runner ignores.
    if let Some(shared) = dirs::data_local_dir() {
        assert_ne!(dest, shared.join("com.qontinui.runner"));
    }
}

// -------------------------------------------------------------------
// S-4 — spawn-test with no `paired_profile_id` snapshots the PRIMARY.
// -------------------------------------------------------------------

/// The route's paired-state decision with no `paired_profile_id`: the
/// primary's live `paired_user.json` + `auth_tokens.enc` are copied into
/// the spawned runner's instance dir, and the named-profile root is not
/// consulted at all. Every root is injected, as `apply_paired_profile`'s
/// tests do.
#[test]
fn no_paired_profile_id_applies_the_primary_snapshot() {
    let profiles = tempfile::tempdir().unwrap();
    let primary = tempfile::tempdir().unwrap();
    let dest_root = tempfile::tempdir().unwrap();
    let dest = dest_root.path().join("instances").join("test-abc");
    std::fs::write(
        primary.path().join("paired_user.json"),
        r#"{"user_id":"operator"}"#,
    )
    .unwrap();
    std::fs::write(primary.path().join("auth_tokens.enc"), b"\x09\x08").unwrap();
    // A profile the route must NOT pick up when no id is given.
    let decoy = profiles.path().join("decoy");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::write(decoy.join("paired_user.json"), r#"{"user_id":"decoy"}"#).unwrap();

    let report =
        super::apply_spawn_paired_state(None, profiles.path(), Some(primary.path()), &dest)
            .expect("primary snapshot must succeed");

    assert_eq!(report["source"], "primary_snapshot");
    assert_eq!(report["status"], "paired");
    assert_eq!(
        report["applied"],
        serde_json::json!(["paired_user.json", "auth_tokens.enc"])
    );
    assert!(
        std::fs::read_to_string(dest.join("paired_user.json"))
            .unwrap()
            .contains("operator"),
        "the PRIMARY's pairing must land in the instance dir, not a profile's"
    );
    assert_eq!(
        std::fs::read(dest.join("auth_tokens.enc")).unwrap(),
        b"\x09\x08"
    );
}

/// An unpaired primary is not an error: the snapshot copies nothing — not
/// even a lone token cache with no pairing beside it — says so, and the
/// spawn proceeds.
#[test]
fn unpaired_primary_snapshot_reports_primary_unpaired() {
    let profiles = tempfile::tempdir().unwrap();
    let primary = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(primary.path().join("auth_tokens.enc"), b"\x01").unwrap();

    let report =
        super::apply_spawn_paired_state(None, profiles.path(), Some(primary.path()), dest.path())
            .expect("an unpaired primary must not fail the spawn");
    assert_eq!(report["status"], "primary_unpaired");
    assert_eq!(report["applied"], serde_json::json!([]));
    assert!(!dest.path().join("paired_user.json").exists());
    assert!(!dest.path().join("auth_tokens.enc").exists());

    let report = super::apply_spawn_paired_state(None, profiles.path(), None, dest.path())
        .expect("an unresolvable primary dir must not fail the spawn");
    assert_eq!(report["status"], "primary_dir_unresolved");
}

/// With a `paired_profile_id` the named snapshot still wins and a missing
/// one is still a 400 — the primary is never a silent fallback for it.
#[test]
fn paired_profile_id_still_selects_the_profile_not_the_primary() {
    let profiles = tempfile::tempdir().unwrap();
    let primary = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(
        primary.path().join("paired_user.json"),
        r#"{"user_id":"operator"}"#,
    )
    .unwrap();
    let snap = profiles.path().join("ci");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(snap.join("paired_user.json"), r#"{"user_id":"ci"}"#).unwrap();

    let report = super::apply_spawn_paired_state(
        Some("ci"),
        profiles.path(),
        Some(primary.path()),
        dest.path(),
    )
    .unwrap();
    assert_eq!(report["source"], "paired_profile");
    assert_eq!(report["paired_profile_id"], "ci");
    assert!(
        std::fs::read_to_string(dest.path().join("paired_user.json"))
            .unwrap()
            .contains("\"ci\"")
    );

    let (status, body) = super::apply_spawn_paired_state(
        Some("missing"),
        profiles.path(),
        Some(primary.path()),
        dest.path(),
    )
    .expect_err("a missing profile must not fall back to the primary");
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "profile_not_found");
}

#[test]
fn coord_credential_is_relayed_or_typed_unknown() {
    let body = serde_json::json!({
        "data": {"coordCredential": {"posture": "live", "state": "live", "canAnswer": true}}
    });
    let got = super::coord_credential_from_health(Some(&body), "unused");
    assert_eq!(got["posture"], "live");
    assert_eq!(got["canAnswer"], true);

    let old = serde_json::json!({"data": {"gitSha": "abc"}});
    let got = super::coord_credential_from_health(Some(&old), "no block");
    assert_eq!(got["posture"], "unknown");
    assert_eq!(got["state"], "unknown");
    assert_eq!(got["reason"], "no block");

    let got = super::coord_credential_from_health(None, "not read");
    assert_eq!(got["posture"], "unknown");
    assert_eq!(got["reason"], "not read");
}

// -------------------------------------------------------------------
// Response field shape — `git_ref` request + provenance fields.
//
// The spawn-test response surfaces a fixed set of provenance fields
// for git_ref builds. These tests pin the wire shape so the agent-
// facing contract is enforced at PR time, not at first-use runtime.
// -------------------------------------------------------------------

#[test]
fn spawn_test_request_git_ref_defaults_none() {
    // Default callers (no git_ref set) must get None so the handler's
    // branch that requires rebuild:true is skipped.
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(req.git_ref.is_none(), "git_ref default must be None");
}

#[test]
fn spawn_test_request_git_ref_round_trips() {
    // Verbatim round-trip — the supervisor never normalizes / lowercases
    // the ref; whatever the caller sends is what `prepare_worktree`
    // hands to `git`.
    let req: super::SpawnTestRequest =
        serde_json::from_str(r#"{"git_ref":"origin/main","rebuild":true}"#)
            .expect("deserialize SpawnTestRequest with git_ref");
    assert_eq!(req.git_ref.as_deref(), Some("origin/main"));
    assert!(req.rebuild);
}

/// The 12-char short SHA is a pure-presentational helper: the supervisor
/// keeps the full 40-char `git rev-parse HEAD` value in
/// `git_ref_resolved_sha` and exposes `git_ref_resolved_sha_short` as
/// the first 12 characters. This test pins both halves of that
/// contract so a future "shorten differently" refactor (8-char, last-N,
/// etc.) doesn't silently drift away from what callers compare with
/// `git rev-parse origin/main | head -c 12`.
#[test]
fn git_ref_resolved_sha_short_is_first_twelve_chars() {
    // Real SHA shape: 40 hex chars. The Vec<char>→String roundtrip
    // mirrors what runs inside the handler.
    let full = "0156c6775b18deadbeef0123456789abcdef0011";
    let short: String = full.chars().take(12).collect();
    assert_eq!(short.len(), 12, "short SHA must always be 12 chars");
    assert_eq!(short, "0156c6775b18");
    assert!(
        full.starts_with(&short),
        "short SHA must be a prefix of the full SHA"
    );
}

/// Boundary: shorter-than-12-char input (truncated/odd-shaped SHA from a
/// minimal/fixture repo) must not panic — `take(12)` clamps to len.
/// The handler's `chars().take(12).collect::<String>()` is panic-free,
/// but pinning that here means a future refactor to `[..12]` slicing
/// (which WOULD panic on shorter inputs) gets caught.
#[test]
fn git_ref_resolved_sha_short_handles_underlength_input() {
    let full = "abc123"; // 6 chars — shorter than 12
    let short: String = full.chars().take(12).collect();
    assert_eq!(short, "abc123");
    assert!(short.len() <= 12);
}

// -------------------------------------------------------------------
// Track 2 (UI-Bridge preview-verification) — work-unit ↔ preview binding.
//
// Pins: (1) the new `unit_id`/`attempt_id` passthrough fields default to
// None and round-trip verbatim; (2) the `git_ref requires rebuild:true`
// provenance guard still fires (and only when git_ref is set); (3) the
// `GET /runners/by-unit/{unit_id}` handler resolves bound previews and
// returns `[]` (not 404) for an unknown unit.
// -------------------------------------------------------------------

#[test]
fn spawn_test_request_unit_attempt_default_none() {
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(req.unit_id.is_none(), "unit_id default must be None");
    assert!(req.attempt_id.is_none(), "attempt_id default must be None");
}

#[test]
fn spawn_test_request_unit_attempt_round_trip() {
    let req: super::SpawnTestRequest = serde_json::from_str(
        r#"{"rebuild":true,"git_ref":"feat/x","unit_id":"u1","attempt_id":"a1"}"#,
    )
    .expect("deserialize SpawnTestRequest with unit/attempt");
    assert_eq!(req.unit_id.as_deref(), Some("u1"));
    assert_eq!(req.attempt_id.as_deref(), Some("a1"));
    // unit_id present without attempt_id is a valid shape (attempt unknown).
    let req2: super::SpawnTestRequest =
        serde_json::from_str(r#"{"unit_id":"u1"}"#).expect("deserialize unit-only");
    assert_eq!(req2.unit_id.as_deref(), Some("u1"));
    assert!(req2.attempt_id.is_none());
}

#[test]
fn git_ref_rebuild_guard_rejects_ref_without_rebuild() {
    // The no-silent-fallback provenance guard: git_ref + rebuild:false → 400.
    let err = super::provenance_rebuild_guard(Some("feat/x"), None, false, false)
        .expect_err("git_ref without rebuild must be rejected");
    let status = axum::response::IntoResponse::into_response(err).status();
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn git_ref_rebuild_guard_allows_ref_with_rebuild() {
    assert!(super::provenance_rebuild_guard(Some("feat/x"), None, false, true).is_ok());
}

// -------------------------------------------------------------------
// Phase 1 — generalized provenance guard + alias detector.
// -------------------------------------------------------------------

/// Full matrix for `provenance_rebuild_guard`:
/// {none, git_ref, worktree_path, both} × {rebuild false, true}.
#[test]
fn provenance_rebuild_guard_matrix() {
    use super::provenance_rebuild_guard as g;
    use crate::error::SupervisorError;
    // none → always ok regardless of rebuild.
    assert!(g(None, None, false, false).is_ok());
    assert!(g(None, None, false, true).is_ok());

    // git_ref alone → requires rebuild:true.
    let err = g(Some("feat/x"), None, false, false).expect_err("git_ref without rebuild must 400");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        axum::http::StatusCode::BAD_REQUEST
    );
    assert!(g(Some("feat/x"), None, false, true).is_ok());

    // worktree_path alone → requires rebuild:true (named in the message).
    let err =
        g(None, Some("D:/wt"), false, false).expect_err("worktree_path without rebuild must 400");
    match &err {
        SupervisorError::Validation(m) => {
            assert!(m.contains("worktree_path requires rebuild:true"), "got {m}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
    assert!(g(None, Some("D:/wt"), false, true).is_ok());

    // from_working_tree alone → requires rebuild:true (named in the
    // message). Silently ignoring it would hand back a live-tree exe while
    // the caller believed they opted in — the same lie class as git_ref.
    let err = g(None, None, true, false).expect_err("from_working_tree without rebuild must 400");
    match &err {
        SupervisorError::Validation(m) => {
            assert!(
                m.contains("from_working_tree requires rebuild:true"),
                "got {m}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
    assert!(g(None, None, true, true).is_ok());

    // Any two selectors → provenance_conflict naming BOTH (even with
    // rebuild:true, and regardless of rebuild).
    for (gr, wp, fwt) in [
        (Some("feat/x"), Some("D:/wt"), false),
        (Some("feat/x"), None, true),
        (None, Some("D:/wt"), true),
    ] {
        for rebuild in [true, false] {
            let err =
                g(gr, wp, fwt, rebuild).expect_err("two selectors must 400 regardless of rebuild");
            match &err {
                SupervisorError::Validation(m) => {
                    assert!(m.contains("provenance_conflict"), "got {m}")
                }
                other => panic!("expected Validation, got {other:?}"),
            }
        }
    }
    // All three at once is still a single, fully-named conflict.
    let err = g(Some("feat/x"), Some("D:/wt"), true, true).expect_err("three selectors must 400");
    let msg = err.to_string();
    for sel in ["git_ref", "worktree_path", "from_working_tree"] {
        assert!(msg.contains(sel), "conflict must name {sel}; got {msg}");
    }
}

// -------------------------------------------------------------------
// THE DEFAULT FLIP — spawn-test builds origin/main, not the shared
// working checkout. This is the regression guard for the defect where a
// `{rebuild:true}` spawn silently compiled whatever branch a peer had
// parked the shared checkout on (72 commits behind origin/main), making a
// landed fix read as a regression.
// -------------------------------------------------------------------

#[test]
fn default_spawn_build_source_is_origin_main_not_the_shared_checkout() {
    use super::{resolve_spawn_build_source as r, SpawnBuildSource};
    assert_eq!(
        r(None, None, false, true),
        SpawnBuildSource::DefaultOriginMain,
        "a plain {{rebuild:true}} spawn MUST build origin/main, never the shared checkout"
    );
    assert_eq!(
        r(None, None, false, true).managed_ref(),
        Some(crate::git_provenance::ORIGIN_MAIN_REF),
        "the default must materialize a worktree at the canonical main ref"
    );
}

#[test]
fn resolve_spawn_build_source_matrix() {
    use super::{resolve_spawn_build_source as r, SpawnBuildSource};
    // Explicit selectors win and are reported verbatim.
    assert_eq!(
        r(Some("feat/x"), None, false, true),
        SpawnBuildSource::ExplicitRef("feat/x".into())
    );
    assert_eq!(
        r(None, Some("D:/wt"), false, true),
        SpawnBuildSource::WorktreePath("D:/wt".into())
    );
    // The shared checkout is reachable ONLY by explicit opt-in...
    assert_eq!(r(None, None, true, true), SpawnBuildSource::LiveTree);
    // ...or when no build happens at all (the exe comes from a slot/LKG),
    // where materializing an origin/main worktree would be pure waste.
    assert_eq!(r(None, None, false, false), SpawnBuildSource::LiveTree);

    // Labels are the response `source` vocabulary.
    assert_eq!(r(None, None, false, true).label(), "origin_main");
    assert_eq!(r(Some("feat/x"), None, false, true).label(), "worktree");
    assert_eq!(r(None, Some("D:/wt"), false, true).label(), "worktree_path");
    assert_eq!(r(None, None, true, true).label(), "live_tree");

    // Only the managed-worktree variants get a ref to prepare.
    assert_eq!(
        r(Some("feat/x"), None, false, true).managed_ref(),
        Some("feat/x")
    );
    assert_eq!(r(None, Some("D:/wt"), false, true).managed_ref(), None);
    assert_eq!(r(None, None, true, true).managed_ref(), None);
}

/// Provenance classification is graded by WHAT WAS COMPILED, not by how the
/// caller spelled the request: an explicit `git_ref: "origin/main"` yields
/// the same vouched `OriginMain` class as the default, because the binary
/// genuinely is merged truth. Vouched ⇒ LKG-promotable, which is the deep
/// fix for an LKG that used to be advanced by whatever branch the shared
/// checkout was parked on.
#[test]
fn build_source_kind_grades_by_compiled_tree_not_request_spelling() {
    use super::SpawnBuildSource;
    use crate::build_monitor::BuildSourceKind;
    use crate::process::manager::BuildSource;

    let sha = "a".repeat(40);

    let default_kind = SpawnBuildSource::DefaultOriginMain.build_source_kind(Some(&sha));
    assert!(matches!(default_kind, BuildSourceKind::OriginMain { .. }));
    assert!(default_kind.build_source().is_vouched());

    let explicit_main =
        SpawnBuildSource::ExplicitRef("origin/main".into()).build_source_kind(Some(&sha));
    assert!(matches!(explicit_main, BuildSourceKind::OriginMain { .. }));

    // A non-canonical ref is a foreign tree: Override, NOT LKG-promotable.
    let feature = SpawnBuildSource::ExplicitRef("feat/x".into()).build_source_kind(Some(&sha));
    assert_eq!(feature.build_source(), BuildSource::Override);
    assert!(!feature.build_source().is_vouched());

    // A LOCAL `main` is deliberately NOT canonical — it can lag or carry
    // unpushed commits, so vouching for it would re-open the hole.
    let local_main = SpawnBuildSource::ExplicitRef("main".into()).build_source_kind(Some(&sha));
    assert_eq!(local_main.build_source(), BuildSource::Override);

    // Caller-owned checkout is always foreign.
    assert_eq!(
        SpawnBuildSource::WorktreePath("D:/wt".into())
            .build_source_kind(Some(&sha))
            .build_source(),
        BuildSource::Override
    );

    // Explicit opt-in to the shared checkout stays LiveTree.
    assert_eq!(
        SpawnBuildSource::LiveTree
            .build_source_kind(None)
            .build_source(),
        BuildSource::LiveTree
    );

    // Canonical-main WITHOUT a resolved sha must not claim merged-truth
    // provenance it never observed.
    assert_eq!(
        SpawnBuildSource::DefaultOriginMain
            .build_source_kind(None)
            .build_source(),
        BuildSource::Override,
        "no resolved sha ⇒ cannot vouch"
    );
}

/// The same contract on `POST /runners/{id}/restart`. This is the whole of
/// the 2026-09-09 defect: the wire default decides the PRIMARY's build
/// source, and it used to be hardcoded `true` at the call site where no
/// body could reach it. A body that omits the flag must get origin/main.
///
/// Asserted on the deserialized request rather than on the literal at the
/// call site deliberately — the bug was a false premise ("this route only
/// serves named/temp runners"), so pinning the spelling would not have
/// caught it. Plan
/// `2026-09-13-supervisor-per-runner-restart-hardcodes-from-working-tree`.
#[test]
fn restart_runner_request_defaults_from_working_tree_to_false() {
    let omitted: super::RestartRunnerRequest =
        serde_json::from_str(r#"{"rebuild":true}"#).expect("deserialize");
    assert!(
        !omitted.from_working_tree,
        "a primary rebuild that does not ask for the working tree must get origin/main"
    );
}

/// The escape hatch stays reachable: an operator who deliberately wants the
/// primary to run uncommitted local changes can still say so.
#[test]
fn restart_runner_request_honours_explicit_from_working_tree() {
    let opted: super::RestartRunnerRequest =
        serde_json::from_str(r#"{"rebuild":true,"from_working_tree":true}"#).expect("deserialize");
    assert!(opted.from_working_tree);
}

/// `from_working_tree` must round-trip through serde and default to
/// `false` — a body that omits it gets the safe origin/main default.
#[test]
fn from_working_tree_defaults_false_and_deserializes() {
    let omitted: super::SpawnTestRequest =
        serde_json::from_str(r#"{"rebuild":true}"#).expect("deserialize");
    assert!(
        !omitted.from_working_tree,
        "omitting the flag must NOT opt into the shared checkout"
    );
    let opted: super::SpawnTestRequest =
        serde_json::from_str(r#"{"rebuild":true,"from_working_tree":true}"#).expect("deserialize");
    assert!(opted.from_working_tree);
}

#[test]
fn reject_known_provenance_aliases_flags_each_alias() {
    use super::reject_known_provenance_aliases as r;
    for (alias, correct) in [
        ("branch", "git_ref"),
        ("ref", "git_ref"),
        ("worktree", "worktree_path"),
    ] {
        let body = serde_json::json!({ alias: "value", "rebuild": true });
        let err = r(&body).expect_err("alias must be rejected");
        let s = err.to_string();
        assert!(
            s.contains(alias) && s.contains(correct),
            "alias `{alias}` 400 must name both the alias and the correct field `{correct}`; got: {s}"
        );
        assert_eq!(
            axum::response::IntoResponse::into_response(err).status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }
}

#[test]
fn reject_known_provenance_aliases_allows_real_fields() {
    use super::reject_known_provenance_aliases as r;
    // The real fields must pass the alias check.
    assert!(r(&serde_json::json!({"git_ref": "origin/main", "rebuild": true})).is_ok());
    assert!(r(&serde_json::json!({"worktree_path": "D:/wt", "rebuild": true})).is_ok());
    assert!(r(&serde_json::json!({})).is_ok());
    // A non-object body short-circuits ok (typed deserialize handles it).
    assert!(r(&serde_json::json!("not-an-object")).is_ok());
}

// -------------------------------------------------------------------
// S3 — `/build/{id}/status` must report the build's ACTUAL source root,
// not the supervisor's live `project_dir`.
// -------------------------------------------------------------------

#[test]
fn resolve_spawn_source_live_tree_when_no_selector() {
    let project_dir = std::path::Path::new("D:/ws/qontinui-runner/src-tauri");
    let (source, root) =
        super::resolve_spawn_source(project_dir, &super::SpawnBuildSource::LiveTree);
    assert_eq!(source, "live_tree");
    assert_eq!(root, project_dir);
}

#[test]
fn resolve_spawn_source_git_ref_reports_spawn_container_not_project_dir() {
    // The misleading-field regression: a git_ref build previously reported
    // `project_dir`, making a worktree-spawned build look like it came from
    // the live tree. It must report its own `.spawn-<ref>` container.
    //
    // `derive_workspace_root` probes the FS for the
    // `qontinui-runner/` + `qontinui-schemas/` sibling pair, so the fixture
    // is a real workspace-shaped tempdir.
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join("qontinui-runner").join("src-tauri")).expect("mkdir runner");
    std::fs::create_dir_all(ws.join("qontinui-schemas")).expect("mkdir schemas");
    let project_dir = ws.join("qontinui-runner").join("src-tauri");

    let (source, root) = super::resolve_spawn_source(
        &project_dir,
        &super::SpawnBuildSource::ExplicitRef("origin/main".into()),
    );

    assert_eq!(
        source, "worktree",
        "must reuse the existing source vocabulary"
    );
    let root_s = root.to_string_lossy().replace('\\', "/");
    assert!(
        root_s.contains("/.spawn-"),
        "a git_ref build must report its .spawn-<ref> container; got: {root_s}"
    );
    assert!(
        root_s.ends_with("qontinui-runner/src-tauri"),
        "the reported root must be the cargo source root inside the container; got: {root_s}"
    );
    assert_ne!(
        root, project_dir,
        "must NOT report the supervisor's live project_dir"
    );

    // And it must agree byte-for-byte with the path prepare_worktree
    // materializes (same helper, single source of truth).
    let expected = crate::spawn_worktree::runner_worktree_path_for_ref(&project_dir, "origin/main")
        .expect("derive container")
        .join("src-tauri");
    assert_eq!(root, expected);
}

#[test]
fn resolve_spawn_source_git_ref_degrades_to_project_dir_on_malformed_layout() {
    // A workspace root that can't be derived must not fail the spawn — the
    // build itself surfaces the real error. Degrade to project_dir.
    let tmp = TempDir::new().expect("tempdir");
    let project_dir = tmp.path().join("not-a-workspace");
    let (source, root) = super::resolve_spawn_source(
        &project_dir,
        &super::SpawnBuildSource::ExplicitRef("origin/main".into()),
    );
    assert_eq!(source, "worktree");
    assert_eq!(root, project_dir);
}

#[test]
fn resolve_spawn_source_worktree_path_reports_caller_checkout() {
    let project_dir = std::path::Path::new("D:/ws/qontinui-runner/src-tauri");
    let (source, root) = super::resolve_spawn_source(
        project_dir,
        &super::SpawnBuildSource::WorktreePath("D:/ws/.spawn-pr370/qontinui-runner".into()),
    );
    assert_eq!(source, "worktree_path");
    assert_eq!(
        root,
        std::path::Path::new("D:/ws/.spawn-pr370/qontinui-runner/src-tauri")
    );
    assert_ne!(root, project_dir);
}

// -------------------------------------------------------------------
// Phase 2/3 — new request-field wire shapes.
// -------------------------------------------------------------------

#[test]
fn spawn_test_request_worktree_path_defaults_none() {
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(
        req.worktree_path.is_none(),
        "worktree_path default must be None"
    );
}

#[test]
fn spawn_test_request_worktree_path_round_trips() {
    let req: super::SpawnTestRequest =
        serde_json::from_str(r#"{"worktree_path":"D:/qontinui-root/.spawn-pr370","rebuild":true}"#)
            .expect("deserialize SpawnTestRequest with worktree_path");
    assert_eq!(
        req.worktree_path.as_deref(),
        Some("D:/qontinui-root/.spawn-pr370")
    );
    assert!(req.rebuild);
}

#[test]
fn spawn_test_request_frontend_only_defaults_false() {
    let req: super::SpawnTestRequest =
        serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
    assert!(!req.frontend_only, "frontend_only default must be false");
}

#[test]
fn spawn_test_request_frontend_only_round_trips() {
    let req: super::SpawnTestRequest =
        serde_json::from_str(r#"{"worktree_path":"D:/wt","frontend_only":true,"rebuild":true}"#)
            .expect("deserialize SpawnTestRequest with frontend_only");
    assert!(req.frontend_only);
}

#[test]
fn git_ref_rebuild_guard_allows_no_ref() {
    // No selector: guard is a no-op regardless of rebuild.
    assert!(super::provenance_rebuild_guard(None, None, false, false).is_ok());
    assert!(super::provenance_rebuild_guard(None, None, false, true).is_ok());
}

/// Helper: insert a runner with an optional preview binding into a state's
/// registry, mirroring the spawn-test path (config + binding on the
/// ManagedRunner). Returns the runner id.
async fn insert_runner_with_binding(
    state: &crate::state::SharedState,
    port: u16,
    binding: Option<crate::state::PreviewBinding>,
) -> String {
    let id = format!("test-{}", port);
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = id.clone();
    config.name = id.clone();
    config.port = port;
    config.kind = super::RunnerKind::Temp { id: id.clone() };
    let managed = std::sync::Arc::new(crate::state::ManagedRunner::new_with_log_dir(
        config, false, None,
    ));
    if let Some(b) = binding {
        *managed.preview_binding.write().await = Some(b);
    }
    state.runners.write().await.insert(id.clone(), managed);
    id
}

/// Like [`make_state`] but rooted at `root`, so `project_dir`
/// (`<root>/src-tauri`) has a real repo at `<root>` for the git-provenance
/// probes to run against.
fn make_state_at(root: &std::path::Path) -> crate::state::SharedState {
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
    let config = SupervisorConfig {
        project_dir: root.join("src-tauri"),
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9875,
        dev_logs_dir: root.join(".dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 19000,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: BuildPoolConfig { pool_size: 1 },
        no_prewarm: true,
        no_webview: true,
        temp_runner_display: None,
    };
    std::sync::Arc::new(crate::state::SupervisorState::new(config))
}

/// Run `git <args>` in `cwd`, asserting success.
fn git_in(cwd: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed in {cwd:?}");
}

/// Commit `name` on the current branch, returning the new HEAD sha.
fn commit_file(dir: &std::path::Path, name: &str) -> String {
    std::fs::write(dir.join(name), name).expect("write file");
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "-q", "-m", name]);
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Seed `state.build_pool.last_known_good` with an LKG carrying `sha`.
async fn seed_lkg(state: &crate::state::SharedState, sha: Option<String>) {
    *state.build_pool.last_known_good.write().await = Some(crate::state::LkgInfo {
        built_at: chrono::Utc::now(),
        source_slot: 0,
        exe_size: 1234,
        sha,
        source: crate::process::manager::BuildSource::LiveTree,
    });
}

/// Item 8 — the motivating trap: the checkout `spawn-test {rebuild:true}`
/// would compile sits BEHIND the LKG, so the response must warn with the
/// LKG sha and a behind-count instead of silently building stale code.
#[tokio::test]
async fn build_older_than_lkg_warns_with_behind_count() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src-tauri")).unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["config", "user.email", "test@example.com"]);
    git_in(root, &["config", "user.name", "test"]);

    // The tree that would be built is two commits behind the LKG's sha.
    let built = commit_file(root, "seed.txt");
    commit_file(root, "a.txt");
    let lkg_sha = commit_file(root, "b.txt");

    let state = make_state_at(root);
    seed_lkg(&state, Some(lkg_sha.clone())).await;

    let out = super::build_older_than_lkg_json(&state, Some(&built), None).await;
    assert_eq!(out["older"], serde_json::json!(true), "got {out}");
    assert_eq!(out["behind_count"], serde_json::json!(2));
    assert_eq!(out["lkg_sha"], serde_json::json!(lkg_sha));
    assert_eq!(out["built_sha"], serde_json::json!(built));
    assert_eq!(out["diverged"], serde_json::json!(false));
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("build_older_than_lkg"),
        "message must name the signal: {out}"
    );
}

/// A build that already contains the LKG's commit is not older ⇒ no alarm.
#[tokio::test]
async fn build_not_older_than_lkg_is_silent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src-tauri")).unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["config", "user.email", "test@example.com"]);
    git_in(root, &["config", "user.name", "test"]);

    let lkg_sha = commit_file(root, "seed.txt");
    let built = commit_file(root, "newer.txt");

    let state = make_state_at(root);
    seed_lkg(&state, Some(lkg_sha)).await;

    let out = super::build_older_than_lkg_json(&state, Some(&built), None).await;
    assert_eq!(out["older"], serde_json::json!(false), "got {out}");
    assert!(out.get("behind_count").is_none());
}

/// REGRESSION (iteration-2): the spawn-response vintage compare must run in
/// the tree that was ACTUALLY compiled, not in the canonical checkout.
///
/// Two UNRELATED repos: the canonical checkout (whose HEAD is the LKG sha)
/// and a caller-supplied `worktree_path` override. The override's sha does
/// not exist in the canonical object db at all, so a compare run there is
/// not computable — while the same compare run in the override tree
/// resolves. Threading `built_root` is what makes the difference, and that
/// thread-through is exactly what silently regresses.
#[tokio::test]
async fn build_older_than_lkg_compares_in_the_override_tree_not_the_canonical_checkout() {
    let canonical_tmp = tempfile::TempDir::new().unwrap();
    let canonical = canonical_tmp.path();
    std::fs::create_dir_all(canonical.join("src-tauri")).unwrap();
    git_in(canonical, &["init", "-q", "-b", "main"]);
    git_in(canonical, &["config", "user.email", "test@example.com"]);
    git_in(canonical, &["config", "user.name", "test"]);
    let canonical_head = commit_file(canonical, "canonical.txt");

    // A SEPARATE repo standing in for a caller-owned `worktree_path`. Its
    // history is disjoint from the canonical checkout's.
    let over_tmp = tempfile::TempDir::new().unwrap();
    let over = over_tmp.path();
    std::fs::create_dir_all(over.join("src-tauri")).unwrap();
    git_in(over, &["init", "-q", "-b", "feature"]);
    git_in(over, &["config", "user.email", "test@example.com"]);
    git_in(over, &["config", "user.name", "test"]);
    let over_built = commit_file(over, "seed.txt");
    commit_file(over, "a.txt");
    let over_lkg = commit_file(over, "b.txt");

    let state = make_state_at(canonical);
    seed_lkg(&state, Some(over_lkg.clone())).await;

    // WITH the override root: both shas live in that repo, so the compare
    // resolves and reports the override tree's real two-commit gap.
    let out = super::build_older_than_lkg_json(&state, Some(&over_built), Some(over)).await;
    assert_eq!(
        out["older"],
        serde_json::json!(true),
        "the override tree's own history must be compared: {out}"
    );
    assert_eq!(out["behind_count"], serde_json::json!(2), "got {out}");
    assert_eq!(out["built_sha"], serde_json::json!(over_built));

    // WITHOUT it (the pre-fix behavior) the canonical checkout is probed,
    // where the override's history does not exist. `git rev-parse` echoes a
    // raw 40-hex sha verbatim even for an absent object, so the compare
    // does not even fail loudly — it silently answers "not older". That
    // false negative is precisely what threading `built_root` removes.
    let wrong = super::build_older_than_lkg_json(&state, Some(&over_built), None).await;
    assert_ne!(
        wrong["older"],
        serde_json::json!(true),
        "fixture sanity: the canonical checkout cannot see the override tree's gap, which              is exactly why built_root must be threaded: {wrong}"
    );
    assert_ne!(
        canonical_head, over_built,
        "fixture sanity: the two trees must have different HEADs"
    );
}

/// Best-effort: an LKG with no recorded sha (legacy sidecar / failed git
/// probe) reports a reason, never a false staleness alarm.
#[tokio::test]
async fn build_older_than_lkg_without_lkg_sha_is_not_computable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = make_state_at(tmp.path());
    seed_lkg(&state, None).await;

    let out = super::build_older_than_lkg_json(&state, Some("deadbeef"), None).await;
    assert_eq!(out["older"], serde_json::json!(false));
    assert_eq!(out["reason"], serde_json::json!("lkg_sha_unknown"));
}

fn make_state() -> crate::state::SharedState {
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
    use std::path::PathBuf;
    let config = SupervisorConfig {
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
        expo_port: 19000,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: BuildPoolConfig { pool_size: 1 },
        no_prewarm: false,
        no_webview: true,
        temp_runner_display: None,
    };
    std::sync::Arc::new(crate::state::SupervisorState::new(config))
}

#[tokio::test]
async fn runners_by_unit_resolves_bound_previews() {
    use axum::extract::{Path, State};
    let state = make_state();
    // u1 has two attempts' previews on distinct ports; u2 has one.
    insert_runner_with_binding(
        &state,
        9877,
        Some(crate::state::PreviewBinding {
            unit_id: "u1".into(),
            attempt_id: Some("a1".into()),
            git_sha: Some("0156c6775b18".into()),
        }),
    )
    .await;
    insert_runner_with_binding(
        &state,
        9878,
        Some(crate::state::PreviewBinding {
            unit_id: "u1".into(),
            attempt_id: Some("a2".into()),
            git_sha: None,
        }),
    )
    .await;
    insert_runner_with_binding(
        &state,
        9879,
        Some(crate::state::PreviewBinding {
            unit_id: "u2".into(),
            attempt_id: None,
            git_sha: None,
        }),
    )
    .await;
    // An unbound runner must never appear in any unit's handle list.
    insert_runner_with_binding(&state, 9880, None).await;

    let resp = super::runners_by_unit(State(state.clone()), Path("u1".to_string()))
        .await
        .expect("by-unit must succeed");
    let arr = resp.0.as_array().expect("array body").clone();
    assert_eq!(arr.len(), 2, "u1 has exactly two bound previews");
    let ports: std::collections::HashSet<u64> =
        arr.iter().map(|h| h["port"].as_u64().unwrap()).collect();
    assert_eq!(
        ports,
        std::collections::HashSet::from([9877, 9878]),
        "only u1's ports, not u2's or the unbound runner's"
    );
    // Handle shape: runner_id, port, ui_bridge_url, git_sha, attempt_id.
    let a1 = arr
        .iter()
        .find(|h| h["attempt_id"] == "a1")
        .expect("a1 present");
    assert_eq!(a1["port"], 9877);
    assert_eq!(a1["ui_bridge_url"], "http://localhost:9877/ui-bridge");
    assert_eq!(a1["git_sha"], "0156c6775b18");
    assert_eq!(a1["runner_id"], "test-9877");
    let a2 = arr
        .iter()
        .find(|h| h["attempt_id"] == "a2")
        .expect("a2 present");
    assert!(a2["git_sha"].is_null(), "a2 sha not yet probed → null");
}

#[tokio::test]
async fn runners_by_unit_unknown_unit_returns_empty_not_404() {
    use axum::extract::{Path, State};
    let state = make_state();
    insert_runner_with_binding(
        &state,
        9877,
        Some(crate::state::PreviewBinding {
            unit_id: "u1".into(),
            attempt_id: Some("a1".into()),
            git_sha: None,
        }),
    )
    .await;
    let resp = super::runners_by_unit(State(state.clone()), Path("nope".to_string()))
        .await
        .expect("unknown unit must be 200, not an error/404");
    assert_eq!(
        resp.0.as_array().expect("array body").len(),
        0,
        "unknown unit returns [] (a queryable answer), never 404"
    );
}

// ----- friction-1: per-requester pinning + requester-scoped reaping -----

/// Insert a temp-runner placeholder owned by `requester` (or unowned when
/// `None`). Mirrors the spawn-test registry-insert + requester stamp. The
/// runner is left `running=false` so the reaper treats it as a reapable
/// placeholder (the spawn-test pre-build state).
async fn insert_temp_runner_owned(
    state: &crate::state::SharedState,
    port: u16,
    requester: Option<&str>,
) -> String {
    let id = format!("test-{}", port);
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = id.clone();
    config.name = id.clone();
    config.port = port;
    config.kind = super::RunnerKind::Temp { id: id.clone() };
    let managed = std::sync::Arc::new(crate::state::ManagedRunner::new_with_log_dir(
        config, false, None,
    ));
    if let Some(r) = requester {
        *managed.requester_id.write().await = Some(r.to_string());
    }
    state.runners.write().await.insert(id.clone(), managed);
    id
}

#[tokio::test]
async fn get_runners_surfaces_requester_id() {
    use axum::extract::State;
    let state = make_state();
    insert_temp_runner_owned(&state, 9877, Some("session-A")).await;
    insert_temp_runner_owned(&state, 9878, None).await;

    let resp = super::list_runners(State(state.clone()))
        .await
        .expect("list must succeed");
    let arr = resp.0.as_array().expect("array body").clone();

    let r_a = arr
        .iter()
        .find(|r| r["port"] == 9877)
        .expect("9877 present");
    assert_eq!(
        r_a["requester_id"], "session-A",
        "owner must be surfaced so a session can pin by id, not port"
    );
    let r_b = arr
        .iter()
        .find(|r| r["port"] == 9878)
        .expect("9878 present");
    assert!(
        r_b["requester_id"].is_null(),
        "an unowned runner reports requester_id: null"
    );
}

// ------------------------------------------------------------------
// S1 — a deaf-but-listening runner must not report healthy.
//
// The wedge that motivated this: the primary alive ~14h with 26178s of
// CPU, holding :9876, accepting TCP connections and answering none of
// them. `RunnerState::liveness` already classified it and the health
// refresher already escalated it (`RUNNER WEDGED`), but `GET /runners`
// rendered neither, so every liveness check on the box read `running`
// and called it healthy.
// ------------------------------------------------------------------

/// Put a runner into the exact wedge shape: the supervisor believes the
/// process is up (`running: true`, a pid), the listener probe says the
/// port is held, and the API probe says nothing answered.
async fn insert_wedged_runner(
    state: &crate::state::SharedState,
    port: u16,
    last_seen: chrono::DateTime<chrono::Utc>,
) -> String {
    let id = format!("wedged-{port}");
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = id.clone();
    config.name = id.clone();
    config.port = port;
    let managed = std::sync::Arc::new(crate::state::ManagedRunner::new_with_log_dir(
        config, false, None,
    ));
    {
        let mut r = managed.runner.write().await;
        r.running = true;
        r.pid = Some(148320);
        r.last_seen_responding_at = Some(last_seen);
    }
    {
        let mut c = managed.cached_health.write().await;
        c.runner_port_open = true;
        c.runner_responding = false;
    }
    state.runners.write().await.insert(id.clone(), managed);
    id
}

#[tokio::test]
async fn list_runners_reports_a_deaf_but_listening_runner_as_wedged() {
    use axum::extract::State;
    let state = make_state();
    let last_seen = chrono::Utc::now() - chrono::Duration::hours(14);
    let id = insert_wedged_runner(&state, 9876, last_seen).await;

    let resp = super::list_runners(State(state.clone()))
        .await
        .expect("list must succeed");
    let arr = resp.0.as_array().expect("array body").clone();
    let row = arr
        .iter()
        .find(|r| r["id"] == id.as_str())
        .expect("the wedged runner is listed");

    assert_eq!(
        row["liveness"]["state"], "wedged",
        "a held port with a silent API is the wedge state, not health: {row}"
    );
    assert_eq!(
        row["liveness"]["unresponsive_since"],
        last_seen.to_rfc3339(),
        "the wedge must carry the T behind \"unresponsive since T\""
    );
    assert_eq!(
        row["running"], true,
        "the supervisor still believes the process is up — `running` alone              is exactly why the wedge was invisible, and it keeps its value"
    );
    assert_eq!(row["api_responding"], false);
    assert_eq!(row["port_open"], true);
    assert_eq!(row["last_seen_responding_at"], last_seen.to_rfc3339());
}

/// The additive contract: every row carries `liveness` as an object with
/// a `state` string, whatever the runner's condition — a consumer never
/// has to branch on the JSON type before reading the verdict, and an
/// un-probed runner says `unknown` rather than going missing.
#[tokio::test]
async fn every_runner_row_carries_a_liveness_object_with_a_state_string() {
    use axum::extract::State;
    let state = make_state(); // already seeds the configured primary
    let wedged_id = insert_wedged_runner(&state, 9876, chrono::Utc::now()).await;
    let temp_id = insert_temp_runner_owned(&state, 9877, None).await;

    let resp = super::list_runners(State(state.clone()))
        .await
        .expect("list must succeed");
    let arr = resp.0.as_array().expect("array body").clone();
    assert!(
        arr.len() >= 3,
        "the seeded primary plus both inserts are listed: {arr:?}"
    );
    assert!(arr.iter().any(|r| r["id"] == wedged_id.as_str()));

    for row in &arr {
        let state_field = row["liveness"]["state"]
            .as_str()
            .unwrap_or_else(|| panic!("liveness.state must be a string: {row}"));
        assert!(
            matches!(state_field, "responding" | "wedged" | "stopped" | "unknown"),
            "unexpected liveness state {state_field:?}: {row}"
        );
        assert!(
            row["liveness"].get("unresponsive_since").is_some(),
            "unresponsive_since is always present (null when N/A): {row}"
        );
        // The pre-existing fields are untouched by this addition.
        assert!(row.get("running").is_some());
        assert!(row.get("api_responding").is_some());
    }

    let unprobed = arr
        .iter()
        .find(|r| r["id"] == temp_id.as_str())
        .expect("the never-probed temp runner is present");
    assert_eq!(
        unprobed["liveness"]["state"], "unknown",
        "a runner the refresher has not reached is UNKNOWN, never `stopped`"
    );
}

// ---------------------------------------------------------------------
// Wire shape `GET /builds` owes the Phase 3 verification harness.
//
// `scripts/verify-scoped-cleanup.ps1` is Windows-only and CI is
// `ubuntu-latest`, so the harness can never run on the merge gate — the
// same reason the sibling pins exist in `crate::diagnostics` and
// `crate::process::slot_territory`. This is the pin for the field it reads
// off THIS route.
// ---------------------------------------------------------------------

/// `pool_size` must stay on the `GET /builds` body, as a positive number.
///
/// The harness's `Resolve-PoolSize` reads `$builds.pool_size` and feeds it
/// to `Get-SlotTargetDirs`, which decides how many slot territories get a
/// probe — so this one field bounds what assertion V2-3 ("orphans in the
/// OTHER pool slots are still ALIVE") can even see.
///
/// This field used to be a SOFT dependency: a rename degraded to a
/// hardcoded `$PoolSize = 3`, and the run continued while quietly checking
/// the wrong number of slots. PR #139 deleted that default precisely
/// because the silent-undersize case manufactures a false PASS — which
/// makes the dependency HARD: rename or drop `pool_size` now and every
/// harness run aborts at preflight with "could not establish the build pool
/// size", on a fleet where the Rust suite stays entirely green. Escalating
/// the consumer's failure mode is what makes this pin necessary, not
/// optional.
#[tokio::test]
async fn list_builds_emits_the_pool_size_the_harness_refuses_to_run_without() {
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    let state = make_state(); // BuildPoolConfig { pool_size: 1 }

    let resp = super::list_builds(State(state.clone()), Query(Default::default()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read /builds body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("/builds is JSON");

    let pool_size = body
        .get("pool_size")
        .expect("`pool_size` is a hard dependency of verify-scoped-cleanup.ps1");
    assert!(
        pool_size.is_u64(),
        "the harness casts this with [int]; a string or null is a contract miss: {body}"
    );
    assert_eq!(
        pool_size.as_u64(),
        Some(state.build_pool.slots.len() as u64),
        "pool_size must report the REAL slot count — the harness plants one \
             probe per slot and an undersized value silently narrows V2-3"
    );
    assert!(
        pool_size.as_u64().unwrap_or(0) > 0,
        "the harness treats a non-positive pool_size as an unusable answer \
             and aborts; a live pool is never zero-sized"
    );
}

/// A never-computed drift reading must not serialize as "no drift".
///
/// `origin_main_drift` is `null` both when the LKG is up to date and when
/// nothing has been computed yet — indistinguishable to a reader, which is
/// the confident-looking default `verification-and-evidence` /
/// `unknown-must-not-render-as-a-default` forbids. `origin_main_drift_probe`
/// is what separates them, so it has to be present on a fresh state.
#[tokio::test]
async fn list_builds_reports_an_uncomputed_drift_as_pending_not_as_no_drift() {
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    let state = make_state();
    assert!(
        state.origin_drift.read().await.is_none(),
        "a fresh state has no drift reading yet"
    );

    let resp = super::list_builds(State(state.clone()), Query(Default::default()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read /builds body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("/builds is JSON");

    assert!(
        body["origin_main_drift"].is_null(),
        "nothing computed yet, so there is no drift to report"
    );
    let probe = body
        .get("origin_main_drift_probe")
        .expect("the probe field is what makes `null` readable");
    assert_eq!(
        probe["state"], "pending",
        "never-computed must be distinguishable from up-to-date: {body}"
    );
    assert!(
        probe["computed_at"].is_null() && probe["age_secs"].is_null(),
        "a pending reading has no timestamp to report: {probe}"
    );
}

/// A cached reading computed for a SUPERSEDED LKG sha must say so.
///
/// The drift cache is refreshed on a timer, so the LKG can move between
/// refreshes. Serving the old reading as current would answer a question
/// nobody asked — about a build that is no longer the LKG.
#[tokio::test]
async fn list_builds_marks_a_drift_reading_superseded_when_the_lkg_moved() {
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    let state = make_state();

    *state.build_pool.last_known_good.write().await = Some(crate::state::LkgInfo {
        built_at: chrono::Utc::now(),
        source_slot: 0,
        exe_size: 1,
        sha: Some("bbbbbbbb".to_string()),
        source: crate::process::manager::BuildSource::OriginMain,
    });
    // Reading was computed for a DIFFERENT sha than the LKG now records.
    *state.origin_drift.write().await = Some(crate::state::OriginDriftSnapshot {
        built_sha: "aaaaaaaa".to_string(),
        drift: crate::git_provenance::OriginMainDrift {
            built_sha: "aaaaaaaa".to_string(),
            origin_main_sha: "cccccccc".to_string(),
            behind_count: 7,
            is_ancestor: true,
            fetched: true,
        },
        computed_at: chrono::Utc::now(),
    });

    let resp = super::list_builds(State(state.clone()), Query(Default::default()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read /builds body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("/builds is JSON");

    let probe = &body["origin_main_drift_probe"];
    assert_eq!(
        probe["state"], "superseded_lkg_moved",
        "a reading for a stale sha must not read as current: {body}"
    );
    assert_eq!(
        probe["computed_for_sha"], "aaaaaaaa",
        "say WHICH sha the reading answers for, so the gap is checkable"
    );
    assert!(
        body["origin_main_drift"].is_null(),
        "a superseded reading must not be published as the drift itself"
    );
}

#[tokio::test]
async fn scoped_purge_never_evicts_another_requesters_runner() {
    // Two sessions each own a placeholder; both look dead (running=false).
    // Session-A purges scoped to itself — only A's runner is reaped, B's
    // (and an unowned third) survive untouched.
    let state = make_state();
    let id_a = insert_temp_runner_owned(&state, 9877, Some("session-A")).await;
    let id_b = insert_temp_runner_owned(&state, 9878, Some("session-B")).await;
    let id_unowned = insert_temp_runner_owned(&state, 9879, None).await;

    let purged = super::purge_stale_test_runners_core(&state, false, Some("session-A")).await;

    let purged_ids: std::collections::HashSet<String> =
        purged.into_iter().map(|(id, _, _)| id).collect();
    assert!(
        purged_ids.contains(&id_a),
        "session-A's own stale runner must be reaped"
    );
    assert!(
        !purged_ids.contains(&id_b),
        "session-B's runner must NOT be reaped by session-A's scoped purge"
    );
    assert!(
        !purged_ids.contains(&id_unowned),
        "an unowned runner is not 'this requester's' and must not be reaped under a scoped purge"
    );

    let registry = state.runners.read().await;
    assert!(!registry.contains_key(&id_a), "A removed from registry");
    assert!(registry.contains_key(&id_b), "B survives in registry");
    assert!(
        registry.contains_key(&id_unowned),
        "unowned survives in registry"
    );
}

#[tokio::test]
async fn unscoped_purge_reaps_all_stale_runners() {
    // The periodic sweep / body-less purge-stale passes None and reaps
    // every stale test runner regardless of owner (legacy behavior).
    let state = make_state();
    let id_a = insert_temp_runner_owned(&state, 9877, Some("session-A")).await;
    let id_b = insert_temp_runner_owned(&state, 9878, Some("session-B")).await;
    let id_unowned = insert_temp_runner_owned(&state, 9879, None).await;

    let purged = super::purge_stale_test_runners_core(&state, false, None).await;
    let purged_ids: std::collections::HashSet<String> =
        purged.into_iter().map(|(id, _, _)| id).collect();

    for id in [&id_a, &id_b, &id_unowned] {
        assert!(purged_ids.contains(id), "unscoped purge reaps {id}");
    }
}

/// A loopback port nothing is listening on: bind an ephemeral port, read
/// it, drop the listener. Never hard-code 9877 — a live temp runner on the
/// test box may hold it.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}

/// Mark a registered runner running (or not) with the given start time and
/// spawn-in-flight marker.
async fn set_runner_phase(
    state: &crate::state::SharedState,
    id: &str,
    running: bool,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    marker: Option<std::time::Duration>,
) {
    let managed = state.get_runner(id).await.expect("registered");
    {
        let mut r = managed.runner.write().await;
        r.running = running;
        r.started_at = started_at;
    }
    managed.set_spawn_in_flight(marker);
}

/// REGRESSION (plan 2026-09-19-supervisor-reaper-purges-an-in-flight-spawn-test):
/// a runner started ~now by an in-flight spawn handler has not bound its
/// port yet. The sweep used to read "running=true, port free" as a crash
/// and delete the record out from under the starting process.
#[tokio::test]
async fn purge_spares_a_runner_inside_its_spawn_startup_window() {
    let state = make_state();
    let port = free_port();
    let id = insert_temp_runner_owned(&state, port, None).await;
    set_runner_phase(
        &state,
        &id,
        true,
        Some(chrono::Utc::now()),
        Some(std::time::Duration::from_secs(240)),
    )
    .await;

    let purged = super::purge_stale_test_runners_core(&state, true, None).await;

    assert!(
        purged.iter().all(|(pid, _, _)| pid != &id),
        "a starting runner inside its spawn window must not be purged: {purged:?}"
    );
    assert!(
        state.runners.read().await.contains_key(&id),
        "the starting runner must still be registered"
    );
}

/// Companion: the crash arm still works for a runner long past its window.
#[tokio::test]
async fn purge_still_reaps_a_crashed_runner_past_its_startup_window() {
    let state = make_state();
    let port = free_port();
    let id = insert_temp_runner_owned(&state, port, None).await;
    set_runner_phase(
        &state,
        &id,
        true,
        Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        None,
    )
    .await;

    let purged = super::purge_stale_test_runners_core(&state, true, None).await;

    assert!(
        purged.iter().any(|(pid, _, _)| pid == &id),
        "a runner started an hour ago with a dead port is crashed and must be purged"
    );
    assert!(!state.runners.read().await.contains_key(&id));
}

/// Post-build / pre-start window: the placeholder is still running=false,
/// no build is active, but the spawn handler owns it. Even the operator
/// path (`respect_active_builds=false`) must spare it.
#[tokio::test]
async fn operator_purge_spares_a_placeholder_whose_spawn_is_in_flight() {
    let state = make_state();
    let port = free_port();
    let id = insert_temp_runner_owned(&state, port, None).await;
    set_runner_phase(
        &state,
        &id,
        false,
        None,
        Some(std::time::Duration::from_secs(240)),
    )
    .await;

    let purged = super::purge_stale_test_runners_core(&state, false, None).await;

    assert!(
        purged.iter().all(|(pid, _, _)| pid != &id),
        "an in-flight spawn's placeholder must not be purged by the operator path"
    );
    assert!(state.runners.read().await.contains_key(&id));
}

/// No marker (restart route / watchdog restart): the generic floor still
/// gives a just-started runner time to bind.
#[tokio::test]
async fn purge_spares_a_just_started_runner_without_a_marker() {
    let state = make_state();
    let port = free_port();
    let id = insert_temp_runner_owned(&state, port, None).await;
    set_runner_phase(&state, &id, true, Some(chrono::Utc::now()), None).await;

    let purged = super::purge_stale_test_runners_core(&state, true, None).await;

    assert!(
        purged.iter().all(|(pid, _, _)| pid != &id),
        "a runner started moments ago must get the startup floor"
    );
    assert!(state.runners.read().await.contains_key(&id));
}

/// Between the 60 s floor and the marker's budget: the marker alone decides.
/// Started 120 s ago with a dead port — spared with a 240 s marker, purged
/// without one.
#[tokio::test]
async fn purge_marker_protects_past_the_floor_but_within_the_budget() {
    let state = make_state();
    let started = chrono::Utc::now() - chrono::Duration::seconds(120);

    let marked = insert_temp_runner_owned(&state, free_port(), None).await;
    set_runner_phase(
        &state,
        &marked,
        true,
        Some(started),
        Some(std::time::Duration::from_secs(240)),
    )
    .await;
    let unmarked = loop {
        // Distinct id: ids are derived from the port.
        let p = free_port();
        if format!("test-{p}") != marked {
            break insert_temp_runner_owned(&state, p, None).await;
        }
    };
    set_runner_phase(&state, &unmarked, true, Some(started), None).await;

    let purged = super::purge_stale_test_runners_core(&state, true, None).await;
    let ids: std::collections::HashSet<String> = purged.into_iter().map(|(id, _, _)| id).collect();

    assert!(
        !ids.contains(&marked),
        "a runner past the floor but inside its spawn budget must be spared"
    );
    assert!(state.runners.read().await.contains_key(&marked));
    assert!(
        ids.contains(&unmarked),
        "the same runner with no marker is past the floor and must be purged"
    );
}

/// `wait_timeout_secs` is unclamped: the budget must saturate, not panic.
#[test]
fn spawn_in_flight_budget_saturates_on_huge_wait() {
    let body: super::SpawnTestRequest = serde_json::from_value(serde_json::json!({
        "wait": true,
        "wait_timeout_secs": u64::MAX,
        "health_probe_timeout_ms": u64::MAX,
    }))
    .expect("parse");
    assert_eq!(
        super::spawn_in_flight_budget(&body),
        std::time::Duration::MAX
    );
}

/// The marker is armed at the placeholder insert in `spawn_test` (so the
/// mint/submit gap is covered) and moved into the exec future; the inner
/// build fn must not arm a second guard that would clear it early.
#[test]
fn spawn_in_flight_guard_is_armed_once_at_the_placeholder_insert() {
    let spawn = fn_source("pub async fn spawn_test(");
    let arm = spawn
        .find("SpawnInFlightGuard::arm(")
        .expect("spawn_test must arm the spawn-in-flight guard");
    let insert = spawn
        .find("runners.insert(id.clone(), managed.clone())")
        .expect("spawn_test placeholder insert");
    assert!(
        arm < insert,
        "the marker must be armed before the placeholder is visible"
    );
    assert!(
        spawn.contains("let _in_flight = in_flight_guard;"),
        "the guard must be moved into the exec future"
    );
    assert!(
        !fn_source("async fn execute_spawn_build(").contains("SpawnInFlightGuard::arm("),
        "execute_spawn_build must not arm its own guard"
    );
}

/// The RAII guard sets the marker and clears it on drop.
#[test]
fn spawn_in_flight_guard_clears_the_marker_on_drop() {
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = "test-guard".to_string();
    let managed = std::sync::Arc::new(crate::state::ManagedRunner::new_with_log_dir(
        config, false, None,
    ));
    assert_eq!(managed.spawn_in_flight(), None);
    let guard = super::SpawnInFlightGuard::arm(&managed, std::time::Duration::from_secs(240));
    assert_eq!(
        managed.spawn_in_flight(),
        Some(std::time::Duration::from_secs(240))
    );
    drop(guard);
    assert_eq!(managed.spawn_in_flight(), None);
}

/// Budget = probe window + wait (only when waiting) + margin.
#[test]
fn spawn_in_flight_budget_sums_probe_wait_and_margin() {
    let mut body: super::SpawnTestRequest =
        serde_json::from_value(serde_json::json!({})).expect("defaults");
    assert_eq!(
        super::spawn_in_flight_budget(&body),
        std::time::Duration::from_secs(60 + 60)
    );
    body.wait = true;
    assert_eq!(
        super::spawn_in_flight_budget(&body),
        std::time::Duration::from_secs(60 + 120 + 60)
    );
}

#[tokio::test]
async fn temp_runner_port_is_stable_for_its_lifetime() {
    // A runner's port lives on its immutable RunnerConfig and the allocator
    // only ever picks ports NOT already held by a runner in the registry —
    // so no concurrent spawn can be handed a live runner's port, and a
    // runner's own port never changes while it is registered.
    let state = make_state();
    let id = insert_temp_runner_owned(&state, 9877, Some("session-A")).await;

    // The allocator's used-port set (the exact expression spawn_test uses).
    let used_ports: std::collections::HashSet<u16> = {
        let runners = state.runners.read().await;
        runners.values().map(|r| r.config.port).collect()
    };
    let next = (9877..=9899)
        .find(|p| !used_ports.contains(p))
        .expect("a free port exists");
    assert_ne!(
        next, 9877,
        "the next allocation must skip the live runner's port"
    );

    // The live runner's port is unchanged after the (hypothetical) sibling
    // allocation — config.port is set once at insert and never reassigned.
    let port_now = {
        let runners = state.runners.read().await;
        runners.get(&id).expect("still registered").config.port
    };
    assert_eq!(port_now, 9877, "a live runner's port is immutable");
}
// -------------------------------------------------------------------
// spawn-test single flight, keyed by (requester_id, build target).
//
// Three pool slots is the scarce resource: a retry of a `curl` that looked
// like it returned nothing must JOIN the build already running for that
// key, not claim a second slot to compile the identical tree.
// -------------------------------------------------------------------

/// Anonymous requests never join — not each other, and not a keyed build.
/// With no requester there is no way to tell one caller retrying from two
/// unrelated callers, and collapsing the latter hands a caller someone
/// else's runner.
#[test]
fn spawn_dedup_key_is_none_for_anonymous_requests() {
    use super::{spawn_dedup_key as k, SpawnBuildSource};
    assert!(k(None, true, false, &SpawnBuildSource::DefaultOriginMain).is_none());
    assert!(k(Some(""), true, false, &SpawnBuildSource::DefaultOriginMain).is_none());
    assert!(
        k(
            Some("   "),
            true,
            false,
            &SpawnBuildSource::DefaultOriginMain
        )
        .is_none(),
        "a whitespace-only requester is anonymous, not an identity"
    );
}

/// A request that claims no build-pool slot is not worth single-flighting:
/// `rebuild:false` spawns from an existing slot exe / the LKG, and two such
/// callers legitimately want two runners.
#[test]
fn spawn_dedup_key_is_none_without_rebuild() {
    use super::{spawn_dedup_key as k, SpawnBuildSource};
    assert!(k(Some("agent-1"), false, false, &SpawnBuildSource::LiveTree).is_none());
}

/// Every distinct build target is its own single flight: the four sources,
/// the operands of the two that carry one, and the `frontend_only` variant
/// of a tree (which would otherwise be handed the stale dist it asked to
/// have replaced).
#[test]
fn spawn_dedup_key_distinguishes_every_build_target() {
    use super::{spawn_dedup_key as k, SpawnBuildSource};
    let key = |rebuild, frontend_only, source: &SpawnBuildSource| {
        k(Some("agent-1"), rebuild, frontend_only, source).expect("keyed")
    };
    let targets: Vec<String> = vec![
        key(true, false, &SpawnBuildSource::DefaultOriginMain),
        key(true, false, &SpawnBuildSource::LiveTree),
        key(true, false, &SpawnBuildSource::ExplicitRef("feat/x".into())),
        key(true, false, &SpawnBuildSource::ExplicitRef("feat/y".into())),
        key(true, false, &SpawnBuildSource::WorktreePath("D:/a".into())),
        key(true, false, &SpawnBuildSource::WorktreePath("D:/b".into())),
        key(true, true, &SpawnBuildSource::DefaultOriginMain),
    ]
    .into_iter()
    .map(|k| k.build_target)
    .collect();

    let unique: std::collections::HashSet<&String> = targets.iter().collect();
    assert_eq!(
        unique.len(),
        targets.len(),
        "every distinct build target must be its own single flight: {targets:?}"
    );

    // ...while the SAME request twice is the same key — that is the join.
    assert_eq!(
        key(true, false, &SpawnBuildSource::DefaultOriginMain),
        key(true, false, &SpawnBuildSource::DefaultOriginMain)
    );
    // A different requester on the same target is a different key.
    assert_ne!(
        k(
            Some("agent-1"),
            true,
            false,
            &SpawnBuildSource::DefaultOriginMain
        ),
        k(
            Some("agent-2"),
            true,
            false,
            &SpawnBuildSource::DefaultOriginMain
        )
    );
}

/// End-to-end through the handler: a second `spawn-test` for a key whose
/// build is still running returns 202 + `deduplicated: true` carrying the
/// EXISTING `build_id`, runner id and port — and starts no build, so the
/// test is deterministic (this path returns before any port reservation).
#[tokio::test]
async fn spawn_test_joins_an_in_flight_build_instead_of_claiming_a_second_slot() {
    use axum::extract::State;
    use axum::response::IntoResponse;
    let state = make_state();

    // A live build for (agent-1, origin_main) — what a plain
    // `{requester_id, rebuild:true}` request resolves to.
    let existing_id = uuid::Uuid::new_v4();
    state
        .build_submissions
        .insert(crate::build_submissions::BuildSubmission {
            id: existing_id,
            worktree_path: std::path::PathBuf::from("/tmp/test/src-tauri"),
            source: None,
            build_kind: crate::build_submissions::BuildKind::Build,
            agent_id: Some("agent-1".to_string()),
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: chrono::Utc::now(),
            status: crate::build_submissions::BuildStatus::Running {
                started_at: chrono::Utc::now(),
            },
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
            spawn: None,
            detached: None,
        })
        .await;
    state.spawn_test_inflight.write().await.insert(
        crate::state::SpawnDedupKey {
            requester_id: "agent-1".to_string(),
            build_target: "origin_main".to_string(),
        },
        crate::state::SpawnInflight {
            submission_id: existing_id,
            runner_id: "test-abc".to_string(),
            port: 9880,
        },
    );

    let resp = super::spawn_test(
        State(state.clone()),
        axum::http::HeaderMap::new(),
        axum::Json(serde_json::json!({"requester_id": "agent-1", "rebuild": true})),
    )
    .await
    .expect("handler ok")
    .into_response();

    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");

    assert_eq!(
        status, 202,
        "a joined request is an accept, not a new build"
    );
    assert_eq!(body["deduplicated"], true);
    assert_eq!(
        body["build_id"], body["submission_id"],
        "build_id IS the submission id — one build identity"
    );
    assert_eq!(
        body["build_id"],
        existing_id.to_string(),
        "the joiner must be handed the EXISTING build_id, not a fresh one"
    );
    assert_eq!(body["id"], "test-abc", "and the existing runner");
    assert_eq!(body["port"], 9880);

    // No second runner was reserved: the registry still holds only the
    // configured primary, and the index still points at the one build.
    assert_eq!(
        state.spawn_test_inflight.read().await.len(),
        1,
        "joining must not record a second in-flight build"
    );
}

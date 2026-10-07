#![cfg(test)]

//! Regression tests for the post-`npm exit 0` defense-in-depth `dist/`
//! sanity gate. See `supervisor-frontend-build-silent-success.md` for
//! the bug these guard against.
use super::{
    artifact_progress_probe, build_timeout_reason, classify_build_stderr, dep_hash_sidecar_path,
    dep_install_reason, dep_manifest_hash, dist_index_ok, merge_process_output,
    needs_frontend_prebuild, provenance_tree_root, provenance_warn_target, resolve_provenance_head,
    rev_parse_head, should_self_heal_slot, stderr_submission_tail, update_lkg_after_success,
    verify_frontend_built, BuildPhase, BuildProvenance, BuildSource, BuildSourceKind, StderrClass,
    LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES,
};
use super::{
    carry_sidecar_into_lkg_via, declared_bin_targets, inspect_built_sidecars,
    remove_if_zero_length, sidecar_build_args, sidecar_build_verdict, SidecarFile,
    PROFILE_CLI_EXE_FILENAME, RUNNER_SIDECARS, SESSION_CLI_EXE_FILENAME, SHIM_EXE_FILENAME,
    SIDECAR_BUILD_ARGS,
};
use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
use crate::error::SupervisorError;
use crate::process::guarded_command::TimeoutKind;
use crate::state::{SharedState, SupervisorState};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// ---------------------------------------------------------------
// External-volume fail-closed guard — plan
// `2026-08-07-external-storage-tiering-for-fleet-disk-pressure`,
// Phase 5. Every case runs with NO dock attached and no filesystem
// access; that is the point, since the plan's HW-ABSENT branch must
// be fully verifiable on a box with no external drive.
// ---------------------------------------------------------------
use super::{disk_guard_allows, disk_guard_allows_for};
use crate::external_volume::ExternalVolumeState;
use std::path::Path;

const GB: u64 = 1024 * 1024 * 1024;
const POOL: &str = "D:/qontinui-ext/target-pool";

#[test]
fn no_declaration_means_byte_identical_to_the_old_guard() {
    // The shipping guarantee: on a machine with no external volume,
    // `disk_guard_allows_for` must agree with `disk_guard_allows` on
    // every input, including the fail-open None.
    let pool = Path::new("D:/qontinui-root/qontinui-runner/target-pool");
    for free in [None, Some(0), Some(29 * GB), Some(30 * GB), Some(900 * GB)] {
        for floor in [0u64, 30] {
            let old = disk_guard_allows(free, floor);
            let new = disk_guard_allows_for(free, floor, None, pool).is_ok();
            assert_eq!(old, new, "divergence at free={free:?} floor={floor}");
        }
    }
}

#[test]
fn external_path_with_unresolvable_probe_refuses() {
    // The inversion. Internally this same input proceeds (asserted
    // in the equivalence test above).
    let err = disk_guard_allows_for(
        None,
        30,
        Some(&ExternalVolumeState::Present),
        Path::new(POOL),
    )
    .expect_err("an unresolvable probe on an external volume must refuse");
    assert!(err.contains("EXTERNAL"), "reason was: {err}");
    assert!(err.contains("fails CLOSED"), "reason was: {err}");
}

#[test]
fn absent_external_volume_refuses_despite_ample_free_space() {
    // 3.9 TB "free" — on the un-mounted stub, i.e. on the internal disk
    // this plan exists to protect. Free space is the wrong question.
    let err = disk_guard_allows_for(
        Some(3900 * GB),
        30,
        Some(&ExternalVolumeState::Absent),
        Path::new(POOL),
    )
    .expect_err("an absent external volume must refuse even with free space");
    assert!(err.contains("NOT mounted"), "reason was: {err}");
}

#[test]
fn mismatched_external_volume_refuses_and_does_not_read_as_a_disconnect() {
    let err = disk_guard_allows_for(
        Some(3900 * GB),
        30,
        Some(&ExternalVolumeState::Mismatched {
            expected: "{d913fcde}".into(),
            found: "{ffffffff}".into(),
        }),
        Path::new(POOL),
    )
    .expect_err("a wrong volume must refuse");
    assert!(err.contains("WRONG volume"), "reason was: {err}");
    assert!(
        !err.contains("NOT mounted"),
        "an operator told 'not mounted' will plug the drive in, which is not the fix: {err}"
    );
}

#[test]
fn present_external_volume_above_the_floor_proceeds() {
    assert!(disk_guard_allows_for(
        Some(3900 * GB),
        30,
        Some(&ExternalVolumeState::Present),
        Path::new(POOL)
    )
    .is_ok());
}

#[test]
fn the_floor_still_bites_on_a_present_external_volume() {
    let err = disk_guard_allows_for(
        Some(GB),
        30,
        Some(&ExternalVolumeState::Present),
        Path::new(POOL),
    )
    .expect_err("1 GB is below the 30 GB floor");
    assert!(err.contains("need at least 30 GB"), "reason was: {err}");
}

#[test]
fn disabling_the_guard_still_disables_it_on_an_external_volume() {
    // `min_free_gb == 0` is the operator's off switch. Silently
    // re-arming it for external paths would make one setting mean two
    // different things depending on where the pool happens to live.
    assert!(
        disk_guard_allows_for(None, 0, Some(&ExternalVolumeState::Absent), Path::new(POOL)).is_ok()
    );
}

/// `queue_timeout_secs` may bound ONLY the phases in which the request is
/// blocked on a lock. Once the build is doing work, the clock must stop —
/// otherwise a small queue bound 504s an already-compiling slot and throws
/// away the compile.
#[test]
fn only_lock_waits_count_as_queue_time() {
    assert!(BuildPhase::AwaitingSlot.is_queue_wait());
    assert!(BuildPhase::AwaitingNpmLock.is_queue_wait());
    assert!(
        !BuildPhase::BuildingFrontend.is_queue_wait(),
        "a running pnpm build is WORK, not queue time"
    );
    assert!(
        !BuildPhase::Compiling.is_queue_wait(),
        "a running cargo build is WORK, not queue time — this is the arm whose \
             timeout abandoned live compiles"
    );
}

/// A timeout must never trigger the poisoned-slot self-heal: wiping the
/// slot's target dir guarantees the retry starts cold, which is the loop
/// that made the build lane unrecoverable.
#[test]
fn timeouts_never_wipe_the_slot_cache() {
    let timeout = SupervisorError::Timeout("no progress for 1200s".into());
    assert!(
        !should_self_heal_slot(&timeout, StderrClass::Environmental),
        "a timed-out build classifies Environmental (no compiler diagnostic) — but its \
             incremental cache is good and must be kept"
    );
    assert!(!should_self_heal_slot(
        &timeout,
        StderrClass::CompilerDiagnostic
    ));
    // Non-timeout failures keep the original rule.
    let other = SupervisorError::Process("linker died".into());
    assert!(should_self_heal_slot(&other, StderrClass::Environmental));
    assert!(!should_self_heal_slot(
        &other,
        StderrClass::CompilerDiagnostic
    ));
}

/// A killed build must say WHICH budget killed it — "no progress for N s"
/// is a diagnosis; the old "timed out after 5400s" was emitted for a build
/// that was actively compiling and said nothing.
#[test]
fn timeout_reason_names_the_budget_that_fired() {
    let no_progress = build_timeout_reason(
        Duration::from_secs(1500),
        TimeoutKind::NoProgress {
            idle: Duration::from_secs(1200),
        },
    );
    assert!(
        no_progress.contains("no-progress watchdog"),
        "{no_progress}"
    );
    assert!(no_progress.contains("1200s"), "{no_progress}");
    assert!(
        no_progress.contains("NOT making progress"),
        "the operator must be told the build was genuinely stuck: {no_progress}"
    );

    let absolute = build_timeout_reason(Duration::from_secs(21600), TimeoutKind::Absolute);
    assert!(absolute.contains("absolute backstop"), "{absolute}");
    assert!(
        absolute.contains("NOT a stuck build"),
        "a ceiling hit must not be reported as stuckness: {absolute}"
    );
    assert!(
        absolute.contains(crate::config::BUILD_ABSOLUTE_TIMEOUT_SECS_ENV),
        "the message must name the knob to raise: {absolute}"
    );
}

/// The artifact probe must move when something is written under the slot's
/// target dir — that is the signal that keeps a silent multi-minute rustc
/// from being read as wedged.
#[test]
fn artifact_probe_moves_when_the_target_dir_changes() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("slot-0");
    std::fs::create_dir_all(target.join("debug").join("deps")).unwrap();
    let probe = artifact_progress_probe(&target);
    let before = probe();
    // mtimes have 1s granularity on some filesystems; make the write
    // unambiguously later.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(target.join("debug").join("deps").join("x.rcgu.o"), b"obj").unwrap();
    let after = probe();
    assert_ne!(
        before, after,
        "writing an artifact under debug/deps must register as progress"
    );
}

/// The phase marker must round-trip through `as_u8`/`from_u8` and produce a
/// phase-accurate queue-timeout message for each phase. This guards the
/// attribution against regression with no live build (plan
/// 2026-06-13-spawn-test-queue-timeout-attribution Verification).
#[test]
fn build_phase_round_trips_and_maps_to_message() {
    for phase in [
        BuildPhase::AwaitingSlot,
        BuildPhase::AwaitingNpmLock,
        BuildPhase::BuildingFrontend,
        BuildPhase::Compiling,
    ] {
        // u8 round-trip.
        assert_eq!(BuildPhase::from_u8(phase.as_u8()), phase);
    }

    // AwaitingSlot is the only phase whose message names a cargo build slot
    // (the genuine slot wait). Permit count is irrelevant here.
    let slot_msg = BuildPhase::AwaitingSlot.timeout_message(30, 3);
    assert!(slot_msg.contains("cargo build slot"), "{slot_msg}");
    assert!(slot_msg.contains("30s"), "{slot_msg}");

    // AwaitingNpmLock attributes the wait to the frontend lock and reports
    // the free cargo permits — the exact mis-attributed starvation case.
    let npm_msg = BuildPhase::AwaitingNpmLock.timeout_message(30, 3);
    assert!(npm_msg.contains("frontend (pnpm) lock"), "{npm_msg}");
    assert!(npm_msg.contains("3 cargo permits free"), "{npm_msg}");
    assert!(
        !npm_msg.contains("build slot"),
        "npm-lock message must NOT claim a slot wait: {npm_msg}"
    );

    // BuildingFrontend names the frontend build, still reporting free permits.
    let fe_msg = BuildPhase::BuildingFrontend.timeout_message(45, 2);
    assert!(fe_msg.contains("frontend (pnpm) build"), "{fe_msg}");
    assert!(fe_msg.contains("2 cargo permits free"), "{fe_msg}");
    assert!(!fe_msg.contains("build slot"), "{fe_msg}");

    // Compiling makes clear the slot was already held (not a slot wait).
    let compile_msg = BuildPhase::Compiling.timeout_message(60, 0);
    assert!(compile_msg.contains("compiling (cargo)"), "{compile_msg}");
    assert!(
        !compile_msg.contains("for a cargo build slot"),
        "{compile_msg}"
    );
}

/// An out-of-range marker value decodes to the conservative `AwaitingSlot`
/// default (an attempt that never advanced the marker reads as the initial
/// slot wait, not a panic).
#[test]
fn build_phase_from_u8_out_of_range_defaults_to_awaiting_slot() {
    assert_eq!(BuildPhase::from_u8(7), BuildPhase::AwaitingSlot);
    assert_eq!(BuildPhase::from_u8(255), BuildPhase::AwaitingSlot);
}

/// `git init` a real repo at `dir` with one commit, returning its HEAD SHA.
/// Mirrors the temp-repo fixture pattern in `spawn_worktree.rs` tests.
fn init_git_repo_one_commit(dir: &std::path::Path, seed_name: &str) -> String {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    fs::write(dir.join(seed_name), seed_name.as_bytes()).expect("seed");
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "initial"]);
    let head = run(&["rev-parse", "HEAD"]);
    String::from_utf8_lossy(&head.stdout).trim().to_string()
}

/// `provenance_tree_root` selects `.parent()` of the live `project_dir`
/// when there's no override. (Source classification is no longer the
/// function's job — it comes from `BuildSourceKind`.)
#[test]
fn provenance_tree_root_live_tree() {
    let project_dir = std::path::Path::new("/ws/qontinui-runner/src-tauri");
    let root = provenance_tree_root(project_dir, None);
    assert_eq!(root, std::path::Path::new("/ws/qontinui-runner"));
}

/// `provenance_tree_root` selects `.parent()` of the OVERRIDE src-tauri,
/// ignoring `project_dir` entirely.
#[test]
fn provenance_tree_root_override() {
    let project_dir = std::path::Path::new("/ws/qontinui-runner/src-tauri");
    let over = std::path::Path::new("/ws/.spawn-feat/qontinui-runner/src-tauri");
    let root = provenance_tree_root(project_dir, Some(over));
    assert_eq!(
        root,
        std::path::Path::new("/ws/.spawn-feat/qontinui-runner")
    );
}

/// The motivating-incident guard: with two distinct git repos (the "live"
/// tree and an "override" worktree at a DIFFERENT HEAD), the SHA probed for
/// an override build is the OVERRIDE tree's HEAD, not the live tree's.
#[tokio::test]
async fn override_build_probes_override_tree_sha_not_live() {
    let base = TempDir::new().expect("tempdir");

    // Live tree: <base>/live/qontinui-runner with src-tauri.
    let live_root = base.path().join("live").join("qontinui-runner");
    let live_src_tauri = live_root.join("src-tauri");
    fs::create_dir_all(&live_src_tauri).expect("mkdir live");
    let live_sha = init_git_repo_one_commit(&live_root, "live-seed");

    // Override tree: <base>/override/qontinui-runner with src-tauri, a
    // DIFFERENT repo with a different HEAD.
    let over_root = base.path().join("override").join("qontinui-runner");
    let over_src_tauri = over_root.join("src-tauri");
    fs::create_dir_all(&over_src_tauri).expect("mkdir override");
    let over_sha = init_git_repo_one_commit(&over_root, "override-seed");

    assert_ne!(live_sha, over_sha, "fixture must produce distinct HEADs");

    // Live-tree selection probes the live tree's HEAD.
    let live_probe_root = provenance_tree_root(&live_src_tauri, None);
    assert_eq!(
        rev_parse_head(&live_probe_root).await,
        Some(live_sha.clone())
    );

    // Override selection probes the OVERRIDE tree's HEAD — the bug fix.
    let over_probe_root = provenance_tree_root(&live_src_tauri, Some(over_src_tauri.as_path()));
    assert_eq!(
        rev_parse_head(&over_probe_root).await,
        Some(over_sha.clone()),
        "override build must record the override tree's sha, not the live tree's"
    );
    assert_ne!(
        rev_parse_head(&over_probe_root).await,
        Some(live_sha),
        "override probe must NOT return the live tree's sha"
    );
}

/// REGRESSION (iteration-2): the pre-cargo provenance WARNING must describe
/// the tree that is actually compiled.
///
/// The warning names a specific sha and asserts "This build compiles X, NOT
/// main". It used to read `project_dir.parent()` unconditionally, so an
/// override build emitted the CANONICAL checkout's HEAD and branch — a
/// confidently wrong claim, for a build whose recorded `build_sha` was (and
/// still is) the override's. This pins the selection to the built tree.
#[test]
fn provenance_warn_target_names_the_built_tree_not_the_canonical_checkout() {
    let project_dir = std::path::Path::new("/ws/qontinui-runner/src-tauri");
    let over = std::path::Path::new("/wt/mtl-iter1/qontinui-runner/src-tauri");

    // Live tree: the canonical checkout, no known sha, not an override.
    let live = provenance_warn_target(project_dir, None, &BuildSourceKind::LiveTree);
    assert_eq!(live.root, std::path::Path::new("/ws/qontinui-runner"));
    assert!(!live.is_override);
    assert_eq!(live.known_sha, None);

    // Foreign override (`worktree_path` / non-main `git_ref`): the OVERRIDE
    // tree, flagged as an override so an unresolvable probe renders UNKNOWN.
    let ovr = provenance_warn_target(project_dir, Some(over), &BuildSourceKind::Override);
    assert_eq!(
        ovr.root,
        std::path::Path::new("/wt/mtl-iter1/qontinui-runner"),
        "an override build's provenance must come from the override tree"
    );
    assert_ne!(
        ovr.root,
        std::path::Path::new("/ws/qontinui-runner"),
        "it must NOT come from the canonical checkout"
    );
    assert!(ovr.is_override);
    assert_eq!(ovr.known_sha, None);

    // Supervisor-materialized origin/main worktree: the worktree root, with
    // the resolved sha carried through rather than re-probed.
    let om = provenance_warn_target(
        project_dir,
        Some(over),
        &BuildSourceKind::OriginMain {
            resolved_sha: "abc123".to_string(),
        },
    );
    assert_eq!(
        om.root,
        std::path::Path::new("/wt/mtl-iter1/qontinui-runner")
    );
    assert!(!om.is_override);
    assert_eq!(om.known_sha.as_deref(), Some("abc123"));

    // The warn target and the RECORDED provenance must always name the same
    // tree — they are two reports of one fact.
    for (over_opt, kind) in [
        (None, BuildSourceKind::LiveTree),
        (Some(over), BuildSourceKind::Override),
    ] {
        assert_eq!(
            provenance_warn_target(project_dir, over_opt, &kind).root,
            provenance_tree_root(project_dir, over_opt),
            "warn target must not drift from compute_build_provenance's root"
        );
    }
}

/// The same guard end-to-end over real git: given an override build root,
/// the HEAD the warning reports is the OVERRIDE's, never the canonical
/// checkout's. Two unrelated repos at different HEADs, so a wrong-tree read
/// cannot coincidentally pass.
#[tokio::test]
async fn provenance_warn_head_is_the_override_trees_not_the_canonical_checkouts() {
    let base = TempDir::new().expect("tempdir");

    let live_root = base.path().join("live").join("qontinui-runner");
    let live_src_tauri = live_root.join("src-tauri");
    fs::create_dir_all(&live_src_tauri).expect("mkdir live");
    let live_sha = init_git_repo_one_commit(&live_root, "live-seed");

    let over_root = base.path().join("override").join("qontinui-runner");
    let over_src_tauri = over_root.join("src-tauri");
    fs::create_dir_all(&over_src_tauri).expect("mkdir override");
    let over_sha = init_git_repo_one_commit(&over_root, "override-seed");
    assert_ne!(live_sha, over_sha, "fixture must produce distinct HEADs");

    let live_target = provenance_warn_target(&live_src_tauri, None, &BuildSourceKind::LiveTree);
    assert_eq!(
        resolve_provenance_head(&live_target).await,
        Some(live_sha.clone())
    );

    let over_target = provenance_warn_target(
        &live_src_tauri,
        Some(over_src_tauri.as_path()),
        &BuildSourceKind::Override,
    );
    let reported = resolve_provenance_head(&over_target).await;
    assert_eq!(
        reported,
        Some(over_sha),
        "the warning must report the override tree's HEAD"
    );
    assert_ne!(
        reported,
        Some(live_sha),
        "reporting the canonical checkout's HEAD is the defect this pins"
    );

    // An origin/main worktree's sha is carried, not re-probed — so it is
    // reported even though the fixture dir is not that worktree.
    let om_target = provenance_warn_target(
        &live_src_tauri,
        Some(over_src_tauri.as_path()),
        &BuildSourceKind::OriginMain {
            resolved_sha: "0123456789abcdef".to_string(),
        },
    );
    assert_eq!(
        resolve_provenance_head(&om_target).await.as_deref(),
        Some("0123456789abcdef")
    );
}

/// A `worktree_path` need not be a git checkout at all. The provenance then
/// cannot be established — and must render UNKNOWN (no sha) rather than
/// falling back to some other tree's confident default.
#[tokio::test]
async fn provenance_warn_head_is_unknown_for_a_non_git_override_tree() {
    let base = TempDir::new().expect("tempdir");

    let live_root = base.path().join("live").join("qontinui-runner");
    let live_src_tauri = live_root.join("src-tauri");
    fs::create_dir_all(&live_src_tauri).expect("mkdir live");
    let live_sha = init_git_repo_one_commit(&live_root, "live-seed");

    // Plain directories — no `git init`.
    let over_src_tauri = base
        .path()
        .join("plain")
        .join("qontinui-runner")
        .join("src-tauri");
    fs::create_dir_all(&over_src_tauri).expect("mkdir plain");

    let target = provenance_warn_target(
        &live_src_tauri,
        Some(over_src_tauri.as_path()),
        &BuildSourceKind::Override,
    );
    assert!(target.is_override, "an override must be flagged as one");
    let head = resolve_provenance_head(&target).await;
    assert_ne!(
        head,
        Some(live_sha),
        "an unresolvable override must NEVER borrow the canonical checkout's sha"
    );
    assert_eq!(
        head, None,
        "unresolvable provenance is UNKNOWN, not a default"
    );
}

/// Filename of the pnpm bin stub. `.cmd` on Windows (where pnpm installs
/// `.bin/<tool>.cmd` shims), bare elsewhere. Mirrors the platform check
/// inside [`needs_frontend_prebuild`].
fn ui_bridge_build_ir_bin() -> &'static str {
    if cfg!(windows) {
        "ui-bridge-build-ir.cmd"
    } else {
        "ui-bridge-build-ir"
    }
}

#[test]
fn needs_frontend_prebuild_true_when_node_modules_and_dist_absent() {
    // Simulates a fresh `git worktree add --detach` — nothing in the
    // workspace, no prior frontend build. Must trigger the prebuild.
    let tmp = TempDir::new().expect("tempdir");
    assert!(
        needs_frontend_prebuild(tmp.path()),
        "fresh worktree (no node_modules + no dist/) must require prebuild"
    );
}

#[test]
fn needs_frontend_prebuild_true_when_only_node_modules_present() {
    // Half-installed state — pnpm install succeeded but the previous
    // `pnpm run build` never ran or failed. We should NOT skip the
    // prebuild because dist/index.html is what cargo embeds.
    let tmp = TempDir::new().expect("tempdir");
    let bin_dir = tmp.path().join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
    assert!(
        needs_frontend_prebuild(tmp.path()),
        "node_modules present but no dist/index.html must still require prebuild"
    );
}

#[test]
fn needs_frontend_prebuild_true_when_only_dist_present() {
    // Inverse half-installed state — somehow dist/ exists but
    // node_modules is gone (e.g. someone ran `rm -rf node_modules`
    // between sessions). Must re-prebuild because `pnpm run build`
    // can't run without the dep tree.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(dist.join("index.html"), b"<!doctype html>").expect("write index");
    assert!(
        needs_frontend_prebuild(tmp.path()),
        "dist/ present but no node_modules must still require prebuild"
    );
}

#[test]
fn needs_frontend_prebuild_false_when_both_present() {
    // Idempotency gate — both signals say a prior prebuild succeeded
    // and we should reuse it. This is the path that saves ~30s per
    // repeated spawn-test on the same ref.
    let tmp = TempDir::new().expect("tempdir");
    let bin_dir = tmp.path().join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(dist.join("index.html"), b"<!doctype html>").expect("write index");
    assert!(
        !needs_frontend_prebuild(tmp.path()),
        "fully populated worktree must skip prebuild (idempotent reuse)"
    );
}

// -----------------------------------------------------------------------
// S1 — dep-install FRESHNESS gate (lockfile-hash sidecar)
//
// Regression class: a reused `.spawn-<ref>` container force-reset to a new
// ref keeps its `node_modules/` from the PREVIOUS ref. The old
// presence-gated check ("does node_modules/.bin/ui-bridge-build-ir exist?")
// said "skip install", so the new ref's TypeScript compiled against the old
// ref's dependency tree and produced a phantom `TS2339` that looked exactly
// like a red origin/main.
// -----------------------------------------------------------------------

/// Populate `wt_root` as an "already installed" worktree: the pnpm bin
/// marker + a lockfile + a package.json with the given contents.
fn seed_installed_worktree(wt_root: &std::path::Path, lockfile: &str, package_json: &str) {
    let bin_dir = wt_root.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).expect("mkdir node_modules/.bin");
    fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
    fs::write(wt_root.join("pnpm-lock.yaml"), lockfile).expect("write lockfile");
    fs::write(wt_root.join("package.json"), package_json).expect("write package.json");
}

/// Simulate what a successful `pnpm install` leaves behind: the sidecar
/// recording the dep-manifest hash AT THAT MOMENT. (The async
/// `write_dep_hash_sidecar` needs a live `SharedState`; the file it writes
/// is exactly this, so the pure gate is driven directly.)
fn stamp_dep_hash_sidecar(wt_root: &std::path::Path) {
    let hash = dep_manifest_hash(wt_root).expect("fixture has manifests");
    fs::write(dep_hash_sidecar_path(wt_root), hash).expect("write sidecar");
}

#[test]
fn dep_install_reason_none_when_lockfile_unchanged() {
    // THE skip case: same container, same ref, deps untouched since the
    // last successful install. Must NOT re-pay the ~30s install.
    let tmp = TempDir::new().expect("tempdir");
    seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
    stamp_dep_hash_sidecar(tmp.path());

    assert!(
        dep_install_reason(tmp.path()).is_none(),
        "unchanged lockfile + present marker + matching sidecar must SKIP install"
    );
}

#[test]
fn dep_install_reason_some_when_lockfile_changed() {
    // THE regression: container reused across a ref whose deps moved
    // (`@qontinui/navigation` 0.1.5 → ^0.2.0). `node_modules` and its
    // marker are still there — only the lockfile content changed — so the
    // old presence gate skipped and built against stale deps.
    let tmp = TempDir::new().expect("tempdir");
    seed_installed_worktree(
        tmp.path(),
        "lockfileVersion: 9\n  '@qontinui/navigation': 0.1.5\n",
        r#"{"dependencies":{"@qontinui/navigation":"^0.1.0"}}"#,
    );
    stamp_dep_hash_sidecar(tmp.path());
    assert!(
        dep_install_reason(tmp.path()).is_none(),
        "precondition: the seeded state must be considered fresh"
    );

    // The container is force-reset to a ref with different deps. Only the
    // lockfile/package.json change on disk — node_modules is untouched.
    fs::write(
        tmp.path().join("pnpm-lock.yaml"),
        "lockfileVersion: 9\n  '@qontinui/navigation': 0.2.0\n",
    )
    .expect("rewrite lockfile");

    let reason = dep_install_reason(tmp.path())
        .expect("a changed lockfile MUST force a reinstall, not a stale-node_modules build");
    assert!(
        reason.contains("CHANGED") && reason.contains("STALE"),
        "reason must name the staleness so the log is self-explaining; got: {}",
        reason
    );
}

#[test]
fn dep_install_reason_some_when_package_json_changed_but_lockfile_stale() {
    // A dep pin bumped in package.json without the lockfile regenerated
    // still disagrees with the installed tree (and `--frozen-lockfile`
    // would reject it). Hashing package.json too catches this.
    let tmp = TempDir::new().expect("tempdir");
    seed_installed_worktree(
        tmp.path(),
        "lockfileVersion: 9\n",
        r#"{"dependencies":{"@qontinui/navigation":"^0.1.0"}}"#,
    );
    stamp_dep_hash_sidecar(tmp.path());

    fs::write(
        tmp.path().join("package.json"),
        r#"{"dependencies":{"@qontinui/navigation":"^0.2.0"}}"#,
    )
    .expect("rewrite package.json");

    assert!(
        dep_install_reason(tmp.path()).is_some(),
        "a package.json dep change must force a reinstall"
    );
}

#[test]
fn dep_install_reason_some_when_sidecar_absent() {
    // Pre-existing containers (installed before this gate shipped) have
    // node_modules but no sidecar. Provenance unknown ⇒ must NOT be trusted.
    let tmp = TempDir::new().expect("tempdir");
    seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
    // deliberately no stamp

    let reason = dep_install_reason(tmp.path())
        .expect("absent sidecar means unknown node_modules provenance ⇒ reinstall");
    assert!(
        reason.contains("absent"),
        "reason must say the sidecar is absent; got: {}",
        reason
    );
}

#[test]
fn dep_install_reason_some_when_marker_absent_even_if_sidecar_matches() {
    // Someone `rm -rf node_modules/.bin` (or a half-install). A surviving
    // sidecar must never vouch for a tree that isn't installed — the marker
    // check is checked FIRST for exactly this reason.
    let tmp = TempDir::new().expect("tempdir");
    seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
    stamp_dep_hash_sidecar(tmp.path());
    fs::remove_file(
        tmp.path()
            .join("node_modules")
            .join(".bin")
            .join(ui_bridge_build_ir_bin()),
    )
    .expect("remove marker");

    let reason =
        dep_install_reason(tmp.path()).expect("missing install marker must force a reinstall");
    assert!(
        reason.contains("marker"),
        "reason must name the missing marker; got: {}",
        reason
    );
}

#[test]
fn dep_install_reason_none_when_no_manifests_at_all() {
    // Not a JS project: nothing governs an install, and running one would
    // just fail. Degrade to the legacy marker-presence outcome.
    let tmp = TempDir::new().expect("tempdir");
    let bin_dir = tmp.path().join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");

    assert!(dep_manifest_hash(tmp.path()).is_none());
    assert!(
        dep_install_reason(tmp.path()).is_none(),
        "no manifests + marker present must preserve the legacy skip"
    );
}

#[test]
fn dep_manifest_hash_is_stable_and_content_sensitive() {
    let tmp = TempDir::new().expect("tempdir");
    fs::write(tmp.path().join("pnpm-lock.yaml"), "a").expect("w");
    fs::write(tmp.path().join("package.json"), "b").expect("w");
    let h1 = dep_manifest_hash(tmp.path()).expect("hash");
    let h2 = dep_manifest_hash(tmp.path()).expect("hash");
    assert_eq!(h1, h2, "hash must be deterministic for identical bytes");

    // Removing a lockfile must change the hash (absence is hashed, so a
    // present→absent flip cannot be mistaken for "unchanged").
    fs::remove_file(tmp.path().join("pnpm-lock.yaml")).expect("rm");
    let h3 = dep_manifest_hash(tmp.path()).expect("hash");
    assert_ne!(h1, h3, "removing the lockfile must change the hash");

    // Swapping content between the two files must not collide (name +
    // length are mixed in ahead of the bytes).
    fs::write(tmp.path().join("pnpm-lock.yaml"), "b").expect("w");
    fs::write(tmp.path().join("package.json"), "a").expect("w");
    let h4 = dep_manifest_hash(tmp.path()).expect("hash");
    assert_ne!(h1, h4, "content swap across manifests must not collide");
}

// -----------------------------------------------------------------------
// S2 — a frontend failure must carry the COMPILER error (tsc writes to
// stdout; the legacy capture read stderr only and came back empty).
// -----------------------------------------------------------------------

/// Build a fake finished-process Output with the given streams.
fn fake_output(stdout: &str, stderr: &str) -> std::process::Output {
    std::process::Output {
        // Status is irrelevant to the merge; take a default failure-ish one.
        status: Default::default(),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn merge_process_output_keeps_tsc_errors_from_stdout() {
    // The exact shape of the P0: tsc puts `error TS2339` on stdout and
    // leaves stderr EMPTY. Reading stderr alone yields "" — a failed build
    // with no visible reason.
    let out = fake_output(
        "src/nav.tsx(12,7): error TS2339: Property 'hasOwnPage' does not exist on type 'NavigationItem'.\n",
        "",
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).is_empty(),
        "fixture premise: stderr is empty"
    );

    let merged = merge_process_output(&out);
    assert!(
        merged.contains("error TS2339"),
        "merged output MUST carry the tsc error from stdout; got: {:?}",
        merged
    );
    assert!(merged.contains("--- stdout ---"));
    assert!(
        !merged.contains("--- stderr ---"),
        "an empty stream must not add an empty labelled section"
    );
}

#[test]
fn merge_process_output_keeps_both_streams() {
    let merged = merge_process_output(&fake_output("OUT-LINE", "ERR-LINE"));
    assert!(merged.contains("OUT-LINE") && merged.contains("ERR-LINE"));
    assert!(merged.contains("--- stdout ---") && merged.contains("--- stderr ---"));
}

#[test]
fn merge_process_output_empty_when_both_streams_empty() {
    assert!(merge_process_output(&fake_output("", "")).is_empty());
}

#[test]
fn verify_frontend_built_err_when_index_missing() {
    // Simulates the empirical 2026-05-21 failure mode: npm exit 0 but
    // dist/index.html still missing. Must surface a clear error
    // mentioning the missing artifact so the user can correlate it
    // with the eventual `tauri::generate_context!` panic.
    let tmp = TempDir::new().expect("tempdir");
    let res = verify_frontend_built(tmp.path());
    let err = res.expect_err("missing dist/index.html must error");
    let s = err.to_string();
    assert!(
        s.contains("dist") && s.contains("index.html"),
        "error must name the missing artifact (dist/index.html); got: {}",
        s
    );
}

#[test]
fn verify_frontend_built_err_when_index_empty() {
    // Pathological case carried over from the legacy safari13
    // regression: vite exits 0 having written zero bytes. Cargo would
    // embed an empty index.html and the runner would render a blank
    // page. Surface as an error too.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(dist.join("index.html"), b"").expect("write empty index");
    let res = verify_frontend_built(tmp.path());
    let err = res.expect_err("empty dist/index.html must error");
    let s = err.to_string();
    assert!(
        s.contains("dist") && s.contains("index.html"),
        "error must name the empty artifact (dist/index.html); got: {}",
        s
    );
}

#[test]
fn verify_frontend_built_ok_when_index_present_and_nonempty() {
    // Happy path — a real npm build wrote every required dist artifact.
    // NOTE: build-id.txt is required as of the build-id-banner plan; a
    // dist carrying index.html alone is NOT a complete build.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(
        dist.join("index.html"),
        b"<!doctype html><html><body>ok</body></html>",
    )
    .expect("write index");
    fs::write(dist.join("build-id.txt"), b"abc123-1785101162102").expect("write build-id");
    verify_frontend_built(tmp.path()).expect("a complete dist must verify clean");
}

#[test]
fn verify_frontend_built_err_when_build_id_missing() {
    // The regression this plan exists for: vite had not yet written
    // dist/build-id.txt when cargo was handed the tree, so build.rs took
    // its invent-a-timestamp fallback and froze a meta-tag/env mismatch
    // into the binary permanently (2026-07-26, plan
    // 2026-07-28-runner-build-id-banner-permanent-false-positive D1).
    // A dist with index.html but no build-id.txt must NOT reach cargo.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(
        dist.join("index.html"),
        b"<!doctype html><html><body>ok</body></html>",
    )
    .expect("write index");
    let err = verify_frontend_built(tmp.path())
        .expect_err("dist without build-id.txt must error before cargo runs");
    let s = err.to_string();
    assert!(
        s.contains("build-id.txt") && !s.contains("index.html"),
        "error must name the MISSING artifact (dist/build-id.txt) and not \
             the present one (index.html); got: {}",
        s
    );
}

#[test]
fn verify_frontend_built_err_when_build_id_empty() {
    // Same window, zero-byte flavour: the file exists but vite has not
    // finished writing it. An empty build-id.txt makes build.rs read an
    // empty string and fall through to the sentinel just as if the file
    // were absent, so it must be rejected on the same terms.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(
        dist.join("index.html"),
        b"<!doctype html><html><body>ok</body></html>",
    )
    .expect("write index");
    fs::write(dist.join("build-id.txt"), b"").expect("write empty build-id");
    let err = verify_frontend_built(tmp.path())
        .expect_err("empty dist/build-id.txt must error before cargo runs");
    let s = err.to_string();
    assert!(
        s.contains("build-id.txt") && !s.contains("index.html"),
        "error must name the EMPTY artifact (dist/build-id.txt) and not \
             the valid one (index.html); got: {}",
        s
    );
}

#[test]
fn dist_index_ok_returns_false_when_dist_dir_missing() {
    // Simulates the multi-agent scenario where a concurrent external
    // `npm run build` wiped the entire dist/ directory between this
    // supervisor's npm exit and cargo's embed step.
    let tmp = TempDir::new().expect("tempdir");
    assert!(
        !dist_index_ok(tmp.path()),
        "missing dist/ must be reported as not-ok so the slot is flagged stale"
    );
}

#[test]
fn dist_index_ok_returns_false_when_index_html_missing() {
    // Simulates an empty-output regression: dist/ exists (an earlier
    // build created it) but index.html specifically is gone.
    let tmp = TempDir::new().expect("tempdir");
    fs::create_dir_all(tmp.path().join("dist")).expect("mkdir dist");
    assert!(
        !dist_index_ok(tmp.path()),
        "dist/ without index.html must be reported as not-ok"
    );
}

#[test]
fn dist_index_ok_returns_false_when_index_html_is_empty() {
    // Simulates the historical safari13 regression where vite exited 0
    // having written zero bytes (proj_issue_runner_npm_build_safari13_target.md).
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(dist.join("index.html"), b"").expect("write empty index");
    assert!(
        !dist_index_ok(tmp.path()),
        "empty dist/index.html must be reported as not-ok"
    );
}

#[test]
fn dist_index_ok_returns_true_when_index_html_present_and_nonempty() {
    // Happy path — a real build wrote every required dist artifact.
    // build-id.txt joined the contract with the build-id-banner plan.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(
        dist.join("index.html"),
        b"<!doctype html><html><body>ok</body></html>",
    )
    .expect("write index");
    fs::write(dist.join("build-id.txt"), b"abc123-1785101162102").expect("write build-id");
    assert!(
        dist_index_ok(tmp.path()),
        "a complete non-empty dist is the signal of a healthy build"
    );
}

#[test]
fn dist_index_ok_returns_false_when_build_id_missing_or_empty() {
    // The live-tree half of the same gate. Widening only
    // verify_frontend_built would have left this path able to flip
    // frontend_stale=false against a dist vite had not finished writing.
    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).expect("mkdir dist");
    fs::write(
        dist.join("index.html"),
        b"<!doctype html><html><body>ok</body></html>",
    )
    .expect("write index");
    assert!(
        !dist_index_ok(tmp.path()),
        "dist without build-id.txt must be reported as not-ok"
    );

    fs::write(dist.join("build-id.txt"), b"").expect("write empty build-id");
    assert!(
        !dist_index_ok(tmp.path()),
        "dist with an empty build-id.txt must be reported as not-ok"
    );
}

#[test]
fn dist_index_ok_returns_false_when_index_html_is_a_directory() {
    // Pathological case: someone created dist/index.html as a
    // directory (mkdir -p dist/index.html). The metadata.is_file()
    // guard catches this — without it, len() would return junk.
    let tmp = TempDir::new().expect("tempdir");
    fs::create_dir_all(tmp.path().join("dist").join("index.html")).expect("mkdir");
    assert!(
        !dist_index_ok(tmp.path()),
        "dist/index.html as a directory must be reported as not-ok"
    );
}

// =====================================================================
// Phase 2: LKG promotion gate — `update_lkg_after_success` must promote
// live-tree builds (recording sha + source in lkg.json) and SKIP override
// builds entirely (exe + sidecar untouched). Root fix for the 2026-06-05
// incident where a branch build was promoted to LKG and deployed.
// =====================================================================

/// Build a `SharedState` whose runner workspace root is a tempdir, so the
/// LKG dir (`<root>/target-pool/lkg/`) and slot exe
/// (`<root>/target-pool/slot-0/debug/qontinui-runner.exe`) land under a
/// throwaway path. `project_dir` is `<root>/src-tauri` because
/// `runner_npm_dir()` takes its parent. Returns the state plus the
/// canonicalized workspace root (canonicalized to match `runner_npm_dir`'s
/// own `canonicalize()`, so the test's path expectations line up).
fn lkg_test_state(workspace_root: &std::path::Path) -> SharedState {
    let project_dir = workspace_root.join("src-tauri");
    fs::create_dir_all(&project_dir).expect("mkdir src-tauri");
    let config = SupervisorConfig {
        project_dir,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9875,
        dev_logs_dir: workspace_root.join(".dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: BuildPoolConfig { pool_size: 1 },
        no_prewarm: true,
        no_webview: true,
        temp_runner_display: None,
    };
    Arc::new(SupervisorState::new(config))
}

/// Stage a fake slot-0 exe with known bytes so the copy step has something
/// to promote. Returns the exe path.
fn stage_slot0_exe(state: &SharedState, bytes: &[u8]) -> std::path::PathBuf {
    let exe = state.config.runner_exe_path_for_slot(0);
    fs::create_dir_all(exe.parent().unwrap()).expect("mkdir slot debug");
    fs::write(&exe, bytes).expect("write slot exe");
    exe
}

fn live_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
    BuildProvenance {
        sha: sha.map(str::to_string),
        source: BuildSource::LiveTree,
        built_from: built_from.to_string(),
        built_at: "2026-06-05T00:00:00Z".to_string(),
    }
}

fn override_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
    BuildProvenance {
        sha: sha.map(str::to_string),
        source: BuildSource::Override,
        built_from: built_from.to_string(),
        built_at: "2026-06-05T00:00:00Z".to_string(),
    }
}

fn origin_main_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
    BuildProvenance {
        sha: sha.map(str::to_string),
        source: BuildSource::OriginMain,
        built_from: built_from.to_string(),
        built_at: "2026-06-07T00:00:00Z".to_string(),
    }
}

/// Live-tree build promotes: the LKG exe is written with the slot's bytes
/// and `lkg.json` records `sha` + `"source":"live_tree"`. Also asserts the
/// in-memory `last_known_good` lock is populated from the same provenance.
#[tokio::test]
async fn live_tree_build_promotes_and_records_provenance() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canon root");
    let state = lkg_test_state(&root);
    stage_slot0_exe(&state, b"fresh-live-tree-bytes");

    let slot = state.build_pool.slots[0].clone();
    let prov = live_provenance(Some("abc123def456"), "/ws/qontinui-runner");

    update_lkg_after_success(&state, &slot, &prov)
        .await
        .expect("live-tree build must promote to LKG");

    // Exe promoted with the slot's bytes.
    let lkg_exe = state.config.lkg_exe_path();
    assert_eq!(
        fs::read(&lkg_exe).expect("read lkg exe"),
        b"fresh-live-tree-bytes",
        "LKG exe must carry the promoted slot bytes"
    );

    // Sidecar carries sha + source from provenance.
    let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
    assert_eq!(meta["sha"], "abc123def456", "lkg.json must record sha");
    assert_eq!(
        meta["source"], "live_tree",
        "lkg.json must record source=live_tree, got {meta_raw}"
    );
    assert_eq!(meta["source_slot"], 0);

    // In-memory lock hydrated from the same provenance.
    let lkg = state.build_pool.last_known_good.read().await.clone();
    let lkg = lkg.expect("last_known_good must be populated after live-tree promote");
    assert_eq!(lkg.sha.as_deref(), Some("abc123def456"));
    assert_eq!(lkg.source, BuildSource::LiveTree);
}

/// Live-tree build with a failed git probe (`sha: None`) still promotes;
/// `lkg.json`'s `sha` serializes as JSON null (honest "unknown SHA"),
/// `source` is still `live_tree`.
#[tokio::test]
async fn live_tree_build_with_null_sha_promotes_with_null_in_sidecar() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canon root");
    let state = lkg_test_state(&root);
    stage_slot0_exe(&state, b"live-no-sha");

    let slot = state.build_pool.slots[0].clone();
    let prov = live_provenance(None, "/ws/qontinui-runner");

    update_lkg_after_success(&state, &slot, &prov)
        .await
        .expect("live-tree build must promote even when sha probe failed");

    let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
    assert!(
        meta["sha"].is_null(),
        "null sha must serialize as JSON null"
    );
    assert_eq!(meta["source"], "live_tree");
}

/// Override build does NOT promote: a PRE-EXISTING LKG exe + sidecar are
/// left byte-for-byte untouched, the in-memory lock is unchanged, and the
/// call still returns `Ok` (skip is not an error). This is the gate.
#[tokio::test]
async fn override_build_does_not_touch_lkg() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canon root");
    let state = lkg_test_state(&root);

    // Pre-seed a prior good LKG (exe + sidecar) so we can prove the
    // override build leaves it intact rather than there simply being
    // nothing to write.
    let lkg_dir = state.config.lkg_dir();
    fs::create_dir_all(&lkg_dir).expect("mkdir lkg");
    let lkg_exe = state.config.lkg_exe_path();
    fs::write(&lkg_exe, b"prior-good-lkg-bytes").expect("seed lkg exe");
    let meta_path = state.config.lkg_metadata_path();
    let prior_meta = r#"{"built_at":"2026-06-01T00:00:00Z","source_slot":2,"exe_size":20,"sha":"prior0000000","source":"live_tree"}"#;
    fs::write(&meta_path, prior_meta).expect("seed lkg.json");

    // Stage a DIFFERENT slot exe that would be promoted if the gate failed.
    stage_slot0_exe(&state, b"foreign-override-bytes");

    let slot = state.build_pool.slots[0].clone();
    let prov = override_provenance(Some("feedface0000"), "/ws/.spawn-feat/qontinui-runner");

    update_lkg_after_success(&state, &slot, &prov)
        .await
        .expect("override build must return Ok (skip, not error)");

    // Exe untouched — still the prior good bytes, NOT the foreign slot exe.
    assert_eq!(
        fs::read(&lkg_exe).expect("read lkg exe"),
        b"prior-good-lkg-bytes",
        "override build must NOT overwrite the LKG exe"
    );
    // Sidecar untouched — byte-for-byte the prior content.
    assert_eq!(
        fs::read_to_string(&meta_path).expect("read lkg.json"),
        prior_meta,
        "override build must NOT rewrite lkg.json"
    );
    // In-memory lock unchanged (still None — we never set it on the prior
    // seed; the gate must not populate it from an override build).
    assert!(
        state.build_pool.last_known_good.read().await.is_none(),
        "override build must NOT populate the last_known_good lock"
    );
}

/// Phase B: an `origin_main` build IS promoted to LKG (it is vouched, unlike
/// `override`). The LKG exe carries the slot bytes and `lkg.json` records
/// `"source":"origin_main"` + the resolved sha. This is the fix for the
/// stale-LKG symptom: an origin/main primary build SHOULD advance LKG.
#[tokio::test]
async fn origin_main_build_promotes_to_lkg() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canon root");
    let state = lkg_test_state(&root);
    stage_slot0_exe(&state, b"fresh-origin-main-bytes");

    let slot = state.build_pool.slots[0].clone();
    let prov = origin_main_provenance(
        Some("0a1b2c3d4e5f"),
        "/ws/.spawn-origin-main/qontinui-runner",
    );

    update_lkg_after_success(&state, &slot, &prov)
        .await
        .expect("origin/main build must promote to LKG");

    // Exe promoted with the slot's bytes.
    let lkg_exe = state.config.lkg_exe_path();
    assert_eq!(
        fs::read(&lkg_exe).expect("read lkg exe"),
        b"fresh-origin-main-bytes",
        "LKG exe must carry the promoted origin/main slot bytes"
    );

    // Sidecar records sha + source=origin_main from provenance.
    let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
    assert_eq!(meta["sha"], "0a1b2c3d4e5f", "lkg.json must record sha");
    assert_eq!(
        meta["source"], "origin_main",
        "lkg.json must record source=origin_main, got {meta_raw}"
    );

    // In-memory lock hydrated from the same provenance.
    let lkg = state.build_pool.last_known_good.read().await.clone();
    let lkg = lkg.expect("last_known_good must be populated after origin/main promote");
    assert_eq!(lkg.sha.as_deref(), Some("0a1b2c3d4e5f"));
    assert_eq!(lkg.source, BuildSource::OriginMain);
}

// =====================================================================
// Sidecar roster (plan 2026-09-27-qontinui-pr-zero-byte-sidecar-
// placeholder-published-as-session-cli, Phase 3): the supervisor builds,
// carries to LKG and deploys REAL `qontinui-shim` / `qontinui-pr` /
// `qontinui_profile` binaries, and never propagates a zero-length one.
// =====================================================================

/// The sidecar build is ONE `--keep-going` invocation over the whole
/// roster, with the runner build's feature set. Pinned literally: dropping
/// `--keep-going` lets one sidecar's compile error cost the shim (the
/// 2026-07-03 class), dropping a `--bin` leaves that sidecar a 0-byte
/// placeholder copy, and changing the features recompiles the warm target.
#[test]
fn sidecar_build_args_pin_the_roster() {
    assert_eq!(
        SIDECAR_BUILD_ARGS,
        &[
            "build",
            "--keep-going",
            "--bin",
            "qontinui-shim",
            "--bin",
            "qontinui-pr",
            "--bin",
            "qontinui_profile",
            "--features",
            "custom-protocol",
        ]
    );
    // The `--bin` values are exactly the roster, in order, so the build,
    // the LKG carry and the deploy can never disagree on what a sidecar is.
    let built: Vec<&str> = SIDECAR_BUILD_ARGS
        .windows(2)
        .filter(|w| w[0] == "--bin")
        .map(|w| w[1])
        .collect();
    let roster: Vec<&str> = RUNNER_SIDECARS.iter().map(|s| s.bin).collect();
    assert_eq!(built, roster);
    assert_eq!(
        RUNNER_SIDECARS.map(|s| s.filename),
        [
            SHIM_EXE_FILENAME,
            SESSION_CLI_EXE_FILENAME,
            PROFILE_CLI_EXE_FILENAME
        ]
    );
    // The runner resolves the shim and the session CLI beside its own exe
    // (shim_materializer.rs); nothing in it resolves qontinui_profile.
    let deployed: Vec<&str> = RUNNER_SIDECARS
        .iter()
        .filter(|s| s.deployed_beside_runner)
        .map(|s| s.bin)
        .collect();
    assert_eq!(deployed, ["qontinui-shim", "qontinui-pr"]);
}

/// Write a runner-like `src-tauri` package: `manifest` as `Cargo.toml`,
/// plus each of `bin_files` (paths relative to the package root).
fn stage_runner_package(root: &std::path::Path, manifest: &str, bin_files: &[&str]) {
    fs::create_dir_all(root).expect("mkdir package");
    fs::write(root.join("Cargo.toml"), manifest).expect("write manifest");
    for rel in bin_files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).expect("mkdir bin dir");
        fs::write(&path, "fn main() {}\n").expect("write bin");
    }
}

/// N1 (review round 1): a runner ref that predates `qontinui-pr` (it did
/// not exist 2026-06-08..2026-07-10) must not be handed `--bin qontinui-pr`
/// — cargo rejects the whole invocation before compiling anything,
/// `--keep-going` notwithstanding, and the shim is lost with it.
#[test]
fn sidecar_build_args_skip_a_bin_the_runner_tree_does_not_declare() {
    let tmp = TempDir::new().expect("tempdir");
    let pkg = tmp.path().join("src-tauri");
    stage_runner_package(
        &pkg,
        "[package]\nname = \"qontinui-runner\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"qontinui-shim\"\npath = \"src/bin/qontinui_shim.rs\"\n",
        &["src/bin/qontinui_shim.rs", "src/bin/qontinui_profile.rs"],
    );

    let declared = declared_bin_targets(&pkg).expect("a readable package manifest");
    assert!(
        declared.contains("qontinui-shim"),
        "explicit [[bin]]: {declared:?}"
    );
    assert!(
        declared.contains("qontinui_profile"),
        "discovered bin: {declared:?}"
    );
    assert!(!declared.contains("qontinui-pr"), "{declared:?}");

    assert_eq!(
        sidecar_build_args(Some(&declared)),
        vec![
            "build",
            "--keep-going",
            "--bin",
            "qontinui-shim",
            "--bin",
            "qontinui_profile",
            "--features",
            "custom-protocol",
        ]
    );
}

/// A tree declaring the whole roster gets exactly [`SIDECAR_BUILD_ARGS`];
/// an unreadable manifest (UNKNOWN) does too; a tree declaring none of the
/// roster gets no `--bin` at all, which the caller treats as "skip cargo".
#[test]
fn sidecar_build_args_full_unknown_and_empty_rosters() {
    let tmp = TempDir::new().expect("tempdir");
    let pkg = tmp.path().join("src-tauri");
    stage_runner_package(
        &pkg,
        "[package]\nname = \"qontinui-runner\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"qontinui-shim\"\npath = \"src/bin/qontinui_shim.rs\"\n\n\
             [[bin]]\nname = \"qontinui-pr\"\npath = \"src/bin/qontinui_cli.rs\"\n",
        &[
            "src/bin/qontinui_shim.rs",
            "src/bin/qontinui_cli.rs",
            "src/bin/qontinui_profile.rs",
        ],
    );
    let declared = declared_bin_targets(&pkg).expect("readable");
    assert_eq!(
        sidecar_build_args(Some(&declared)),
        SIDECAR_BUILD_ARGS.to_vec()
    );
    assert_eq!(sidecar_build_args(None), SIDECAR_BUILD_ARGS.to_vec());
    let none_declared = std::collections::BTreeSet::new();
    assert!(!sidecar_build_args(Some(&none_declared)).contains(&"--bin"));
}

/// `autobins = false` turns discovery off; a manifest with no `[package]`
/// (a virtual workspace root) or no manifest at all is UNKNOWN (`None`).
#[test]
fn declared_bin_targets_honours_autobins_and_refuses_what_it_cannot_read() {
    let tmp = TempDir::new().expect("tempdir");
    let pkg = tmp.path().join("no-autobins");
    stage_runner_package(
        &pkg,
        "[package]\nname = \"r\"\nversion = \"0.1.0\"\nautobins = false\n\n\
             [[bin]]\nname = \"qontinui-shim\"\npath = \"src/bin/qontinui_shim.rs\"\n",
        &["src/bin/qontinui_shim.rs", "src/bin/qontinui_profile.rs"],
    );
    let declared = declared_bin_targets(&pkg).expect("readable");
    assert!(declared.contains("qontinui-shim"));
    assert!(!declared.contains("qontinui_profile"), "{declared:?}");

    let dir_bin = tmp.path().join("dir-bin");
    stage_runner_package(
        &dir_bin,
        "[package]\nname = \"r\"\nversion = \"0.1.0\"\n",
        &["src/bin/qontinui_profile/main.rs"],
    );
    assert!(declared_bin_targets(&dir_bin)
        .expect("readable")
        .contains("qontinui_profile"));

    let virtual_root = tmp.path().join("virtual");
    stage_runner_package(&virtual_root, "[workspace]\nmembers = []\n", &[]);
    assert_eq!(declared_bin_targets(&virtual_root), None);
    assert_eq!(declared_bin_targets(&tmp.path().join("absent")), None);
}

/// R2-2 (review round 2): a runner tree that declares NONE of the roster
/// (refs before 2026-04-30) must not be told "every sidecar file is
/// present and non-empty" — nothing was checked. The stale files a slot
/// may still hold from another build are not this tree's, so they are not
/// reported as built either.
#[test]
fn sidecar_verdict_for_a_tree_declaring_no_sidecar_claims_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let debug = tmp.path().join("debug");
    fs::create_dir_all(&debug).expect("mkdir debug");
    // Leftovers from another build in the same slot.
    fs::write(debug.join(SHIM_EXE_FILENAME), b"other-build-shim").expect("shim");
    fs::write(debug.join(SESSION_CLI_EXE_FILENAME), b"other-build-cli").expect("cli");
    let every_bin: Vec<&str> = RUNNER_SIDECARS.iter().map(|s| s.bin).collect();

    for failure in [None, Some("cargo exit 101: error: no bin target named `x`")] {
        let verdict = sidecar_build_verdict(0, &debug, &every_bin, failure);
        assert!(verdict.built.is_empty(), "{failure:?}: {verdict:?}");
        assert!(
            verdict.warnings.is_empty(),
            "nothing was declared, so nothing may be claimed ({failure:?}): {verdict:?}"
        );
    }
}

/// The fallback WARN still fires where it is true: every DECLARED sidecar
/// is present, yet cargo failed — so one may be an earlier build's.
#[test]
fn sidecar_verdict_names_a_failed_build_whose_declared_sidecars_look_real() {
    let tmp = TempDir::new().expect("tempdir");
    let debug = tmp.path().join("debug");
    fs::create_dir_all(&debug).expect("mkdir debug");
    fs::write(debug.join(SHIM_EXE_FILENAME), b"shim").expect("shim");
    fs::write(debug.join(PROFILE_CLI_EXE_FILENAME), b"profile").expect("profile");

    // qontinui-pr undeclared (a ref predating it): not checked, not named.
    let verdict = sidecar_build_verdict(3, &debug, &["qontinui-pr"], Some("timed out after 600s"));
    assert_eq!(verdict.built.len(), 2, "{verdict:?}");
    assert_eq!(verdict.warnings.len(), 1, "{verdict:?}");
    let warning = &verdict.warnings[0];
    assert!(
        warning.contains("every sidecar this runner tree declares is present"),
        "{warning}"
    );
    assert!(warning.contains("timed out after 600s"), "{warning}");
    assert!(
        !warning.contains("qontinui-pr"),
        "an undeclared sidecar is not named: {warning}"
    );

    // Success: nothing to warn about.
    let clean = sidecar_build_verdict(3, &debug, &["qontinui-pr"], None);
    assert!(clean.warnings.is_empty(), "{clean:?}");
}

/// The post-build check reports EACH sidecar by name, and a zero-length
/// file is reported as `Empty`, never as a built binary.
#[test]
fn inspect_built_sidecars_reports_each_sidecar_and_refuses_zero_length() {
    let tmp = TempDir::new().expect("tempdir");
    let debug = tmp.path().join("debug");
    fs::create_dir_all(&debug).expect("mkdir debug");
    fs::write(debug.join(SHIM_EXE_FILENAME), b"real-shim").expect("write shim");
    // The exact incident artifact: tauri-build's copy of the 0-byte
    // `binaries/qontinui-pr-<triple>` placeholder.
    fs::write(debug.join(SESSION_CLI_EXE_FILENAME), b"").expect("write empty cli");
    // qontinui_profile: not produced at all.

    let verdicts: Vec<(&str, SidecarFile)> = inspect_built_sidecars(&debug)
        .into_iter()
        .map(|(sidecar, path, file)| {
            assert_eq!(path, debug.join(sidecar.filename));
            (sidecar.bin, file)
        })
        .collect();
    assert_eq!(
        verdicts,
        vec![
            ("qontinui-shim", SidecarFile::Present { bytes: 9 }),
            ("qontinui-pr", SidecarFile::Empty),
            ("qontinui_profile", SidecarFile::Missing),
        ]
    );
}

/// `remove_if_zero_length` removes a 0-byte file and nothing else: a file
/// with content, a missing path and a directory are all left alone.
#[test]
fn remove_if_zero_length_never_touches_content() {
    let tmp = TempDir::new().expect("tempdir");
    let empty = tmp.path().join("empty.exe");
    let real = tmp.path().join("real.exe");
    let dir = tmp.path().join("a-dir");
    fs::write(&empty, b"").expect("write empty");
    fs::write(&real, b"MZ").expect("write real");
    fs::create_dir_all(&dir).expect("mkdir");

    assert!(remove_if_zero_length(&empty));
    assert!(!empty.exists(), "the zero-length file must be removed");
    assert!(!remove_if_zero_length(&real));
    assert_eq!(fs::read(&real).expect("read real"), b"MZ");
    assert!(!remove_if_zero_length(&tmp.path().join("absent.exe")));
    assert!(!remove_if_zero_length(&dir));
    assert!(dir.is_dir());
}

/// LKG promotion carries EVERY roster sidecar, refuses a zero-length one,
/// and removes a zero-length copy an earlier build left in the LKG dir —
/// while a sidecar with content keeps being carried byte-for-byte.
#[tokio::test]
async fn lkg_carries_every_real_sidecar_and_never_a_zero_length_one() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canon root");
    let state = lkg_test_state(&root);
    let slot_exe = stage_slot0_exe(&state, b"runner-bytes");
    let slot_debug = slot_exe.parent().expect("slot debug").to_path_buf();
    fs::write(slot_debug.join(SHIM_EXE_FILENAME), b"real-shim").expect("shim");
    fs::write(slot_debug.join(SESSION_CLI_EXE_FILENAME), b"").expect("empty cli");
    fs::write(slot_debug.join(PROFILE_CLI_EXE_FILENAME), b"real-profile").expect("profile");

    // A zero-length session CLI already in the LKG dir, from a build that
    // predates the zero-length refusal.
    let lkg_dir = state.config.lkg_dir();
    fs::create_dir_all(&lkg_dir).expect("mkdir lkg");
    fs::write(lkg_dir.join(SESSION_CLI_EXE_FILENAME), b"").expect("stale empty cli");

    let slot = state.build_pool.slots[0].clone();
    let prov = live_provenance(Some("abc123def456"), "/ws/qontinui-runner");
    update_lkg_after_success(&state, &slot, &prov)
        .await
        .expect("a sidecar refusal must never fail the LKG promotion");

    assert_eq!(
        fs::read(state.config.lkg_exe_path()).unwrap(),
        b"runner-bytes"
    );
    assert_eq!(
        fs::read(lkg_dir.join(SHIM_EXE_FILENAME)).unwrap(),
        b"real-shim"
    );
    assert_eq!(
        fs::read(lkg_dir.join(PROFILE_CLI_EXE_FILENAME)).unwrap(),
        b"real-profile"
    );
    assert!(
        !lkg_dir.join(SESSION_CLI_EXE_FILENAME).exists(),
        "a zero-length session CLI must never be carried into, or left in, the LKG dir"
    );
    let litter: Vec<_> = fs::read_dir(&lkg_dir)
        .expect("read lkg dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(litter.is_empty(), "tmp files left in lkg: {:?}", litter);
}

/// S1 (review round 1): the slot is released before LKG capture, so a
/// queued build can rewrite the slot's sidecar — tauri-build's 0-byte
/// placeholder copy — between the check and the copy. The carry must judge
/// the bytes it actually COPIED: nothing zero-length may be renamed into
/// the LKG dir, and an earlier real LKG copy stays in place.
#[test]
fn lkg_carry_refuses_a_source_emptied_between_check_and_copy() {
    let tmp = TempDir::new().expect("tempdir");
    let slot_debug = tmp.path().join("slot-0").join("debug");
    let lkg_dir = tmp.path().join("lkg");
    fs::create_dir_all(&slot_debug).expect("mkdir slot");
    fs::create_dir_all(&lkg_dir).expect("mkdir lkg");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    fs::write(&source_exe, b"runner").expect("runner");
    let src = slot_debug.join(SESSION_CLI_EXE_FILENAME);
    fs::write(&src, b"real-cli").expect("real cli at check time");
    let dst = lkg_dir.join(SESSION_CLI_EXE_FILENAME);
    fs::write(&dst, b"earlier-real-cli").expect("earlier LKG copy");

    // The concurrent build's placeholder copy lands after the check.
    let racing_copy = |from: &std::path::Path, to: &std::path::Path| {
        fs::write(from, b"")?;
        fs::copy(from, to)
    };
    let result = carry_sidecar_into_lkg_via(
        &source_exe,
        &lkg_dir,
        SESSION_CLI_EXE_FILENAME,
        0,
        racing_copy,
    );

    assert!(
        result.is_err(),
        "a zero-byte copy must be refused: {result:?}"
    );
    assert_eq!(
        fs::read(&dst).expect("read lkg cli"),
        b"earlier-real-cli",
        "the earlier real LKG copy must not be replaced by zero bytes"
    );
    let litter: Vec<_> = fs::read_dir(&lkg_dir)
        .expect("read lkg dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(litter.is_empty(), "tmp files left in lkg: {:?}", litter);
}

/// The same race with a zero-length copy already in the LKG dir: that
/// stale copy is removed rather than left in place.
#[test]
fn lkg_carry_race_removes_a_zero_length_lkg_copy() {
    let tmp = TempDir::new().expect("tempdir");
    let slot_debug = tmp.path().join("slot-0").join("debug");
    let lkg_dir = tmp.path().join("lkg");
    fs::create_dir_all(&slot_debug).expect("mkdir slot");
    fs::create_dir_all(&lkg_dir).expect("mkdir lkg");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    fs::write(&source_exe, b"runner").expect("runner");
    fs::write(slot_debug.join(SESSION_CLI_EXE_FILENAME), b"real-cli").expect("cli");
    let dst = lkg_dir.join(SESSION_CLI_EXE_FILENAME);
    fs::write(&dst, b"").expect("stale zero-length LKG copy");

    let racing_copy = |from: &std::path::Path, to: &std::path::Path| {
        fs::write(from, b"")?;
        fs::copy(from, to)
    };
    let result = carry_sidecar_into_lkg_via(
        &source_exe,
        &lkg_dir,
        SESSION_CLI_EXE_FILENAME,
        0,
        racing_copy,
    );
    assert!(
        result.is_err(),
        "a zero-byte copy must be refused: {result:?}"
    );
    assert!(
        !dst.exists(),
        "a zero-length LKG copy must not survive the refusal"
    );
}

/// The `from_working_tree` flag selects the build's source classification:
/// the working-tree path uses `BuildSourceKind::LiveTree` ⇒ `live_tree`;
/// the default origin/main path uses
/// `BuildSourceKind::OriginMain { resolved_sha }` ⇒ `origin_main`. This is
/// the pure classification seam the primary rebuild path threads — the kind
/// alone decides the recorded `BuildSource`, disambiguating an origin/main
/// primary build from a spawn-test override (both `Some(src_tauri)`).
#[test]
fn build_source_kind_classifies_working_tree_vs_origin_main() {
    // from_working_tree:true → live tree.
    assert_eq!(
        BuildSourceKind::LiveTree.build_source(),
        BuildSource::LiveTree
    );
    // from_working_tree:false (default) → origin/main.
    assert_eq!(
        BuildSourceKind::OriginMain {
            resolved_sha: "deadbeef".to_string(),
        }
        .build_source(),
        BuildSource::OriginMain
    );
    // spawn-test foreign override stays Override (unchanged).
    assert_eq!(
        BuildSourceKind::Override.build_source(),
        BuildSource::Override
    );

    // The vouched predicate: working-tree + origin/main promote; override
    // does not.
    assert!(BuildSourceKind::LiveTree.build_source().is_vouched());
    assert!(BuildSourceKind::OriginMain {
        resolved_sha: "x".to_string()
    }
    .build_source()
    .is_vouched());
    assert!(!BuildSourceKind::Override.build_source().is_vouched());
}

// ---------- Issue 3: stderr classifier + submission tail ----------

/// A stderr carrying a real `error[E####]` diagnostic code classifies as a
/// compiler diagnostic — the user's code is broken, no poisoned-slot retry.
#[test]
fn classify_compiler_error_code_is_diagnostic() {
    let stderr = "   Compiling qontinui-runner v0.1.0\n\
             error[E0432]: unresolved import `crate::does_not_exist`\n\
              --> src/main.rs:3:5\n\
             error: aborting due to previous error\n";
    assert_eq!(
        classify_build_stderr(stderr),
        StderrClass::CompilerDiagnostic
    );
}

/// Cargo's terminal `could not compile` summary also classifies as a
/// compiler diagnostic even if the `error[E####]` line was truncated off
/// the captured tail.
#[test]
fn classify_could_not_compile_is_diagnostic() {
    let stderr = "   Compiling qontinui-runner v0.1.0\n\
             error: could not compile `qontinui-runner` (bin \"qontinui-runner\") due to 1 previous error\n";
    assert_eq!(
        classify_build_stderr(stderr),
        StderrClass::CompilerDiagnostic
    );
}

/// A failure with ONLY "Compiling …" progress noise + a linker/fingerprint
/// error and NO compiler diagnostic classifies as environmental — the
/// poisoned-slot self-heal should fire and retry in a cleaned slot. This is
/// the exact 2 KB-tail surface the user saw (`Compiling qontinui-runner …`).
#[test]
fn classify_environmental_noise_is_environmental() {
    let stderr =
        "   Compiling qontinui-runner v0.1.0 (D:\\qontinui-root\\qontinui-runner\\src-tauri)\n\
             error: linking with `link.exe` failed: exit code: 1104\n\
             LINK : fatal error LNK1104: cannot open file 'qontinui_runner.exe'\n";
    assert_eq!(classify_build_stderr(stderr), StderrClass::Environmental);
}

/// A bare `error:` line (no `error[E####]`, no `could not compile`) — e.g. a
/// "could not find Cargo.toml" environmental failure — must NOT be misread
/// as a compiler diagnostic, or the self-heal would never fire.
#[test]
fn classify_bare_error_line_is_environmental() {
    let stderr = "error: could not find `Cargo.toml` in `/tmp/x` or any parent directory\n";
    assert_eq!(classify_build_stderr(stderr), StderrClass::Environmental);
}

/// The submission tail returns the input unchanged when it's under the cap,
/// and a boundary-safe tail (≤ cap bytes, preserving the END where cargo's
/// real error lives) when it's over.
#[test]
fn stderr_submission_tail_caps_and_keeps_tail() {
    let small = "error[E0277]: trait bound not satisfied";
    assert_eq!(stderr_submission_tail(small), small);

    let big =
        "x".repeat(LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES * 2) + "\nerror[E0599]: tail marker";
    let tail = stderr_submission_tail(&big);
    assert!(tail.len() <= LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES);
    assert!(
        tail.ends_with("error[E0599]: tail marker"),
        "tail must preserve the END of the stderr where the real error lives"
    );
}

// -----------------------------------------------------------------------------
// QONTINUI_ALLOW_PLACEHOLDER_DIST: pre-warm only
// (plan 2026-10-05-supervisor-first-start-embeds-placeholder-frontend)
// -----------------------------------------------------------------------------

#[test]
fn prewarm_cargo_check_carries_allow_placeholder_dist() {
    let env = super::cargo_invocation_env(super::CargoInvocation::Prewarm);
    assert!(
        env.set.contains(&("QONTINUI_ALLOW_PLACEHOLDER_DIST", "1")),
        "the pre-warm `cargo check` must keep working on a fresh tree with no dist/: {env:?}"
    );
    assert!(!env.remove.contains(&"QONTINUI_ALLOW_PLACEHOLDER_DIST"));
}

#[test]
fn real_builds_never_carry_and_actively_remove_allow_placeholder_dist() {
    for kind in [
        super::CargoInvocation::SlotBuild,
        super::CargoInvocation::SidecarBuild,
        super::CargoInvocation::Submission,
    ] {
        let env = super::cargo_invocation_env(kind);
        assert!(
            !env.set
                .iter()
                .any(|(k, _)| *k == "QONTINUI_ALLOW_PLACEHOLDER_DIST"),
            "{kind:?} produces an exe and must let build.rs refuse a placeholder dist: {env:?}"
        );
        assert!(
            env.remove.contains(&"QONTINUI_ALLOW_PLACEHOLDER_DIST"),
            "{kind:?} must REMOVE an inherited QONTINUI_ALLOW_PLACEHOLDER_DIST: {env:?}"
        );
    }
}

/// The `/build/submit` path is a plain tokio `Command`: the removal must land
/// on it as a removal (`get_envs` reports a removed key as `(key, None)`).
#[test]
fn submission_command_removes_allow_placeholder_dist() {
    let mut cmd = tokio::process::Command::new("cargo");
    super::apply_invocation_env(&mut cmd, super::CargoInvocation::Submission);
    let removed = cmd
        .as_std()
        .get_envs()
        .any(|(k, v)| k == "QONTINUI_ALLOW_PLACEHOLDER_DIST" && v.is_none());
    assert!(removed, "submission build must env_remove the override");
}

/// The pure function is only half the rule: each call site must ask for its
/// OWN kind. Read the production source (everything before the test module)
/// and pin which kind each cargo invocation passes, so wiring the pre-warm
/// kind into a real build — or dropping it from the pre-warm — goes red.
#[test]
fn each_cargo_call_site_applies_its_own_invocation_kind() {
    let src = include_str!("../build_monitor.rs");
    let prod = &src[..src
        .find(&["#[cfg(test)]\nmod ", "tests;"].concat())
        .expect("build_monitor.rs must still declare its tests module")];
    let count = |needle: &str| prod.matches(needle).count();
    // One use each, at the call site (the enum declaration and the match in
    // `cargo_invocation_env` spell `Prewarm =>` / `SlotBuild |` / `SidecarBuild =>`).
    assert_eq!(count("CargoInvocation::Prewarm)"), 1);
    assert_eq!(count("CargoInvocation::SlotBuild)"), 1);
    assert_eq!(count("CargoInvocation::SidecarBuild)"), 1);
    let subm = include_str!("../build_submissions.rs");
    assert_eq!(
        subm.matches("CargoInvocation::Submission,").count(),
        1,
        "build_submissions.rs must apply the Submission env rules to its cargo command"
    );
    // The pre-warm site is the one that runs `cargo check`.
    let at = prod.find("CargoInvocation::Prewarm)").unwrap();
    let before = &prod[..at];
    let fn_start = before
        .rfind("\nasync fn ")
        .or_else(|| before.rfind("\npub async fn "))
        .unwrap();
    assert!(
        prod[fn_start..at].contains("\"check\","),
        "CargoInvocation::Prewarm must be applied inside the pre-warm `cargo check` fn"
    );
}

// ---------------------------------------------------------------
// The supervisor never installs through a linked `node_modules` —
// plan `2026-10-06-a-junctioned-node-modules-install-is-refused-by-every-harness-and-noticed-at-boot`,
// Phase 3 (D2). A worktree whose `node_modules` is a symlink into the
// shared primary must get the LINK removed before `pnpm install`, so
// the install lands in a real dir and the primary is byte-identical.
//
// Runs on unix (a symlink) AND Windows (a junction, `mklink /J` — the
// shape the allocator's link arm actually produces on MSYS). CI is
// ubuntu-only, so the Windows half runs on a Windows dev box; it is the
// only coverage `remove_link`'s `remove_dir`-on-a-reparse-point branch has.
// ---------------------------------------------------------------
#[cfg(any(unix, windows))]
mod unlink_before_install {
    use super::super::{
        prebuild_worktree_frontend_with, unlink_node_modules_link, UNLINKED_NODE_MODULES_LOG,
    };
    use super::lkg_test_state;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Make `link` a directory link to `target`: a symlink on unix, a
    /// junction on Windows (no privilege needed, unlike `symlink_dir`).
    #[cfg(unix)]
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link.as_ref())
            .arg(target.as_ref())
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "mklink /J failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )))
        }
    }

    /// Every regular file under `root` (not following links) → its bytes.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).expect("read_dir") {
                let p = entry.expect("entry").path();
                let meta = fs::symlink_metadata(&p).expect("meta");
                if meta.is_dir() {
                    walk(root, &p, out);
                } else {
                    let bytes = if meta.file_type().is_symlink() {
                        fs::read_link(&p)
                            .expect("read_link")
                            .to_string_lossy()
                            .into_owned()
                            .into_bytes()
                    } else {
                        fs::read(&p).expect("read")
                    };
                    out.insert(p.strip_prefix(root).unwrap().to_path_buf(), bytes);
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// A pnpm primary: a real `node_modules` holding a `.modules.yaml`
    /// sentinel naming itself, plus the install marker.
    fn seed_primary(prim: &Path) {
        let nm = prim.join("node_modules");
        fs::create_dir_all(nm.join(".bin")).unwrap();
        fs::write(
            nm.join(".modules.yaml"),
            "virtualStoreDir: .pnpm\n# owner: prim\n",
        )
        .unwrap();
        fs::write(nm.join(".bin").join("ui-bridge-build-ir"), b"prim-bin").unwrap();
        fs::write(prim.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
        fs::write(prim.join("package.json"), r#"{"name":"prim"}"#).unwrap();
    }

    fn ok_output() -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![],
            stderr: vec![],
        }
    }

    #[test]
    fn unlink_removes_only_the_link_and_never_its_target() {
        let tmp = TempDir::new().unwrap();
        let prim = tmp.path().join("prim");
        let wt = tmp.path().join("wt");
        seed_primary(&prim);
        fs::create_dir_all(&wt).unwrap();
        symlink(prim.join("node_modules"), wt.join("node_modules")).unwrap();
        let before = snapshot(&prim);

        assert!(
            unlink_node_modules_link(&wt).expect("unlink"),
            "a link must be removed"
        );
        assert!(
            fs::symlink_metadata(wt.join("node_modules")).is_err(),
            "wt/node_modules must be gone after the unlink"
        );
        assert_eq!(
            snapshot(&prim),
            before,
            "the link's target must be untouched"
        );
    }

    #[test]
    fn unlink_removes_a_dangling_link() {
        let tmp = TempDir::new().unwrap();
        // Link to a real dir, then delete the target: the dangling shape a
        // reaped worktree's primary is left with, on either platform.
        fs::create_dir_all(tmp.path().join("gone")).unwrap();
        symlink(tmp.path().join("gone"), tmp.path().join("node_modules")).unwrap();
        fs::remove_dir(tmp.path().join("gone")).unwrap();
        assert!(unlink_node_modules_link(tmp.path()).expect("unlink"));
        assert!(fs::symlink_metadata(tmp.path().join("node_modules")).is_err());
    }

    /// The post-`npm_lock` re-check calls the helper a second time; on a
    /// tree the first call already fixed (or a real dir) it must be a no-op.
    #[test]
    fn unlink_is_idempotent_and_a_second_call_after_install_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let prim = tmp.path().join("prim");
        let wt = tmp.path().join("wt");
        seed_primary(&prim);
        fs::create_dir_all(&wt).unwrap();
        symlink(prim.join("node_modules"), wt.join("node_modules")).unwrap();

        assert!(unlink_node_modules_link(&wt).expect("first call"));
        assert!(!unlink_node_modules_link(&wt).expect("second call, absent"));
        fs::create_dir_all(wt.join("node_modules")).unwrap();
        fs::write(wt.join("node_modules/.modules.yaml"), "own").unwrap();
        assert!(!unlink_node_modules_link(&wt).expect("third call, real dir"));
        assert_eq!(
            fs::read_to_string(wt.join("node_modules/.modules.yaml")).unwrap(),
            "own"
        );
    }

    #[test]
    fn unlink_is_a_noop_on_a_real_dir_and_on_absence() {
        let tmp = TempDir::new().unwrap();
        assert!(!unlink_node_modules_link(tmp.path()).expect("absent is fine"));

        seed_primary(tmp.path());
        let before = snapshot(tmp.path());
        assert!(
            !unlink_node_modules_link(tmp.path()).expect("real dir"),
            "a real node_modules is not a link and must be left alone"
        );
        assert_eq!(
            snapshot(tmp.path()),
            before,
            "a real node_modules must be untouched"
        );
    }

    /// The whole prebuild, pnpm stubbed: the stub writes what a real
    /// `pnpm install` / `pnpm run build` would write into `cwd`. Without the
    /// unlink, those writes go THROUGH the link into `prim` — so the mutant
    /// that skips the unlink fails both the real-dir and the byte-identical
    /// assertions.
    #[tokio::test]
    async fn prebuild_unlinks_a_linked_node_modules_and_installs_into_a_real_dir() {
        let tmp = TempDir::new().unwrap();
        let state = lkg_test_state(&tmp.path().join("live"));
        let slot = state.build_pool.slots[0].clone();

        let prim = tmp.path().join("prim");
        let wt = tmp.path().join("wt");
        seed_primary(&prim);
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
        fs::write(wt.join("package.json"), r#"{"name":"wt"}"#).unwrap();
        symlink(prim.join("node_modules"), wt.join("node_modules")).unwrap();
        let prim_before = snapshot(&prim);

        let calls: Arc<Mutex<Vec<(PathBuf, String)>>> = Arc::default();
        let rec = calls.clone();
        prebuild_worktree_frontend_with(&state, &slot, &wt, false, move |cwd, args| {
            rec.lock().unwrap().push((cwd.clone(), args.to_string()));
            async move {
                if args.starts_with("install") {
                    let nm = cwd.join("node_modules");
                    fs::create_dir_all(nm.join(".bin"))?;
                    fs::write(
                        nm.join(".modules.yaml"),
                        "virtualStoreDir: .pnpm\n# owner: wt\n",
                    )?;
                    fs::write(nm.join(".bin").join("ui-bridge-build-ir"), b"wt-bin")?;
                } else {
                    let dist = cwd.join("dist");
                    fs::create_dir_all(&dist)?;
                    fs::write(dist.join("index.html"), b"<!doctype html>")?;
                    fs::write(dist.join("build-id.txt"), b"test-build-id")?;
                }
                Ok(ok_output())
            }
        })
        .await
        .expect("prebuild succeeds");

        let wt_nm = fs::symlink_metadata(wt.join("node_modules")).expect("wt/node_modules exists");
        assert!(
            wt_nm.is_dir() && !wt_nm.file_type().is_symlink(),
            "wt/node_modules must be a REAL dir after the prebuild"
        );
        assert_eq!(
            fs::read_to_string(wt.join("node_modules/.modules.yaml")).unwrap(),
            "virtualStoreDir: .pnpm\n# owner: wt\n"
        );
        assert_eq!(
            snapshot(&prim),
            prim_before,
            "the primary must be byte-identical"
        );

        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls.first().map(|(c, a)| (c.as_path(), a.as_str())),
            Some((wt.as_path(), "install --frozen-lockfile")),
            "the install must run (in wt) — the unlink leaves no marker to call it fresh"
        );

        let logged = state
            .logs
            .build_history()
            .await
            .iter()
            .any(|e| e.message.contains(UNLINKED_NODE_MODULES_LOG));
        assert!(
            logged,
            "the unlink must be logged with the Arming-line text"
        );
    }
}

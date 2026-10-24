//! Tests for the optimized primary's start decisions (plan
//! `2026-10-19-primary-runner-is-a-debug-build-and-nothing-shows-it`, Phase 5,
//! review round 1). The build step and the profile support are injected, so
//! every arm runs without cargo.

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::build_profile::RunnerBuildProfile;
use crate::config::RUNNER_BIN_NAME;

const WITHOUT_PROFILE: &str = "[workspace]\nmembers = []\n[profile.release]\nopt-level = 2\n";

fn with_profile() -> String {
    format!("{WITHOUT_PROFILE}[profile.release-checked]\ninherits = \"release\"\n")
}

/// A state whose runner tree lives under `root`. The profile is passed
/// explicitly so an ambient `QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE`
/// cannot change the fixture.
fn state_under(root: &std::path::Path) -> SharedState {
    use clap::Parser;
    let src_tauri = root.join("qontinui-runner").join("src-tauri");
    std::fs::create_dir_all(&src_tauri).expect("mkdir");
    let project_dir = src_tauri.to_string_lossy().into_owned();
    let config = crate::config::SupervisorConfig::from_args(crate::config::CliArgs::parse_from([
        "test",
        "--project-dir",
        project_dir.as_str(),
        "--primary-build-profile",
        "release-checked",
    ]));
    let mut config = config;
    // A fixed two-slot pool, so no test depends on this machine's sizing.
    config.build_pool = crate::config::BuildPoolConfig::fixed(2);
    Arc::new(crate::state::SupervisorState::new(config))
}

fn write_file(p: &std::path::Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn write_lkg(state: &SharedState, profile: RunnerBuildProfile) {
    let dir = state.config.lkg_dir(profile);
    write_file(&dir.join(RUNNER_BIN_NAME), b"lkg");
    std::fs::write(
        dir.join("lkg.json"),
        format!(
            r#"{{"built_at":"2026-10-24T01:00:00Z","source_slot":0,"exe_size":3,"sha":"abc","source":"origin_main","profile":"{}"}}"#,
            profile.cargo_name()
        ),
    )
    .unwrap();
}

/// A build step that must never run.
fn no_build() -> impl FnOnce() -> std::future::Ready<Result<(), SupervisorError>> {
    || panic!("this start must not build in the foreground")
}

/// Whether any entry, in EITHER log buffer, contains `needle`: a
/// `LogSource::Build` entry lands only in the build buffer, which `history()`
/// does not read.
async fn logged(state: &SharedState, needle: &str) -> bool {
    state
        .logs
        .merged_history()
        .await
        .iter()
        .any(|e| e.message.contains(needle))
}

fn primary_copy_dir(state: &SharedState, profile: RunnerBuildProfile) -> String {
    let primary = crate::config::RunnerConfig::default_primary();
    let p = state.config.runner_exe_copy_path_for(&primary, profile);
    p.strip_prefix(state.config.runner_npm_dir())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

// ── B1: the build tree must declare the profile ─────────────────────────────

#[test]
fn profile_support_names_a_tree_without_release_checked() {
    let rc = RunnerBuildProfile::ReleaseChecked;
    assert_eq!(
        profile_support_from_manifest(Some(&with_profile()), "x", rc),
        ProfileSupport::Supported
    );
    match profile_support_from_manifest(Some(WITHOUT_PROFILE), "origin/main:Cargo.toml", rc) {
        ProfileSupport::Missing(cause) => {
            assert!(cause.contains("[profile.release-checked]"), "{cause}");
            assert!(cause.contains("origin/main:Cargo.toml"), "{cause}");
            assert!(cause.contains("Phase 3"), "{cause}");
        }
        other => panic!("{other:?}"),
    }
    match profile_support_from_manifest(None, "x", rc) {
        ProfileSupport::Missing(cause) => assert!(cause.contains("could not be read"), "{cause}"),
        other => panic!("an unreadable manifest is not support: {other:?}"),
    }
}

#[tokio::test]
async fn build_tree_profile_support_reads_the_live_tree_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let rc = RunnerBuildProfile::ReleaseChecked;
    let live = BuildTree::LiveWorkingTree;
    assert!(!build_tree_profile_support(&state, rc, live)
        .await
        .is_supported());
    let manifest = state.config.runner_npm_dir().join("Cargo.toml");
    std::fs::write(&manifest, WITHOUT_PROFILE).unwrap();
    assert!(!build_tree_profile_support(&state, rc, live)
        .await
        .is_supported());
    std::fs::write(&manifest, with_profile()).unwrap();
    assert!(build_tree_profile_support(&state, rc, live)
        .await
        .is_supported());
    // dev is cargo's built-in profile.
    std::fs::write(&manifest, "").unwrap();
    assert!(
        build_tree_profile_support(&state, RunnerBuildProfile::Dev, live)
            .await
            .is_supported()
    );
}

/// The pre-check runs before the build lock, the worktree prep and any
/// frontend build: a tree with no `[profile.release-checked]` is refused with
/// the cause named, and the primary build dir is never touched.
#[tokio::test]
async fn primary_build_refuses_up_front_when_the_tree_lacks_the_profile() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    std::fs::write(
        state.config.runner_npm_dir().join("Cargo.toml"),
        WITHOUT_PROFILE,
    )
    .unwrap();
    let err = primary_build(&state, BuildTree::LiveWorkingTree, BuildNeed::Always)
        .await
        .expect_err("no profile, no build");
    let msg = err.to_string();
    assert!(msg.contains("unavailable"), "{msg}");
    assert!(msg.contains("[profile.release-checked]"), "{msg}");
    assert!(state.build_pool.primary_slot.busy.read().await.is_none());
    assert!(!state.config.primary_target_dir().exists());
    assert!(state.build_pool.primary_build_lock.try_lock().is_ok());
}

// ── M2 / m5: an unattended start never waits for a cold build ───────────────

#[tokio::test]
async fn an_unattended_start_takes_the_dev_lkg_at_once_and_kicks_a_background_build() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    write_lkg(&state, RunnerBuildProfile::Dev);

    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        no_build(),
    )
    .await
    .expect("an unattended start falls back");
    assert_eq!(p.resolved.profile, RunnerBuildProfile::Dev);
    assert_eq!(
        p.resolved.origin,
        ExeOrigin::LkgFallback(RunnerBuildProfile::Dev)
    );
    assert_eq!(
        p.resolved.path,
        state.config.lkg_exe_path(RunnerBuildProfile::Dev)
    );
    let why = p
        .resolved
        .profile_fallback
        .as_deref()
        .expect("fallback reason");
    assert!(why.contains("UNATTENDED"), "{why}");
    assert!(p.kick_background_build);
    assert!(logged(&state, "DEBUG PRIMARY FALLBACK").await);
    // The deployed copy lands in the directory of what it holds.
    assert_eq!(
        primary_copy_dir(&state, p.resolved.profile),
        format!(
            "target/debug/qontinui-runner-primary{}",
            std::env::consts::EXE_SUFFIX
        )
    );
}

/// Round 2, item 1: a COLD box (nothing at all to start) is not refused — an
/// unattended start builds in the foreground, because nothing else could be
/// started (a restart still stops the old process only after the build).
#[tokio::test]
async fn an_unattended_start_on_a_cold_box_builds_in_the_foreground() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let built = state
        .config
        .primary_exe_path(RunnerBuildProfile::ReleaseChecked);
    let target = built.clone();
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        move || {
            write_file(&target, b"cold-built");
            std::future::ready(Ok(()))
        },
    )
    .await
    .expect("a cold box builds");
    assert_eq!(
        p.resolved.origin,
        ExeOrigin::PrimaryBuildDir(RunnerBuildProfile::ReleaseChecked)
    );
    assert_eq!(p.resolved.path, built);
    assert!(p.resolved.profile_fallback.is_none());
    assert!(!p.kick_background_build);
}

/// m5 nit: a dev slot whose provenance says `override` is never the last
/// resort while another slot is clean.
#[tokio::test]
async fn an_override_dev_slot_is_skipped_for_a_clean_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    for id in [0, 1] {
        write_file(&state.config.runner_exe_path_for_slot(id), b"dev-slot");
    }
    let override_prov = r#"{"sha":"f00","source":"override","built_from":"/ws/.spawn-feat","built_at":"2026-10-24T00:00:00Z"}"#;
    std::fs::write(
        state
            .config
            .runner_exe_path_for_slot(0)
            .with_file_name(crate::process::manager::SLOT_PROVENANCE_SIDECAR_FILENAME),
        override_prov,
    )
    .unwrap();
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        no_build(),
    )
    .await
    .unwrap();
    assert_eq!(p.resolved.origin, ExeOrigin::Slot(1));
}

/// m5: with no dev LKG an unattended start takes an existing dev slot exe.
#[tokio::test]
async fn an_unattended_start_with_no_dev_lkg_takes_a_dev_slot_exe() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let slot0 = state.config.runner_exe_path_for_slot(0);
    write_file(&slot0, b"dev-slot");
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        no_build(),
    )
    .await
    .expect("the dev slot exe is the last resort");
    assert_eq!(p.resolved.origin, ExeOrigin::Slot(0));
    assert_eq!(p.resolved.path, slot0);
    assert_eq!(p.resolved.profile, RunnerBuildProfile::Dev);
    assert!(p.resolved.profile_fallback.is_some());
    assert!(logged(&state, "DEBUG PRIMARY FALLBACK").await);
}

/// A cold box whose foreground build fails is refused, naming the failure.
#[tokio::test]
async fn an_unattended_cold_box_whose_build_fails_refuses_naming_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let err = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        || std::future::ready(Err(SupervisorError::BuildFailed("cold-boom".to_string()))),
    )
    .await
    .expect_err("nothing to start");
    let msg = err.to_string();
    assert!(msg.contains("cold-boom"), "{msg}");
    assert!(msg.contains("no dev artifact"), "{msg}");
}

/// The release-checked LKG is still optimized, so it comes before any dev
/// artifact, with no fallback reason.
#[tokio::test]
async fn the_release_checked_lkg_beats_every_dev_artifact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    write_lkg(&state, RunnerBuildProfile::Dev);
    write_lkg(&state, RunnerBuildProfile::ReleaseChecked);
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        no_build(),
    )
    .await
    .unwrap();
    assert_eq!(
        p.resolved.origin,
        ExeOrigin::LkgFallback(RunnerBuildProfile::ReleaseChecked)
    );
    assert_eq!(p.resolved.profile, RunnerBuildProfile::ReleaseChecked);
    assert!(p.resolved.profile_fallback.is_none());
    assert!(p.kick_background_build, "refresh the optimized artifact");
}

#[tokio::test]
async fn a_startable_artifact_is_used_without_building_or_kicking() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let built = state
        .config
        .primary_exe_path(RunnerBuildProfile::ReleaseChecked);
    write_file(&built, b"optimized");
    for intent in [StartIntent::Operator, StartIntent::Unattended] {
        let p = prepare_optimized_primary(
            &state,
            RunnerBuildProfile::ReleaseChecked,
            intent,
            no_build(),
        )
        .await
        .unwrap();
        assert_eq!(p.resolved.path, built);
        assert!(!p.kick_background_build);
    }
}

// ── The operator arm: build now, never fall back to debug ───────────────────

#[tokio::test]
async fn an_operator_build_failure_refuses_and_never_takes_the_dev_lkg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    write_lkg(&state, RunnerBuildProfile::Dev);
    let built = Arc::new(AtomicBool::new(false));
    let flag = built.clone();
    let err = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Operator,
        move || {
            flag.store(true, Ordering::SeqCst);
            std::future::ready(Err(SupervisorError::BuildFailed("boom".to_string())))
        },
    )
    .await
    .expect_err("an operator start refuses");
    assert!(
        built.load(Ordering::SeqCst),
        "the operator start built first"
    );
    let msg = err.to_string();
    assert!(msg.contains("boom"), "{msg}");
    assert!(
        msg.contains("never falls back to a debug/ artifact"),
        "{msg}"
    );
    assert!(!logged(&state, "DEBUG PRIMARY FALLBACK").await);
}

#[tokio::test]
async fn an_operator_build_success_starts_the_fresh_artifact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let built = state
        .config
        .primary_exe_path(RunnerBuildProfile::ReleaseChecked);
    let target = built.clone();
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Operator,
        move || {
            write_file(&target, b"fresh");
            std::future::ready(Ok(()))
        },
    )
    .await
    .unwrap();
    assert_eq!(
        p.resolved.origin,
        ExeOrigin::PrimaryBuildDir(RunnerBuildProfile::ReleaseChecked)
    );
    assert_eq!(p.resolved.path, built);
}

#[tokio::test]
async fn an_operator_start_on_a_tree_without_the_profile_refuses_naming_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    write_lkg(&state, RunnerBuildProfile::Dev);
    // The build step carries the profile pre-check (`primary_build`), so an
    // unsupported tree arrives as a named build failure.
    let err = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Operator,
        || {
            std::future::ready(Err(SupervisorError::BuildFailed(
                "the release-checked primary build is unavailable: the tree declares no \
                 [profile.release-checked]"
                    .to_string(),
            )))
        },
    )
    .await
    .expect_err("refused");
    assert!(
        err.to_string().contains("[profile.release-checked]"),
        "{err}"
    );
}

// ── Wiring, pinned by scanning the source it lives in ───────────────────────

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn fn_body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let a = src
        .find(start)
        .unwrap_or_else(|| panic!("{start} not found"));
    let b = src[a..]
        .find(end)
        .unwrap_or_else(|| panic!("{end} not found"))
        + a;
    &src[a..b]
}

/// M1: `restart_after_gate` resolves (and, when asked, builds) the optimized
/// primary's artifact, and the stop lives only in the half it calls AFTER —
/// so nothing is stopped while there is nothing startable.
#[test]
fn a_restart_prepares_the_optimized_primary_before_it_stops_anything() {
    let src = include_str!("../manager.rs");
    let gate = fn_body(
        src,
        "async fn restart_after_gate(",
        "async fn stop_rebuild_and_start(",
    );
    let prepare = gate.find("prepare_primary_start").expect("prepares");
    let early_build = gate.find("BuildNeed::Always").expect("builds early");
    let hand_off = gate
        .find("stop_rebuild_and_start(")
        .expect("then stops and starts");
    assert!(early_build < prepare && prepare < hand_off);
    assert!(
        !gate.contains("stop_runner_by_id"),
        "restart_after_gate must not stop before preparing"
    );
    let rest = fn_body(
        src,
        "async fn stop_rebuild_and_start(",
        "pub async fn stop_runner(",
    );
    assert!(rest.contains("stop_runner_by_id"));
}

/// The boot `--auto-start` and the crash watchdog are UNATTENDED starts.
#[test]
fn boot_start_and_crash_restart_are_unattended() {
    let main = strip_ws(include_str!("../../main.rs"));
    let boot = fn_body(&main, "ifauto_start{", "auto-start:failedtoboot-start");
    assert!(
        boot.contains("start_runner_by_id(&state_for_autostart,&primary_id,process::manager::StartIntent::Unattended,)"),
        "{boot}"
    );
    let manager = strip_ws(include_str!("../manager.rs"));
    let crash = fn_body(
        &manager,
        "asyncfnmaybe_crash_restart(",
        "asyncfnwatch_first_healthy(",
    );
    assert!(
        crash.contains("start_runner_by_id(&state,&runner_id,StartIntent::Unattended)"),
        "the crash watchdog must start unattended"
    );
}

#[test]
fn only_the_serving_watchdog_restart_is_unattended() {
    assert_eq!(
        StartIntent::of_restart(&RestartSource::ServingWatchdog),
        StartIntent::Unattended
    );
    assert_eq!(
        StartIntent::of_restart(&RestartSource::Watchdog),
        StartIntent::Operator
    );
    assert_eq!(
        StartIntent::of_restart(&RestartSource::Manual),
        StartIntent::Operator
    );
}

/// Round 2 items 2, 4, 7, 8, pinned at source level (they order locks and
/// tasks that a unit test cannot drive without cargo).
#[test]
fn the_primary_build_orders_its_locks_and_detaches_its_task() {
    let src = include_str!("../primary_start.rs");
    let build = fn_body(
        src,
        "pub async fn primary_build(",
        "pub async fn primary_build_detached(",
    );
    let optimized = &build[build.find("let lock = state").expect("optimized arm")..];
    // primary lock -> container lock -> one resolved sha -> profile check at it
    // -> worktree at the SAME sha (the permit is taken inside run_primary_build).
    let order = [
        "primary_build_lock",
        "spawn_container_lock",
        "origin_main_build_point",
        "profile_support_from_manifest",
        "prepare_worktree_at(&project_dir, \"origin/main\", &sha)",
        "run_primary_build",
    ];
    let mut at = 0;
    for needle in order {
        let i = optimized[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing or out of order"))
            + at;
        at = i;
    }
    let point = fn_body(
        src,
        "async fn origin_main_build_point(",
        "async fn git_stdout(",
    );
    let fetch = point.find("fetch_origin").expect("bounded fetch");
    let rev = point.find("rev-parse").expect("resolve once");
    let show = point.find("show").expect("read at the sha");
    assert!(fetch < rev && rev < show);
    let prepare = fn_body(
        src,
        "pub async fn prepare_primary_start(",
        "pub async fn prepare_optimized_primary<",
    );
    assert!(
        prepare.contains("primary_build_detached"),
        "the start-path build must be detached"
    );
    let detached = fn_body(
        src,
        "pub async fn primary_build_detached(",
        "async fn log_origin_main_build(",
    );
    assert!(detached.contains("tokio::spawn"));
    let kick = fn_body(
        src,
        "pub fn spawn_background_primary_build(",
        "/// What [`prepare_optimized_primary`] decided.",
    );
    assert!(
        kick.contains("BackgroundBuildLatch"),
        "the in-flight flag needs a Drop guard"
    );
}

/// Round 2 item 2: the predicate a synchronous door uses to file a no-rebuild
/// restart as the build it will run.
#[tokio::test]
async fn primary_start_needs_build_only_when_nothing_is_startable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    assert!(primary_start_needs_build(&state).await);
    // An LKG is a fallback, not a startable build: the operator path still builds.
    write_lkg(&state, RunnerBuildProfile::ReleaseChecked);
    assert!(primary_start_needs_build(&state).await);
    write_file(
        &state
            .config
            .primary_exe_path(RunnerBuildProfile::ReleaseChecked),
        b"optimized",
    );
    assert!(!primary_start_needs_build(&state).await);
}

// ── Review round 3 ──────────────────────────────────────────────────────────

/// A registered, UNPROTECTED primary on a port nothing listens on, so the
/// readiness gate skips and a restart reaches its latch without probing
/// anything real.
async fn register_dead_primary(state: &SharedState) -> Arc<crate::state::ManagedRunner> {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let mut config = crate::config::RunnerConfig::default_primary();
    config.port = port;
    config.protected = false;
    let id = config.id.clone();
    let managed = Arc::new(crate::state::ManagedRunner::new(config, false));
    state.runners.write().await.insert(id, managed.clone());
    managed
}

/// Wait (bounded) until `cond` holds.
async fn eventually<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !cond().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Finding 1: a caller dropped MID-PREPARE (a disconnected HTTP client while
/// a no-rebuild restart cold-builds) neither abandons the restart nor strands
/// `restart_requested`, which the serving watchdog reads as operator intent.
/// The held build lock parks the restart inside its cold build; the caller is
/// dropped there; the restart still runs to its end (the build fails on this
/// fixture's missing schemas sibling, so the start is refused) and the latch
/// comes off.
#[tokio::test]
async fn a_restart_dropped_mid_prepare_still_clears_its_latch() {
    use crate::process::manager::{restart_runner_by_id, CloseIntent};
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let managed = register_dead_primary(&state).await;
    let build_lock = state
        .build_pool
        .primary_build_lock
        .clone()
        .lock_owned()
        .await;

    {
        let restart = restart_runner_by_id(
            &state,
            "primary",
            false,
            RestartSource::Manual,
            false,
            BuildTree::OriginMain,
            CloseIntent::Kill,
        );
        tokio::pin!(restart);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            tokio::select! {
                biased;
                r = &mut restart => panic!("the restart must park on the held build lock: {r:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            if managed.runner.read().await.restart_requested {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the restart never latched"
            );
        }
        // Let it reach the parked build, then drop the caller.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(managed.runner.read().await.restart_requested);
    }

    drop(build_lock);
    eventually(
        "the latch comes off after the dropped restart ends",
        || async { !managed.runner.read().await.restart_requested },
    )
    .await;
    // The restart RAN TO ITS END after its caller was gone: only the latched
    // half (`restart_in_flight`) records a `RestartFailed` once the gate has
    // admitted the request. A latch that merely cleared on drop, with the
    // restart abandoned, would record nothing.
    let state2 = state.clone();
    eventually(
        "the abandoned caller's restart records its failure",
        move || {
            let state = state2.clone();
            async move {
                state
                    .diagnostics
                    .read()
                    .await
                    .events(100, None)
                    .iter()
                    .any(|e| {
                        matches!(
                            e.kind,
                            crate::diagnostics::DiagnosticEventKind::RestartFailed { .. }
                        )
                    })
            }
        },
    )
    .await;
    assert!(
        !managed
            .restart_in_flight
            .load(std::sync::atomic::Ordering::SeqCst),
        "the in-flight guard is released with the latch"
    );
    assert!(
        !managed.runner.read().await.stop_requested,
        "a refused start must not have stopped anything"
    );
}

/// Finding 1: the latch guard's `Drop` is the backstop for an exit that never
/// reaches `clear` (a panic, a cancelled task).
#[tokio::test]
async fn a_dropped_restart_latch_clears_the_flag() {
    use crate::process::manager::RestartLatch;
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let managed = register_dead_primary(&state).await;

    let in_flight = || {
        managed
            .restart_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
    };

    let latch = RestartLatch::acquire(&managed).await.expect("acquire");
    assert!(managed.runner.read().await.restart_requested);
    assert!(in_flight());
    drop(latch);
    assert!(!managed.runner.read().await.restart_requested);
    assert!(!in_flight());

    // Contended at drop: cleared as soon as the lock frees — and no new
    // restart can latch before that clear lands, or the late clear would
    // switch off the newer restart's flag.
    let latch = RestartLatch::acquire(&managed).await.expect("acquire");
    let reader = managed.runner.read().await;
    drop(latch);
    assert!(reader.restart_requested);
    assert!(
        RestartLatch::acquire(&managed).await.is_err(),
        "a restart must not latch while the previous one's clear is pending"
    );
    drop(reader);
    eventually("the contended latch clears", || async {
        !managed.runner.read().await.restart_requested && !in_flight()
    })
    .await;

    // The normal exit.
    let latch = RestartLatch::acquire(&managed).await.expect("acquire");
    latch.clear().await;
    assert!(!managed.runner.read().await.restart_requested);
    assert!(!in_flight());
}

/// A restart whose caller disconnected keeps running, so a retried restart
/// is refused while it runs instead of overlapping it, and is admitted once
/// it ends.
#[tokio::test]
async fn a_second_restart_is_refused_while_one_is_in_flight() {
    use crate::process::manager::RestartLatch;
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let managed = register_dead_primary(&state).await;

    let first = RestartLatch::acquire(&managed).await.expect("first");
    let refused = RestartLatch::acquire(&managed)
        .await
        .err()
        .expect("a second restart must be refused");
    assert!(
        refused.to_string().contains("already in progress"),
        "{refused}"
    );
    assert!(
        managed.runner.read().await.restart_requested,
        "the refusal must not clear the running restart's latch"
    );
    first.clear().await;
    RestartLatch::acquire(&managed)
        .await
        .expect("admitted once the first ends")
        .clear()
        .await;
}

/// A caller dropped while `acquire` waits for the runner's write lock must
/// not leave `restart_in_flight` set, or every later restart of the runner
/// (the serving watchdog's included) would be refused for good.
#[tokio::test]
async fn an_acquire_cancelled_at_its_lock_wait_releases_the_in_flight_bit() {
    use crate::process::manager::RestartLatch;
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let managed = register_dead_primary(&state).await;
    let in_flight = || {
        managed
            .restart_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
    };

    let reader = managed.runner.read().await;
    let parked = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        RestartLatch::acquire(&managed),
    )
    .await;
    assert!(parked.is_err(), "acquire must park on the held lock");
    drop(reader);
    eventually("the cancelled acquire's bit is released", || async {
        !in_flight()
    })
    .await;
    assert!(!managed.runner.read().await.restart_requested);
    RestartLatch::acquire(&managed)
        .await
        .expect("a later restart is admitted")
        .clear()
        .await;
}

/// The LKG pin a restart carries is applied only under its latch: a retry
/// refused as already in flight leaves the running restart's override alone.
#[test]
fn the_restart_door_hands_its_pin_to_the_restart() {
    let runner = include_str!("../../routes/runner.rs");
    let handler = fn_body(
        runner,
        "pub async fn restart_runner(",
        "pub async fn control_watchdog(",
    );
    assert!(
        !handler.contains("source_exe_override"),
        "the route must not write the shared override itself"
    );
    assert!(handler.contains("exe_pin,"));
    let manager = include_str!("../manager.rs");
    let in_flight = fn_body(
        manager,
        "async fn restart_in_flight(",
        "pub(crate) fn orphaned_by_failed_restart(",
    );
    let set = in_flight
        .find("latch.pin(pin)")
        .expect("pins under the latch");
    let gate = in_flight.find("restart_after_gate(").expect("restarts");
    let clear = in_flight.find("latch.clear().await").expect("clears");
    assert!(set < gate && gate < clear);
    assert!(
        !in_flight.contains("source_exe_override"),
        "the pin is the latch's: restart_in_flight must not write the override itself"
    );
}

/// The latch restores the override its pin replaced on EVERY exit — the
/// normal clear, and a drop (a panic or cancellation inside the restart) —
/// before it lets another restart in, so a pin can neither outlive its restart
/// nor clobber an override someone else had set.
#[tokio::test]
async fn a_restart_pin_is_restored_on_clear_and_on_drop() {
    use crate::process::manager::RestartLatch;
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let managed = register_dead_primary(&state).await;
    let earlier = std::path::PathBuf::from("earlier-override.exe");
    let pin = std::path::PathBuf::from("lkg.exe");
    *managed.source_exe_override.write().await = Some(earlier.clone());

    // Normal exit.
    let mut latch = RestartLatch::acquire(&managed).await.expect("acquire");
    latch.pin(pin.clone()).await;
    assert_eq!(*managed.source_exe_override.read().await, Some(pin.clone()));
    latch.clear().await;
    assert_eq!(
        *managed.source_exe_override.read().await,
        Some(earlier.clone())
    );

    // Dropped (a panic or cancellation inside the restart), uncontended.
    let mut latch = RestartLatch::acquire(&managed).await.expect("acquire");
    latch.pin(pin.clone()).await;
    drop(latch);
    assert_eq!(
        *managed.source_exe_override.read().await,
        Some(earlier.clone())
    );
    assert!(!managed.runner.read().await.restart_requested);

    // Dropped while the override is contended: restored before the bit frees.
    let mut latch = RestartLatch::acquire(&managed).await.expect("acquire");
    latch.pin(pin.clone()).await;
    let reader = managed.source_exe_override.read().await;
    drop(latch);
    assert!(
        RestartLatch::acquire(&managed).await.is_err(),
        "no restart may latch before the pin is restored"
    );
    drop(reader);
    eventually("the contended pin is restored", || async {
        !managed
            .restart_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    assert_eq!(*managed.source_exe_override.read().await, Some(earlier));
}

/// Finding 1, both doors: the latched half of `restart_runner_by_id` runs in
/// its own task, and so does the no-rebuild tail of `POST /runner/restart`
/// (LKG pin, restart, dev-action result).
#[test]
fn the_synchronous_restart_doors_run_their_restart_in_a_task() {
    let manager = include_str!("../manager.rs");
    let by_id = fn_body(
        manager,
        "pub async fn restart_runner_by_id(",
        "pub(crate) struct RestartLatch",
    );
    let latch = by_id.find("RestartLatch::acquire").expect("latches");
    let spawn = by_id.find("tokio::spawn(async move {").expect("spawns");
    // The body must run INSIDE the spawned closure, not after it.
    let closure_end = spawn
        + by_id[spawn..]
            .find(
                "
    })
    .await",
            )
            .expect("the spawned closure is awaited");
    let body = by_id.find("restart_in_flight(").expect("runs the body");
    assert!(latch < spawn && spawn < body && body < closure_end);
    let runner = include_str!("../../routes/runner.rs");
    let handler = fn_body(
        runner,
        "pub async fn restart_runner(",
        "pub async fn control_watchdog(",
    );
    // The rebuild branch runs detached through `submit_detached` and spells
    // the same calls, so read only the no-rebuild tail after it.
    let tail = &handler[handler
        .find("// ── No-rebuild path")
        .expect("no-rebuild tail")..];
    let spawn = tail
        .find("tokio::spawn(async move {")
        .expect("spawns the tail");
    for needle in [
        "exe_pin = Some(lkg_exe.clone())",
        "manager::restart_runner_pinned(",
        "record_run_result(run_result_for(e))",
        "record_run_result(ActionRunResult::Ran)",
    ] {
        let at = tail
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"));
        assert!(spawn < at, "{needle} must run inside the spawned task");
    }
    // Nit: the cold-build filing is decided after the explicit LKG pin.
    let pin = handler.find("let explicit_lkg_pin").expect("explicit pin");
    let cold = handler.find("let cold_build").expect("cold_build");
    assert!(pin < cold);
    assert!(handler.contains("&& !explicit_lkg_pin"));
}

/// Finding 2: both origin/main arms of `primary_build` register their
/// container with the pruner while they hold its lock.
#[test]
fn both_origin_main_arms_of_the_primary_build_register_their_container() {
    let src = include_str!("../primary_start.rs");
    let build = fn_body(
        src,
        "pub async fn primary_build(",
        "pub async fn primary_build_detached(",
    );
    let (dev, optimized) = build.split_at(build.find("let lock = state").expect("optimized arm"));
    for arm in [dev, optimized] {
        let lock = arm.find("spawn_container_lock").expect("container lock");
        // Bound to a NAMED variable: `let _ = …` would drop the guard at once.
        let active = arm
            .find("let _active = ActiveSpawnContainer::register(state, &container);")
            .expect("registered and held");
        let prepare = arm.find("prepare_worktree").expect("prepares");
        assert!(lock < active && active < prepare);
    }
}

/// Finding 2: the guard holds the container in the active set until it drops,
/// and never removes a registration it did not make.
#[tokio::test]
async fn an_active_spawn_container_is_registered_until_dropped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let container = tmp.path().join(".spawn-origin_main");
    {
        let _active = crate::state::ActiveSpawnContainer::register(&state, &container);
        assert!(state.active_spawn_containers().contains(&container));
    }
    assert!(!state.active_spawn_containers().contains(&container));

    state
        .active_spawn_worktrees
        .lock()
        .unwrap()
        .insert(container.clone());
    drop(crate::state::ActiveSpawnContainer::register(
        &state, &container,
    ));
    assert!(
        state.active_spawn_containers().contains(&container),
        "a registration another holder made must survive this guard"
    );
}

/// Finding 3: a pre-check that read a local origin/main its fetch did not
/// refresh says so in the label every profile cause quotes.
#[test]
fn a_profile_check_after_a_failed_fetch_says_its_ref_may_be_stale() {
    let repo = std::path::Path::new("/ws/qontinui-runner");
    let fresh = build_point_label("abc", repo, true);
    assert!(!fresh.contains("may be stale"), "{fresh}");
    let stale = build_point_label("abc", repo, false);
    assert!(stale.contains(STALE_ORIGIN_NOTE), "{stale}");
    for manifest in [None, Some(WITHOUT_PROFILE)] {
        match profile_support_from_manifest(manifest, &stale, RunnerBuildProfile::ReleaseChecked) {
            ProfileSupport::Missing(cause) => assert!(
                cause.contains(
                    "fetch did not complete; checked the local origin/main ref, which may be stale"
                ),
                "{cause}"
            ),
            ProfileSupport::Supported => panic!("not supported"),
        }
    }
}

/// Finding 4: a detached build that fails with nobody awaiting it still logs
/// its error. The awaiter is dropped while the build waits for the lock; the
/// build then fails at once (this fixture has no schemas sibling).
#[tokio::test]
async fn a_detached_build_logs_its_failure_when_the_awaiter_is_gone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    let build_lock = state
        .build_pool
        .primary_build_lock
        .clone()
        .lock_owned()
        .await;
    {
        let build = primary_build_detached(&state, BuildTree::OriginMain, BuildNeed::Always);
        let parked = tokio::time::timeout(std::time::Duration::from_millis(200), build).await;
        assert!(parked.is_err(), "the build must park on the held lock");
    }
    drop(build_lock);
    let state2 = state.clone();
    eventually("the orphaned build's failure is logged", move || {
        let state = state2.clone();
        async move { logged(&state, "Primary build failed").await }
    })
    .await;
}

fn slot_exe(id: usize, mtime_secs: u64) -> ResolvedRunnerExe {
    ResolvedRunnerExe {
        mtime: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs)),
        path: std::path::PathBuf::from(format!("slot-{id}")),
        origin: ExeOrigin::Slot(id),
        provenance: None,
        unverified_warning: None,
        profile: RunnerBuildProfile::Dev,
        profile_fallback: None,
    }
}

/// Nit: the dev fallback takes the pool's last successful slot, else the
/// NEWEST slot exe — never the first slot by iteration order.
#[test]
fn the_dev_fallback_slot_is_the_last_successful_else_the_newest() {
    let pick = |last| {
        pick_dev_slot(
            vec![slot_exe(0, 100), slot_exe(1, 300), slot_exe(2, 200)],
            last,
        )
        .map(|r| r.origin)
    };
    assert_eq!(pick(None), Some(ExeOrigin::Slot(1)));
    assert_eq!(pick(Some(2)), Some(ExeOrigin::Slot(2)));
    // A last-successful slot that is not a candidate (busy, gone) falls back.
    assert_eq!(pick(Some(7)), Some(ExeOrigin::Slot(1)));
    assert!(pick_dev_slot(Vec::new(), None).is_none());
}

/// Nit: a slot that is building right now is never the dev fallback, even
/// when its exe is the newest.
#[tokio::test]
async fn a_busy_dev_slot_is_never_the_fallback() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = state_under(tmp.path());
    for id in [0, 1] {
        write_file(&state.config.runner_exe_path_for_slot(id), b"dev-slot");
    }
    // Slot 1 is an hour older; slot 0 is the newest, and is building.
    std::fs::OpenOptions::new()
        .write(true)
        .open(state.config.runner_exe_path_for_slot(1))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3_600))
        .unwrap();
    *state.build_pool.slots[0].busy.write().await = Some(crate::state::BuildInfo {
        started_at: chrono::Utc::now(),
        requester_id: None,
        rebuild_kind: "exe".to_string(),
        mem_gate: None,
    });
    let p = prepare_optimized_primary(
        &state,
        RunnerBuildProfile::ReleaseChecked,
        StartIntent::Unattended,
        no_build(),
    )
    .await
    .unwrap();
    assert_eq!(p.resolved.origin, ExeOrigin::Slot(1));
}

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

async fn logged(state: &SharedState, needle: &str) -> bool {
    state
        .logs
        .history()
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
/// unattended start builds in the foreground, because there is no running
/// primary to protect.
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

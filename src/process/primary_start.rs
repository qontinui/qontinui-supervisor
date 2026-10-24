//! Starting and building the PRIMARY runner in an optimized build profile.
//!
//! Plan `2026-10-19-primary-runner-is-a-debug-build-and-nothing-shows-it`,
//! Phase 5 (and its review round 1). The primary is built in `release-checked`
//! into `target-pool/primary/` ([`crate::build_profile`]); this module decides
//! what it STARTS from:
//!
//! 1. Its own supervisor build (`target-pool/primary/release-checked/`), or a
//!    newer vouched local `target/release-checked/` build — startable.
//! 2. Otherwise it depends on who asked ([`StartIntent`]):
//!    - **Unattended** (boot `--auto-start`, crash watchdog, serving watchdog):
//!      never wait for a cold optimized build with no primary on the box. Start
//!      the `release-checked` LKG if there is one; else the newest DEV artifact
//!      (the dev LKG, else a dev pool-slot exe) with an ERROR line and
//!      configured ≠ resolved on `/health`; and kick a BACKGROUND
//!      `release-checked` build so the next start finds one.
//!    - **Operator** (any HTTP start/restart): build the artifact now, in the
//!      foreground; on failure take the `release-checked` LKG, else refuse and
//!      name why. An operator start never falls back to a `debug/` artifact —
//!      a debug primary is the `dev` opt-in.
//!
//! **The build tree must DECLARE the profile.** On the path that is about to
//! build — and only there — [`primary_build`] reads the workspace `Cargo.toml`
//! of the exact tree it will compile (origin/main resolved once to a sha, the
//! manifest read and the worktree prepared at that sha) before any frontend
//! build; a tree with no `[profile.release-checked]` (one that predates the
//! runner half of the plan, Phase 3) makes the optimized build unavailable with
//! a named cause instead of a cargo error forty seconds into a frontend build.
//! An unattended start with nothing at all to start (a cold box) builds in the
//! foreground, because there is nothing else it could start. That is not "no
//! primary is running": the serving watchdog's restart is unattended and has
//! a wedged primary on the box; a restart prepares before it stops, so that
//! primary is stopped only once the build has produced something to start.

use std::future::Future;
use std::sync::atomic::Ordering;

use tracing::{error, info, warn};

use crate::build_profile::RunnerBuildProfile;
use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::{
    file_mtime, resolve_primary_dir_exe, BuildProvenance, BuildTree, ExeOrigin, ResolvedRunnerExe,
};
use crate::state::{ActiveSpawnContainer, LkgInfo, SharedState};

/// Who asked for a runner start. Decides only what an optimized primary
/// starts from when its own profile has nothing startable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartIntent {
    /// An operator or caller request: an HTTP start, restart or spawn.
    Operator,
    /// The supervisor on its own, with nobody present: the `--auto-start`
    /// boot start, the crash-only watchdog, the serving watchdog.
    Unattended,
}

impl StartIntent {
    /// The intent of a restart from `source`. Only the in-process serving
    /// watchdog is unattended: `Watchdog` is a WIRE value any HTTP caller can
    /// claim (and is refused for non-temp runners anyway), so it must not buy
    /// a caller the unattended debug fallback.
    pub fn of_restart(source: &RestartSource) -> Self {
        match source {
            RestartSource::ServingWatchdog => StartIntent::Unattended,
            _ => StartIntent::Operator,
        }
    }
}

/// Can the runner tree a primary build would compile build the profile?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSupport {
    Supported,
    /// It cannot, and why (named in every refusal and fallback line).
    Missing(String),
}

#[cfg(test)]
impl ProfileSupport {
    pub fn is_supported(&self) -> bool {
        matches!(self, ProfileSupport::Supported)
    }
}

/// The ONE point an `origin/main` primary build compiles, and its manifest.
///
/// A `git fetch` with a deadline ([`crate::git_provenance::fetch_origin`]) so a
/// ref pushed since the last fetch is seen — no ticker is relied on: the
/// origin-drift refresher returns before fetching on a box with no dev LKG
/// sha. Then `origin/main` is resolved to a sha ONCE, and the workspace
/// `Cargo.toml` is read AT that sha. The caller prepares the worktree at the
/// same sha (without fetching again), so the tree the profile check read IS
/// the tree that is built.
///
/// A fetch that did not complete (offline, a remote error, the deadline)
/// degrades to the refs already present — and says so in the label, which
/// every profile cause quotes, so a `Missing` read off a possibly stale local
/// `origin/main` is never presented as firm.
async fn origin_main_build_point(
    state: &SharedState,
) -> Result<(String, Option<String>, String), SupervisorError> {
    let repo = crate::spawn_worktree::find_repo_root(&state.config.project_dir)?;
    let fetched = match crate::git_provenance::fetch_origin(&repo).await {
        Ok(()) => true,
        Err(e) => {
            warn!("Primary build: {e}");
            false
        }
    };
    let sha = git_stdout(&repo, &["rev-parse", "--verify", "origin/main^{commit}"])
        .await
        .ok_or_else(|| {
            SupervisorError::BuildFailed(format!(
                "the primary build could not resolve origin/main in {}",
                repo.display()
            ))
        })?;
    let manifest = git_stdout(&repo, &["show", &format!("{sha}:Cargo.toml")]).await;
    let label = build_point_label(&sha, &repo, fetched);
    Ok((sha, manifest, label))
}

/// What the profile check names as the tree it read.
fn build_point_label(sha: &str, repo: &std::path::Path, fetched: bool) -> String {
    let mut label = format!("{sha}:Cargo.toml (origin/main) in {}", repo.display());
    if !fetched {
        label.push_str(STALE_ORIGIN_NOTE);
    }
    label
}

/// Appended to the build-point label when the pre-check fetch did not land.
const STALE_ORIGIN_NOTE: &str =
    "; fetch did not complete; checked the local origin/main ref, which may be stale";

/// `git <args>` in `cwd`, bounded to 20 s; trimmed stdout on success, `None`
/// on any failure.
async fn git_stdout(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// [`ProfileSupport`] of `profile` for the tree `tree`. `dev` is always
/// supported (cargo's built-in profile). An unreadable manifest is NOT
/// support — unknown never reads as "fine" — so it is `Missing` naming that.
///
/// `LiveWorkingTree` reads `<runner>/Cargo.toml`; `OriginMain` reads it at the
/// sha [`origin_main_build_point`] resolves. Called only on a path that is
/// about to build (or prewarm) — never on a start that already has a
/// startable artifact.
pub async fn build_tree_profile_support(
    state: &SharedState,
    profile: RunnerBuildProfile,
    tree: BuildTree,
) -> ProfileSupport {
    if profile == RunnerBuildProfile::Dev {
        return ProfileSupport::Supported;
    }
    match tree {
        BuildTree::LiveWorkingTree => {
            let path = state.config.runner_npm_dir().join("Cargo.toml");
            let manifest = std::fs::read_to_string(&path).ok();
            profile_support_from_manifest(manifest.as_deref(), &path.display().to_string(), profile)
        }
        BuildTree::OriginMain => match origin_main_build_point(state).await {
            Ok((_, manifest, label)) => {
                profile_support_from_manifest(manifest.as_deref(), &label, profile)
            }
            Err(e) => ProfileSupport::Missing(e.to_string()),
        },
    }
}

/// Pure core of [`build_tree_profile_support`].
pub fn profile_support_from_manifest(
    manifest: Option<&str>,
    label: &str,
    profile: RunnerBuildProfile,
) -> ProfileSupport {
    match manifest {
        None => ProfileSupport::Missing(format!(
            "the runner workspace manifest ({label}) could not be read, so it is not known to \
             declare [profile.{}]",
            profile.cargo_name()
        )),
        Some(text) if crate::build_profile::manifest_declares_profile(text, profile) => {
            ProfileSupport::Supported
        }
        Some(_) => ProfileSupport::Missing(format!(
            "the runner tree the primary builds ({label}) declares no [profile.{}] — it predates \
             the runner half of plan 2026-10-19-primary-runner-is-a-debug-build-and-nothing-shows-it \
             (Phase 3)",
            profile.cargo_name()
        )),
    }
}

/// Whether [`primary_build`] must compile even when the artifact exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildNeed {
    /// An explicit rebuild (`restart {rebuild: true}`, fix-and-rebuild).
    Always,
    /// A start that found nothing startable: re-checked after the build lock
    /// is taken, so a start that queued behind another start's build does not
    /// build a second time.
    IfAbsent,
}

fn unavailable(
    profile: RunnerBuildProfile,
    support: ProfileSupport,
) -> Result<(), SupervisorError> {
    match support {
        ProfileSupport::Supported => Ok(()),
        ProfileSupport::Missing(cause) => Err(SupervisorError::BuildFailed(format!(
            "the {profile} primary build is unavailable: {cause}"
        ))),
    }
}

/// Build the PRIMARY in its configured profile from `tree`.
///
/// `dev`: into the build pool, as before (the origin/main worktree under its
/// container lock). Optimized, in this order — `primary_build_lock`, then (for
/// origin/main) the spawn container lock, then the pool permit inside
/// [`crate::build_monitor::run_primary_build`]:
/// 1. the `IfAbsent` re-check under the build lock;
/// 2. the profile pre-check on the exact tree that will be built (origin/main
///    resolved ONCE to a sha after a fetch with a deadline, its manifest read
///    at that sha, the worktree prepared at that sha with no further fetch of
///    the runner repo) — before any frontend build;
/// 3. the build into `target-pool/primary/`.
pub async fn primary_build(
    state: &SharedState,
    tree: BuildTree,
    need: BuildNeed,
) -> Result<(), SupervisorError> {
    let profile = crate::build_profile::primary_profile(&state.config);
    let project_dir = state.config.project_dir.clone();
    if profile == RunnerBuildProfile::Dev {
        return match tree {
            BuildTree::OriginMain => {
                let container =
                    crate::spawn_worktree::container_path_for_ref(&project_dir, "origin/main")?;
                let _container = state.spawn_container_lock(&container).lock_owned().await;
                let _active = ActiveSpawnContainer::register(state, &container);
                let prepared =
                    crate::spawn_worktree::prepare_worktree(&project_dir, "origin/main").await?;
                log_origin_main_build(state, &prepared, profile).await;
                crate::build_monitor::run_cargo_build_with_dir(
                    state,
                    Some("primary-rebuild:origin/main".to_string()),
                    Some(prepared.src_tauri.clone()),
                    false,
                    crate::build_monitor::BuildSourceKind::OriginMain {
                        resolved_sha: prepared.resolved_sha.clone(),
                    },
                )
                .await
            }
            BuildTree::LiveWorkingTree => crate::build_monitor::run_cargo_build(state).await,
        };
    }

    let lock = state
        .build_pool
        .primary_build_lock
        .clone()
        .lock_owned()
        .await;
    if need == BuildNeed::IfAbsent && state.config.primary_exe_path(profile).exists() {
        info!("Primary build skipped: a {profile} artifact appeared while waiting for the lock");
        return Ok(());
    }
    match tree {
        BuildTree::OriginMain => {
            let container =
                crate::spawn_worktree::container_path_for_ref(&project_dir, "origin/main")?;
            let _container = state.spawn_container_lock(&container).lock_owned().await;
            // Registered for the whole arm (dropped before the lock above), so
            // no pruner sweep removes the tree this build is reading.
            let _active = ActiveSpawnContainer::register(state, &container);
            let (sha, manifest, label) = origin_main_build_point(state).await?;
            unavailable(
                profile,
                profile_support_from_manifest(manifest.as_deref(), &label, profile),
            )?;
            let prepared =
                crate::spawn_worktree::prepare_worktree_at(&project_dir, "origin/main", &sha)
                    .await?;
            if prepared.resolved_sha != sha {
                return Err(SupervisorError::BuildFailed(format!(
                    "the primary worktree resolved {} but the profile check read {sha}; refusing \
                     to build a tree other than the one checked",
                    prepared.resolved_sha
                )));
            }
            log_origin_main_build(state, &prepared, profile).await;
            crate::build_monitor::run_primary_build(
                state,
                &lock,
                Some("primary-rebuild:origin/main".to_string()),
                Some(prepared.src_tauri.clone()),
                crate::build_monitor::BuildSourceKind::OriginMain { resolved_sha: sha },
            )
            .await
        }
        BuildTree::LiveWorkingTree => {
            unavailable(
                profile,
                build_tree_profile_support(state, profile, BuildTree::LiveWorkingTree).await,
            )?;
            crate::build_monitor::run_primary_build(
                state,
                &lock,
                Some("primary-rebuild:live_tree".to_string()),
                None,
                crate::build_monitor::BuildSourceKind::LiveTree,
            )
            .await
        }
    }
}

/// [`primary_build`] in its OWN task, awaited. A synchronous HTTP door (a
/// no-rebuild restart, `POST /runners/{id}/start`) can reach a cold build;
/// if the client disconnects, axum drops the handler future, and a build
/// running inside it would be cancelled mid-flight (KillOnDrop kills cargo).
/// A spawned task is not cancelled with its awaiter, so the build always
/// finishes and the next start finds the artifact.
///
/// A failure is logged INSIDE the task: once the awaiter is gone, nobody
/// reads the returned error, and the early refusals (profile unavailable, an
/// unresolvable origin/main, a failed worktree prep, the sha guard) happen
/// before the build monitor logs anything of its own.
pub async fn primary_build_detached(
    state: &SharedState,
    tree: BuildTree,
    need: BuildNeed,
) -> Result<(), SupervisorError> {
    let state = state.clone();
    tokio::spawn(async move {
        let outcome = primary_build(&state, tree, need).await;
        if let Err(e) = &outcome {
            let msg = format!("Primary build failed: {e}");
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Build, LogLevel::Error, msg)
                .await;
        }
        outcome
    })
    .await
    .map_err(|e| SupervisorError::BuildFailed(format!("the primary build task failed: {e}")))?
}

async fn log_origin_main_build(
    state: &SharedState,
    prepared: &crate::spawn_worktree::PreparedWorktree,
    profile: RunnerBuildProfile,
) {
    let msg = format!(
        "Primary rebuild: building from origin/main worktree (resolved_sha={}, src_tauri={:?}, \
         profile {profile}). This is the default Phase B path — the primary runs \
         latest-green-main, not the working checkout. Pass from_working_tree:true to compile the \
         live tree instead.",
        prepared.resolved_sha, prepared.src_tauri
    );
    info!("{}", msg);
    state.logs.emit(LogSource::Build, LogLevel::Info, msg).await;
}

/// Clears `primary_background_build_in_flight` on every exit, a panic
/// included, so one failed background build cannot block every later one.
struct BackgroundBuildLatch(SharedState);

impl Drop for BackgroundBuildLatch {
    fn drop(&mut self) {
        self.0
            .build_pool
            .primary_background_build_in_flight
            .store(false, Ordering::SeqCst);
    }
}

/// Kick ONE background optimized primary build from `origin/main`, so the
/// next primary start finds an optimized artifact. A no-op while one is
/// already in flight. A tree that cannot build the profile fails fast with
/// the cause named.
pub fn spawn_background_primary_build(state: &SharedState) {
    if state
        .build_pool
        .primary_background_build_in_flight
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let latch = BackgroundBuildLatch(state.clone());
    let state = state.clone();
    tokio::spawn(async move {
        let _latch = latch;
        let msg = "Background primary build: building the optimized primary artifact so the \
                   next primary start does not fall back"
            .to_string();
        info!("{}", msg);
        state.logs.emit(LogSource::Build, LogLevel::Info, msg).await;
        let outcome = primary_build(&state, BuildTree::OriginMain, BuildNeed::IfAbsent).await;
        let (level, msg) = match &outcome {
            Ok(()) => (
                LogLevel::Info,
                "Background primary build succeeded: the next primary start uses it".to_string(),
            ),
            Err(e) => (
                LogLevel::Error,
                format!("Background primary build failed: {e}"),
            ),
        };
        match level {
            LogLevel::Error => error!("{}", msg),
            _ => info!("{}", msg),
        }
        state.logs.emit(LogSource::Build, level, msg).await;
    });
}

/// What [`prepare_optimized_primary`] decided.
#[derive(Debug, Clone)]
pub struct PreparedPrimary {
    pub resolved: ResolvedRunnerExe,
    /// The caller should start a background optimized build (an unattended
    /// start that took a fallback).
    pub kick_background_build: bool,
}

/// Would an optimized primary's next start BUILD before it could start?
/// True when the primary is configured optimized and no supervisor build or
/// adoptable local build of that profile exists. Cheap (stats only); lets a
/// synchronous door file its dev-action record as the build it will run.
pub async fn primary_start_needs_build(state: &SharedState) -> bool {
    let profile = crate::build_profile::primary_profile(&state.config);
    if profile == RunnerBuildProfile::Dev {
        return false;
    }
    !resolve_primary_dir_exe(state, profile)
        .await
        .is_some_and(|r| startable(&r))
}

fn startable(r: &ResolvedRunnerExe) -> bool {
    matches!(
        r.origin,
        ExeOrigin::PrimaryBuildDir(_) | ExeOrigin::AdoptedLocalBuild(_)
    )
}

/// The entry the start and restart paths use: decide what an optimized
/// primary starts from. Any build runs DETACHED ([`primary_build_detached`]),
/// and an unattended fallback kicks a background build.
pub async fn prepare_primary_start(
    state: &SharedState,
    profile: RunnerBuildProfile,
    intent: StartIntent,
) -> Result<ResolvedRunnerExe, SupervisorError> {
    let prepared = prepare_optimized_primary(state, profile, intent, || {
        primary_build_detached(state, BuildTree::OriginMain, BuildNeed::IfAbsent)
    })
    .await?;
    if prepared.kick_background_build {
        spawn_background_primary_build(state);
    }
    Ok(prepared.resolved)
}

/// The decision, with the build step injected so every arm is testable
/// without cargo. The build step carries the profile pre-check itself
/// ([`primary_build`]), so a tree without the profile is a build failure
/// naming the missing `[profile.release-checked]`.
///
/// 1. A startable artifact → it.
/// 2. Operator: build now; on failure the profile's LKG, else an unverified
///    local build (the identity gate decides), else refuse. Never `debug/`.
/// 3. Unattended: the profile's LKG, else the newest dev artifact (ERROR,
///    configured ≠ resolved) — both kick a background build; with NOTHING to
///    start at all (a cold box: nothing else could be started — a wedged
///    primary under a serving-watchdog restart is stopped only after this
///    build) build in the foreground; on failure an unverified local build,
///    else refuse.
pub async fn prepare_optimized_primary<B, Fut>(
    state: &SharedState,
    profile: RunnerBuildProfile,
    intent: StartIntent,
    build: B,
) -> Result<PreparedPrimary, SupervisorError>
where
    B: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), SupervisorError>>,
{
    let found = resolve_primary_dir_exe(state, profile).await;
    if let Some(r) = found.as_ref().filter(|r| startable(r)) {
        return Ok(PreparedPrimary {
            resolved: r.clone(),
            kick_background_build: false,
        });
    }
    let profile_lkg = crate::state::lkg_on_disk(&state.config, profile);
    let mut why = format!(
        "no startable {profile} artifact (looked in {:?} and the {profile} local build dir)",
        state.config.primary_exe_path(profile)
    );

    if intent == StartIntent::Unattended {
        if let Some((info, exe)) = profile_lkg.clone() {
            log_profile_lkg_start(state, profile, &why, &info, &exe).await;
            return Ok(PreparedPrimary {
                resolved: lkg_resolution(info, exe, profile, None),
                kick_background_build: true,
            });
        }
        if let Some(mut dev) = newest_dev_artifact(state).await {
            let reason = format!(
                "configured {profile}, but {why}; an UNATTENDED start fell back to the newest dev \
                 artifact {:?} (origin {}) rather than leave the box with no primary, and started \
                 a background {profile} build for the next start.",
                dev.path,
                dev.origin.label(),
            );
            error!("DEBUG PRIMARY FALLBACK: {}", reason);
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Error,
                    format!("DEBUG PRIMARY FALLBACK: {reason}"),
                )
                .await;
            dev.profile_fallback = Some(reason);
            return Ok(PreparedPrimary {
                resolved: dev,
                kick_background_build: true,
            });
        }
    }

    // Build now: an operator start, or an unattended start with nothing at all
    // to start (a cold box — nothing else could be started; a restart stops
    // the old process, wedged or not, only after this returns).
    let msg = format!("Primary start: {why} — building one from origin/main before starting");
    warn!("{}", msg);
    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Warn, msg)
        .await;
    match build().await {
        Ok(()) => match resolve_primary_dir_exe(state, profile).await {
            Some(r) if startable(&r) => {
                return Ok(PreparedPrimary {
                    resolved: r,
                    kick_background_build: false,
                })
            }
            _ => {
                why = format!(
                    "the {profile} build reported success but left no exe at {:?}",
                    state.config.primary_exe_path(profile)
                )
            }
        },
        Err(e) => why = format!("the {profile} build failed: {e}"),
    }

    // The same profile's LKG: still optimized, so allowed for any start.
    if let Some((info, exe)) = profile_lkg {
        log_profile_lkg_start(state, profile, &why, &info, &exe).await;
        return Ok(PreparedPrimary {
            resolved: lkg_resolution(info, exe, profile, None),
            kick_background_build: false,
        });
    }

    // An unverified local build of the profile: the identity gate decides.
    if let Some(r) = found {
        return Ok(PreparedPrimary {
            resolved: r,
            kick_background_build: false,
        });
    }

    Err(SupervisorError::Process(format!(
        "Refusing to start the primary: it is configured for the {profile} build profile \
         ({source}) and {why}. {tail} Fix that and retry (POST /runner/restart \
         {{\"rebuild\": true}}), or opt into a debug primary with --primary-build-profile dev / \
         QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE=dev.",
        source = state.config.primary_build_profile.source.knob(),
        tail = match intent {
            StartIntent::Operator => {
                "An operator start never falls back to a debug/ artifact unless the dev profile \
                 was opted into."
            }
            StartIntent::Unattended => "There is no dev artifact to fall back to either.",
        },
    )))
}

async fn log_profile_lkg_start(
    state: &SharedState,
    profile: RunnerBuildProfile,
    why: &str,
    info: &LkgInfo,
    exe: &std::path::Path,
) {
    let msg = format!(
        "Primary start: {why}; starting the {profile} last-known-good {exe:?} (built_at {}, sha {})",
        info.built_at.to_rfc3339(),
        info.sha.as_deref().unwrap_or("(unknown)")
    );
    warn!("{}", msg);
    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Warn, msg)
        .await;
}

/// The newest DEV artifact an unattended fallback may start: the dev LKG,
/// else a dev pool-slot exe (a supervisor build) picked by [`pick_dev_slot`] —
/// never a slot mid-build, and never one whose provenance says it was built
/// from a foreign override tree. Its record carries `profile: Dev`, which
/// `/health` shows as resolved.
async fn newest_dev_artifact(state: &SharedState) -> Option<ResolvedRunnerExe> {
    if let Some((info, exe)) = crate::state::lkg_on_disk(&state.config, RunnerBuildProfile::Dev) {
        return Some(lkg_resolution(info, exe, RunnerBuildProfile::Dev, None));
    }
    let last_successful = *state.build_pool.last_successful_slot.read().await;
    let mut candidates = Vec::new();
    for slot in state.build_pool.slots.iter() {
        // A lock held for write is a build starting or ending: busy too.
        let busy = slot.busy.try_read().map_or(true, |b| b.is_some());
        let path = slot.exe_path();
        if busy || !path.exists() {
            continue;
        }
        let provenance = crate::process::manager::read_slot_provenance(&slot.target_dir);
        if provenance
            .as_ref()
            .is_some_and(|p| p.source == crate::process::manager::BuildSource::Override)
        {
            continue;
        }
        candidates.push(ResolvedRunnerExe {
            mtime: file_mtime(&path),
            path,
            origin: ExeOrigin::Slot(slot.id),
            provenance,
            unverified_warning: None,
            profile: RunnerBuildProfile::Dev,
            profile_fallback: None,
        });
    }
    pick_dev_slot(candidates, last_successful)
}

/// Among idle, non-override dev slot exes: the `last_successful_slot` one
/// (the pool's own record of its newest good build), else the newest by exe
/// mtime. Never iteration order — slot 0 is not the newest by being first.
fn pick_dev_slot(
    candidates: Vec<ResolvedRunnerExe>,
    last_successful: Option<usize>,
) -> Option<ResolvedRunnerExe> {
    let is_last = |r: &ResolvedRunnerExe| matches!(r.origin, ExeOrigin::Slot(id) if Some(id) == last_successful);
    if let Some(i) = candidates.iter().position(is_last) {
        return candidates.into_iter().nth(i);
    }
    candidates.into_iter().max_by_key(|r| r.mtime)
}

/// A [`ResolvedRunnerExe`] for an LKG start.
fn lkg_resolution(
    info: LkgInfo,
    exe: std::path::PathBuf,
    profile: RunnerBuildProfile,
    profile_fallback: Option<String>,
) -> ResolvedRunnerExe {
    ResolvedRunnerExe {
        mtime: file_mtime(&exe),
        provenance: Some(BuildProvenance {
            sha: info.sha.clone(),
            source: info.source,
            built_from: exe.to_string_lossy().to_string(),
            built_at: info.built_at.to_rfc3339(),
        }),
        path: exe,
        origin: ExeOrigin::LkgFallback(profile),
        unverified_warning: None,
        profile,
        profile_fallback,
    }
}

#[cfg(test)]
mod tests;

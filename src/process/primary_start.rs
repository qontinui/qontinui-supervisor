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
//! **The build tree must DECLARE the profile.** Before any stop, worktree prep
//! or frontend build, [`build_tree_profile_support`] reads the runner tree's
//! workspace `Cargo.toml`; a tree with no `[profile.release-checked]` (one
//! that predates the runner half of the plan, Phase 3) makes the optimized
//! build unavailable with a named cause instead of a cargo error forty seconds
//! into a frontend build.

use std::future::Future;
use std::sync::atomic::Ordering;

use tracing::{error, info, warn};

use crate::build_profile::RunnerBuildProfile;
use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::{
    file_mtime, resolve_primary_dir_exe, resolve_source_exe_detailed, BuildProvenance, BuildTree,
    ExeOrigin, ResolvedRunnerExe,
};
use crate::state::{LkgInfo, SharedState};

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

impl ProfileSupport {
    pub fn is_supported(&self) -> bool {
        matches!(self, ProfileSupport::Supported)
    }
}

/// Read the workspace `Cargo.toml` of the tree `tree` names, with a label
/// saying where it was read from. `None` when it cannot be read.
///
/// `LiveWorkingTree` reads `<runner>/Cargo.toml`. `OriginMain` reads
/// `origin/main:Cargo.toml` from the runner repo's LOCAL remote-tracking ref
/// (no fetch here: the origin-drift ticker fetches every two minutes), so it
/// costs one `git show` and runs before any worktree is prepared.
async fn read_build_tree_manifest(
    state: &SharedState,
    tree: BuildTree,
) -> (Option<String>, String) {
    let npm = state.config.runner_npm_dir();
    match tree {
        BuildTree::LiveWorkingTree => {
            let path = npm.join("Cargo.toml");
            (
                std::fs::read_to_string(&path).ok(),
                format!("{}", path.display()),
            )
        }
        BuildTree::OriginMain => {
            let label = format!("origin/main:Cargo.toml in {}", npm.display());
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&npm)
                    .args(["show", "origin/main:Cargo.toml"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            let text = match out {
                Ok(Ok(o)) if o.status.success() => String::from_utf8(o.stdout).ok(),
                _ => None,
            };
            (text, label)
        }
    }
}

/// [`ProfileSupport`] of `profile` for the tree `tree`. `dev` is always
/// supported (cargo's built-in profile). An unreadable manifest is NOT
/// support — unknown never reads as "fine" — so it is `Missing` naming that.
pub async fn build_tree_profile_support(
    state: &SharedState,
    profile: RunnerBuildProfile,
    tree: BuildTree,
) -> ProfileSupport {
    if profile == RunnerBuildProfile::Dev {
        return ProfileSupport::Supported;
    }
    let (manifest, label) = read_build_tree_manifest(state, tree).await;
    profile_support_from_manifest(manifest.as_deref(), &label, profile)
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
    /// An explicit rebuild (`restart {rebuild: true}`).
    Always,
    /// A start that found nothing startable: re-checked after the build lock
    /// is taken, so a start that queued behind another start's build does not
    /// build a second time.
    IfAbsent,
}

/// Build the PRIMARY in its configured profile from `tree`.
///
/// `dev`: into the build pool, exactly as before. Optimized: under
/// `primary_build_lock` — taken FIRST, so the worktree prep happens under it
/// too — after the profile pre-check, into `target-pool/primary/`
/// ([`crate::build_monitor::run_primary_build`]).
pub async fn primary_build(
    state: &SharedState,
    tree: BuildTree,
    need: BuildNeed,
) -> Result<(), SupervisorError> {
    let profile = crate::build_profile::primary_profile(&state.config);
    if profile == RunnerBuildProfile::Dev {
        return match tree {
            BuildTree::OriginMain => {
                let prepared = crate::spawn_worktree::prepare_worktree(
                    &state.config.project_dir,
                    "origin/main",
                )
                .await?;
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

    if let ProfileSupport::Missing(cause) = build_tree_profile_support(state, profile, tree).await {
        return Err(SupervisorError::BuildFailed(format!(
            "the {profile} primary build is unavailable: {cause}"
        )));
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
            let prepared =
                crate::spawn_worktree::prepare_worktree(&state.config.project_dir, "origin/main")
                    .await?;
            log_origin_main_build(state, &prepared, profile).await;
            crate::build_monitor::run_primary_build(
                state,
                &lock,
                Some("primary-rebuild:origin/main".to_string()),
                Some(prepared.src_tauri.clone()),
                crate::build_monitor::BuildSourceKind::OriginMain {
                    resolved_sha: prepared.resolved_sha.clone(),
                },
            )
            .await
        }
        BuildTree::LiveWorkingTree => {
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

/// Kick ONE background `release-checked` primary build from `origin/main`, so
/// the next primary start finds an optimized artifact. A no-op while one is
/// already in flight.
pub fn spawn_background_primary_build(state: &SharedState) {
    if state
        .build_pool
        .primary_background_build_in_flight
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
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
        state
            .build_pool
            .primary_background_build_in_flight
            .store(false, Ordering::SeqCst);
    });
}

/// What [`prepare_optimized_primary`] decided.
#[derive(Debug, Clone)]
pub struct PreparedPrimary {
    pub resolved: ResolvedRunnerExe,
    /// The caller should start a background optimized build (an unattended
    /// start that fell back, on a tree that supports the profile).
    pub kick_background_build: bool,
}

/// The entry the start and restart paths use: decide what an optimized
/// primary starts from, building in the foreground for an operator, and
/// kicking a background build after an unattended fallback.
pub async fn prepare_primary_start(
    state: &SharedState,
    profile: RunnerBuildProfile,
    intent: StartIntent,
) -> Result<ResolvedRunnerExe, SupervisorError> {
    let support = build_tree_profile_support(state, profile, BuildTree::OriginMain).await;
    let prepared = prepare_optimized_primary(state, profile, intent, &support, || {
        primary_build(state, BuildTree::OriginMain, BuildNeed::IfAbsent)
    })
    .await?;
    if prepared.kick_background_build {
        spawn_background_primary_build(state);
    }
    Ok(prepared.resolved)
}

/// The decision, with the build step and the profile support injected so
/// every arm is testable without cargo. See the module docs for the rules.
pub async fn prepare_optimized_primary<B, Fut>(
    state: &SharedState,
    profile: RunnerBuildProfile,
    intent: StartIntent,
    support: &ProfileSupport,
    build: B,
) -> Result<PreparedPrimary, SupervisorError>
where
    B: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), SupervisorError>>,
{
    let startable = |r: &ResolvedRunnerExe| {
        matches!(
            r.origin,
            ExeOrigin::PrimaryBuildDir(_) | ExeOrigin::AdoptedLocalBuild(_)
        )
    };
    let found = resolve_primary_dir_exe(state, profile).await;
    if let Some(r) = found.as_ref().filter(|r| startable(r)) {
        return Ok(PreparedPrimary {
            resolved: r.clone(),
            kick_background_build: false,
        });
    }
    let profile_lkg = crate::state::lkg_on_disk(&state.config, profile);

    // Why the optimized artifact is not startable, for every line below.
    let mut why = match support {
        ProfileSupport::Supported => format!(
            "no startable {profile} artifact (looked in {:?} and the {profile} local build dir)",
            state.config.primary_exe_path(profile)
        ),
        ProfileSupport::Missing(cause) => cause.clone(),
    };

    if intent == StartIntent::Operator && support.is_supported() {
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
    }

    // The same profile's LKG: still optimized, so allowed for any start.
    if let Some((info, exe)) = profile_lkg {
        let msg = format!(
            "Primary start: {why}; starting the {profile} last-known-good {exe:?} (built_at {}, \
             sha {})",
            info.built_at.to_rfc3339(),
            info.sha.as_deref().unwrap_or("(unknown)")
        );
        warn!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, msg)
            .await;
        return Ok(PreparedPrimary {
            resolved: lkg_resolution(info, exe, profile, None),
            kick_background_build: intent == StartIntent::Unattended && support.is_supported(),
        });
    }

    if intent == StartIntent::Unattended {
        if let Some(mut dev) = newest_dev_artifact(state).await {
            let reason = format!(
                "configured {profile}, but {why}; an UNATTENDED start fell back to the newest dev \
                 artifact {:?} (origin {}) rather than leave the box with no primary{}.",
                dev.path,
                dev.origin.label(),
                if support.is_supported() {
                    "; a background release-checked build was started for the next start"
                } else {
                    ""
                }
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
                kick_background_build: support.is_supported(),
            });
        }
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
         ({source}) and {why}. {tail} Fix that and retry, or opt into a debug primary with \
         --primary-build-profile dev / QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE=dev.",
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

/// The newest DEV artifact an unattended fallback may start: the dev LKG,
/// else a dev pool-slot exe (a supervisor build). Its record carries
/// `profile: Dev`, which is what `/health` shows as resolved.
async fn newest_dev_artifact(state: &SharedState) -> Option<ResolvedRunnerExe> {
    if let Some((info, exe)) = crate::state::lkg_on_disk(&state.config, RunnerBuildProfile::Dev) {
        return Some(lkg_resolution(info, exe, RunnerBuildProfile::Dev, None));
    }
    match resolve_source_exe_detailed(state, RunnerBuildProfile::Dev).await {
        Ok(r) if matches!(r.origin, ExeOrigin::Slot(_)) => Some(r),
        _ => None,
    }
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

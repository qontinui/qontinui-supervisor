use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_stream::StreamExt;

use crate::config::RUNNER_API_PORT;
use crate::health_cache::{CachedRunnerHealth, RecentCrashSummary, RunnerStatus, UiErrorSummary};
use crate::sdk_features::{SDK_FEATURES, SDK_FEATURE_DOC_URL};
use crate::state::{SharedState, SseConnectionGuard};
use qontinui_types::wire::runner_kind::RunnerKind;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    /// Why `status` is not the plain process/API verdict, when it is not.
    ///
    /// Set today only when the primary runner is running and answering but
    /// reports ITSELF errored (e.g. a PLACEHOLDER frontend: `frontendReady:
    /// false`) — the case that used to read `status: "healthy"` beside a
    /// runner `errored`. Absent otherwise. Plan
    /// `2026-10-05-supervisor-first-start-embeds-placeholder-frontend`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    pub runner: RunnerHealth,
    pub ports: PortsHealth,
    pub watchdog: WatchdogHealth,
    pub build: BuildHealth,
    pub expo: ExpoHealth,
    pub supervisor: SupervisorInfo,
    /// Multi-runner status array (includes all managed runners).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<RunnerInstanceHealth>,
    /// SDK feature inventory baked at compile time. Lets test drivers tell
    /// in one round-trip whether the running supervisor binary's bundled
    /// `@qontinui/ui-bridge` SDK includes a feature they need; an absent
    /// entry means the binary predates that feature's SDK release.
    /// See `crate::sdk_features` for the source of truth.
    #[serde(rename = "sdkFeatures")]
    pub sdk_features: Vec<&'static str>,
    /// Documentation URL describing what each `sdkFeatures` entry means.
    #[serde(rename = "sdkFeaturesDocUrl")]
    pub sdk_features_doc_url: &'static str,
    /// Identifier for the embedded frontend bundle this supervisor is
    /// currently serving. Stable across the life of the supervisor process,
    /// changes when the supervisor binary is rebuilt with a fresh
    /// `dist/index.html` embedded. Connected dashboard tabs read this from
    /// the SSE stream and compare against the `<meta name="build-id">` that
    /// was injected at HTML serve time so a rebuild can prompt them to
    /// refresh.
    #[serde(rename = "buildId")]
    pub build_id: String,
    /// **Capability flag: does this supervisor scope the kill-on-exit
    /// JobObject to temp runners only?**
    ///
    /// Always serialized, always `true` on this build — its *presence* is the
    /// signal, not its value. A supervisor built before 2026-07-28 assigned
    /// EVERY runner it spawned to the `KILL_ON_JOB_CLOSE` job, so stopping it
    /// reaped the operator's primary (2026-07-27 incident); that binary has no
    /// code for this field and omits it entirely.
    ///
    /// `scripts/restart-supervisor.ps1` reads it from `GET /health` before
    /// stopping the RUNNING supervisor. A process cannot be removed from a
    /// Windows job after assignment, so what matters is which binary made the
    /// assignment — not which binary is on disk. Field present ⇒ the running
    /// supervisor's job holds temp runners only ⇒ the pre-flight
    /// "non-temp runner is running" guard is unnecessary and skips itself.
    /// Field absent ⇒ pre-fix supervisor ⇒ the guard stands. Without this the
    /// guard would refuse every restart forever (the primary is always
    /// running on this fleet), training operators into a permanent
    /// `-ForceKillRunners`, which disarms the guard on exactly the binary
    /// where it matters.
    pub ephemeral_job_temp_only: bool,
    /// Live count of in-flight SSE connections across every long-lived
    /// streaming endpoint (`/health/stream`, `/logs/stream`,
    /// `/expo/logs/stream`, `/runners/{id}/logs/stream`,
    /// `/supervisor-bridge/commands/stream`). Each handler holds an
    /// [`crate::state::SseConnectionGuard`] for the lifetime of its
    /// response stream; the count drops to 0 when the graceful-shutdown
    /// drain has released every active subscriber. Surfaced for ops
    /// visibility — verifying the drain works no longer requires
    /// hand-opening a stream and triggering shutdown.
    pub sse_active_connections: usize,
}

#[derive(Serialize, Clone)]
pub struct RunnerInstanceHealth {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub kind: RunnerKind,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub api_responding: bool,
    pub watchdog_status: WatchdogHealth,
    /// UI-level error reported by the runner's /health endpoint (Phase 3J.1).
    /// `None` when the runner reports no error or when the runner is too old
    /// to include the `ui_error` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_error: Option<UiErrorSummary>,
    /// Most recent Rust crash dump surfaced by the runner's /health endpoint.
    /// `None` when the runner has no fresh dump on disk or predates the
    /// crash-dump scanner (post-3J follow-up). Distinct from `ui_error`:
    /// non-unwinding panics abort the process before the React boundary sees
    /// them, so this is the only signal for that class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_crash: Option<RecentCrashSummary>,
    /// Supervisor-derived status (healthy / degraded / errored / offline /
    /// starting). Combines process liveness with the runner's self-reported
    /// `ui_error` + `derived_status` + `recent_crash`.
    pub derived_status: RunnerStatus,
    /// The wedge verdict, identical in shape and value to the one on
    /// `GET /runners`: `{"state": "responding"|"wedged"|"stopped"|"unknown",
    /// "unresponsive_since": <rfc3339>|null}`.
    ///
    /// The health refresher has computed this on every tick since the field
    /// was added to `CachedRunnerHealth` -- and the mapping into this struct
    /// dropped it, so the one surface that says "healthy" out loud could not
    /// see the state that most contradicts it. `running` + `api_responding`
    /// below are unchanged; read `liveness`.
    pub liveness: crate::state::RunnerLiveness,
    /// When this runner's API was last seen responding, RFC3339. `null` =
    /// never observed responding, which is UNKNOWN, not "never ran".
    pub last_seen_responding_at: Option<String>,
    /// Whether a listener held the runner's port on the most recent probe.
    /// Together with `api_responding` this is the raw pair the verdict above
    /// is derived from, so an operator can check the derivation rather than
    /// trust it.
    pub port_open: bool,
}

#[derive(Serialize)]
pub struct ExpoHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub configured: bool,
}

#[derive(Serialize)]
pub struct RunnerHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub api_responding: bool,
    /// The primary's wedge verdict -- same shape as `runners[].liveness` and
    /// as `GET /runners`. This block is what the dashboard header renders, and
    /// a header that reads `running` alone is exactly how a runner that held
    /// its port for ~14h while answering nothing kept showing as healthy.
    pub liveness: crate::state::RunnerLiveness,
    /// When the primary's API was last seen responding, RFC3339.
    pub last_seen_responding_at: Option<String>,
}

#[derive(Serialize)]
pub struct PortsHealth {
    pub api_port: PortStatus,
}

#[derive(Serialize)]
pub struct PortStatus {
    pub port: u16,
    pub in_use: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct WatchdogHealth {
    pub enabled: bool,
    pub restart_attempts: u32,
    pub last_restart_at: Option<String>,
    pub disabled_reason: Option<String>,
    pub crash_count: usize,
    /// The TRUE global arm for crash-only auto-restart
    /// (`config.watchdog_enabled_at_start && !crash_restart_env_disabled()`),
    /// NOT the per-runner `enabled`. When `false`, a crash of this runner will
    /// NOT be auto-restarted even though `enabled` may read `true` — the two no
    /// longer conflate. Computed by
    /// `process::manager::crash_restart_globally_armed`.
    pub crash_restart_armed: bool,

    // ── The serving arm (plan 2026-09-03-runner-zombie-serving-watchdog) ──
    //
    // Same one-helper-everywhere discipline as `crash_restart_armed`: the arm
    // is surfaced wherever the watchdog is, so "watchdog enabled" can never
    // imply a protection that is not there.
    /// The TRUE global arm for SERVING restarts
    /// (`config.watchdog_enabled_at_start && !serving_restart_env_disabled()`).
    /// A wedged runner is auto-restarted only when this is `true`.
    pub serving_restart_armed: bool,
    /// Serving restarts taken for this runner in this supervisor's lifetime.
    pub serving_restart_attempts: u32,
    pub last_serving_restart_at: Option<String>,
    /// Set when the serving-loop guard trips — distinct from
    /// `disabled_reason`, so a crash-loop disarm and a serving-loop disarm are
    /// never mistaken for each other.
    pub serving_disabled_reason: Option<String>,
}

impl WatchdogHealth {
    /// Snapshot the live per-runner crash-only watchdog state (see
    /// `process::manager::maybe_crash_restart` for who maintains it). `armed`
    /// is the global crash-restart arm (see
    /// [`crate::process::manager::crash_restart_globally_armed`]); it is passed
    /// in because `WatchdogState` only carries the per-runner `enabled` bit.
    ///
    /// BOTH global arms are parameters. There is deliberately no one-arm
    /// convenience form: a caller that supplied only the crash arm would have
    /// to invent a value for the serving arm, and an invented arm that read
    /// `true` would advertise a protection that is not there — the exact
    /// conflation `crash_restart_armed` was added to end.
    pub fn from_state_with_arms(
        wd: &crate::state::WatchdogState,
        crash_armed: bool,
        serving_armed: bool,
    ) -> Self {
        Self {
            enabled: wd.enabled,
            restart_attempts: wd.restart_attempts,
            last_restart_at: wd.last_restart_at.map(|t| t.to_rfc3339()),
            disabled_reason: wd.disabled_reason.clone(),
            crash_count: wd.crash_history.len(),
            crash_restart_armed: crash_armed,
            serving_restart_armed: serving_armed,
            serving_restart_attempts: wd.serving_restart_attempts,
            last_serving_restart_at: wd.last_serving_restart_at.map(|t| t.to_rfc3339()),
            serving_disabled_reason: wd.serving_disabled_reason.clone(),
        }
    }

    /// Fallback value when no live `WatchdogState` is reachable: no managed
    /// primary exists, or (on the sync SSE path) the lock was contended
    /// this tick. Keeps the JSON shape stable for API consumers.
    pub fn unavailable() -> Self {
        Self {
            enabled: false,
            restart_attempts: 0,
            last_restart_at: None,
            disabled_reason: Some("watchdog state unavailable".to_string()),
            crash_count: 0,
            crash_restart_armed: false,
            serving_restart_armed: false,
            serving_restart_attempts: 0,
            last_serving_restart_at: None,
            serving_disabled_reason: None,
        }
    }
}

#[derive(Serialize)]
pub struct BuildHealth {
    pub in_progress: bool,
    /// Number of build pool slots currently available for new builds.
    /// 0 means the pool is saturated; >0 means new `spawn-test {rebuild: true}`
    /// calls will begin immediately without queuing.
    pub available_slots: usize,
    pub error_detected: bool,
    pub last_error: Option<String>,
    pub last_build_at: Option<String>,
    /// True when at least one build slot embeds a stale frontend because its
    /// most recent `npm run build` failed but a cargo build proceeded using a
    /// prior `dist/` snapshot. Clears when a subsequent npm build on that
    /// slot succeeds.
    pub frontend_stale_any: bool,
    /// Last-known-good runner binary metadata. `None` until the first
    /// successful build (or after a fresh checkout where `target-pool/lkg/`
    /// doesn't exist). Agents deciding whether to fall back to the LKG when
    /// their own build fails should compare `built_at` (RFC3339) against the
    /// mtime of every file they've changed. If `built_at` is later than the
    /// max file mtime, the LKG already contains those changes and is safe to
    /// run via `POST /runners/spawn-test {use_lkg: true}`. Otherwise the LKG
    /// predates the changes and would silently run stale code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lkg: Option<LkgHealth>,
}

#[derive(Serialize)]
pub struct LkgHealth {
    /// RFC3339 wall-clock time the LKG build completed. THIS is the value
    /// agents compare against `mtime(changed files)` to decide LKG safety.
    pub built_at: String,
    /// Pool slot the LKG exe was copied from at build time. Informational —
    /// the LKG file lives at a fixed path independent of slot state.
    pub source_slot: usize,
    /// Byte size of the LKG exe. Useful for spotting truncated copies.
    pub exe_size: u64,
    /// Git SHA of the live tree the LKG exe was built from (#65). `None` when
    /// the git probe failed at build time, or when hydrated from a legacy
    /// `lkg.json` predating the provenance fields. The artifact is
    /// self-describing on disk — surface it rather than stripping it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Which tree the LKG exe was built from (#65). Serializes as
    /// `"live_tree"`/`"override"` via `BuildSource`'s serde; always
    /// `"live_tree"` for any LKG written from #65 forward (override builds are
    /// never promoted to LKG).
    pub source: crate::process::manager::BuildSource,
}

#[derive(Serialize)]
pub struct SupervisorInfo {
    /// The crate version. Hardcoded in `Cargo.toml` and unchanged for the life
    /// of the repo, so it identifies the PRODUCT, never the build — use
    /// [`Self::built_from_sha`] to tell two supervisor binaries apart.
    pub version: String,
    pub project_dir: String,
    /// **The commit THIS supervisor binary was compiled from**: a bare
    /// lowercase-hex git sha — the full 40 characters for any build that read
    /// it from git, and an unambiguous prefix only if a build-env override
    /// supplied one — or `null` when the build could not establish a commit at
    /// all (built outside a git checkout / no git on PATH).
    ///
    /// Shape is a consumed contract — it is fed straight to
    /// `git merge-base --is-ancestor <fix-sha> <built_from_sha>` to answer "is
    /// the running supervisor newer than fix X?", which before this field had
    /// no read-only answer at all (2026-08-04: the fallback measured an exe
    /// mtime, and the wrong exe at that). So: **a bare sha, never a timestamp
    /// and never a composite.** The neighbouring `buildId` on this same
    /// response is the RUNNER's and is an ISO timestamp; the runner's own
    /// `/health` spells the same name as `<sha>-<epoch-ms>`. Two shapes for one
    /// name already exist — this field must not become a third. Full shape
    /// rationale: [`crate::self_provenance`].
    ///
    /// `null` is UNKNOWN, not "clean" and not "old".
    pub built_from_sha: Option<String>,
    /// Whether the working tree carried modified TRACKED files when this binary
    /// was built; `null` when unknown.
    ///
    /// Carried as a SEPARATE field rather than a `-dirty` suffix so
    /// [`Self::built_from_sha`] stays directly git-resolvable. `true` makes the
    /// sha a lower bound: "contains fix X" still holds, "does not contain fix
    /// X" is not conclusive.
    pub built_from_dirty: Option<bool>,
}

impl SupervisorInfo {
    /// The live values for this running binary: crate version + the compile-time
    /// build provenance. One constructor so the `GET /health` and SSE arms can
    /// never report different provenance for the same process.
    pub fn current(project_dir: String) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            project_dir,
            built_from_sha: crate::self_provenance::built_from_sha().map(str::to_string),
            built_from_dirty: crate::self_provenance::built_from_dirty(),
        }
    }
}

/// Determine the overall health status string based on runner, API, and build state.
/// This is a pure function extracted for testability.
pub fn determine_overall_status(
    runner_running: bool,
    api_responding: bool,
    build_in_progress: bool,
) -> &'static str {
    if runner_running && api_responding {
        "healthy"
    } else if runner_running && !api_responding {
        "degraded"
    } else if api_responding {
        "external"
    } else if build_in_progress {
        "building"
    } else {
        "stopped"
    }
}

/// The top-level `status` plus the reason it was overridden, if it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverallStatus {
    pub status: &'static str,
    pub reason: Option<String>,
}

/// How long the primary's own `/health` must report `frontendReady: false`
/// WITHOUT A BREAK before the top-level status says `degraded`.
///
/// Why 120 s, from the runner's own code (qontinui-runner origin/main
/// b58a81dce) and one measured recovery:
/// - `frontendReady` is `frontendState == Responsive`
///   (`mcp_api.rs:1390`), so it is false while the WebView boots and whenever
///   no pong arrived for `UI_STALE_AFTER_MS` = 30 s (`ui_error.rs:220`). Rust
///   pings every 3 s; a healthy boot pongs within the runner's own 10 s
///   UI-Bridge readiness budget (`main.rs:2612`). Boot is far inside 120 s.
/// - The runner itself calls the UI dead only at `UI_DEAD_AFTER_MS` = 90 s
///   (`ui_error.rs:237`, "30 consecutive missed pings"). The supervisor must
///   not call it sooner than the runner does.
/// - The runner's in-process WebView recovery waits 60 s for a reload to pong
///   and then recreates the window; on this box (2026-09-30 02:51:03 →
///   02:52:36, `webview_recovery`) that cycle took 93 s. 120 s lets a
///   self-healing runner recover without flapping the top level.
///
/// The placeholder-frontend runner of 2026-10-05 never pongs at all, so any
/// grace catches it; the grace only buys freedom from false positives.
/// The clock starts when the supervisor FIRST OBSERVES `false` (the health
/// cache refreshes every 2 s and restarts the run on a pid change), so the
/// runner's API bootstrap time before `/health` answers never counts.
pub const PRIMARY_FRONTEND_NOT_READY_GRACE_SECS: i64 = 120;

/// The primary runner's frontend, as the health cache last observed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrimaryFrontend {
    /// Start of the current unbroken run of `frontendReady == false`
    /// observations; `None` when the last observation was not `false`
    /// (ready, or UNKNOWN — an older runner that publishes no `frontendReady`).
    pub not_ready_since: Option<chrono::DateTime<chrono::Utc>>,
    /// The runner's `frontendState`, when it published one.
    pub state: Option<String>,
}

impl PrimaryFrontend {
    /// Read from the primary's cached snapshot. No snapshot = UNKNOWN = no
    /// observation, which never overrides the top-level status.
    pub fn from_snapshot(snap: Option<&CachedRunnerHealth>) -> Self {
        match snap {
            Some(c) => Self {
                not_ready_since: c.frontend_not_ready_since,
                state: c.frontend_state.clone(),
            },
            None => Self::default(),
        }
    }
}

/// The one rule both `GET /health` and the SSE stream use for the top-level
/// status, so the two surfaces cannot diverge.
///
/// [`determine_overall_status`] answers from process + API liveness only, so
/// the 2026-10-05 runner with a PLACEHOLDER frontend (`frontendReady: false`,
/// `frontendState: "window_not_visible"`) read `healthy`. Here a primary whose
/// frontend has been not-ready for at least
/// [`PRIMARY_FRONTEND_NOT_READY_GRACE_SECS`] turns a `healthy` base into
/// `degraded`, with a reason naming the duration and `frontendState`.
///
/// Scoped to the FRONTEND on purpose — not to the runner's `errored` status.
/// That status folds in `recent_crash`, a startup scan of an unclean PRIOR
/// shutdown that stays set until a user dismisses it (qontinui-runner
/// `crash_observability.rs:512`, folded by `ui_error.rs:1501`
/// `compute_derived_status`), so keying on it would read `degraded` for a
/// runner's whole life after any power loss. An absent `frontendReady` (older
/// runner) is UNKNOWN and changes nothing.
///
/// `primary_legacy_exe` = the primary was started from the legacy cargo-target
/// exe ([`crate::process::manager::ExeOrigin::is_legacy_fallback`]); the reason
/// then says so and names the rebuild that produces a real build-pool slot.
///
/// **Display only.** No supervisor code path acts on this top-level string
/// (the watchdog keys on per-runner liveness, not on it), so a `degraded`
/// here can never trigger a restart.
pub fn resolve_overall_status(
    runner_running: bool,
    api_responding: bool,
    build_in_progress: bool,
    frontend: &PrimaryFrontend,
    primary_legacy_exe: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> OverallStatus {
    let base = determine_overall_status(runner_running, api_responding, build_in_progress);
    if base == "healthy" {
        if let Some(since) = frontend.not_ready_since {
            let secs = (now - since).num_seconds();
            if secs >= PRIMARY_FRONTEND_NOT_READY_GRACE_SECS {
                let state = frontend.state.as_deref().unwrap_or("unknown");
                let mut why = format!(
                    "primary runner frontend not ready for {secs}s (frontendState={state})"
                );
                if primary_legacy_exe {
                    why.push_str(
                        "; the primary runs the legacy cargo-target exe (no build-pool slot) \
                         — POST /runner/restart {\"rebuild\":true} builds a real slot",
                    );
                }
                return OverallStatus {
                    status: "degraded",
                    reason: Some(why),
                };
            }
        }
    }
    OverallStatus {
        status: base,
        reason: None,
    }
}

/// Build runner instance health from the cached snapshot (sync-safe for SSE).
fn build_sse_runners(state: &SharedState) -> Vec<RunnerInstanceHealth> {
    match state.cached_runner_health.try_read() {
        Ok(snapshots) => snapshots
            .iter()
            .map(|r: &CachedRunnerHealth| RunnerInstanceHealth {
                id: r.id.clone(),
                name: r.name.clone(),
                port: r.port,
                kind: r.kind.clone(),
                running: r.running,
                pid: r.pid,
                started_at: None, // Not cached — use GET /runners for full detail
                api_responding: r.api_responding,
                watchdog_status: r.watchdog.clone(),
                ui_error: r.ui_error.clone(),
                recent_crash: r.recent_crash.clone(),
                derived_status: r.derived_status.clone(),
                liveness: r.liveness,
                last_seen_responding_at: r.last_seen_responding_at.map(|t| t.to_rfc3339()),
                port_open: r.port_open,
            })
            .collect(),
        Err(_) => Vec::new(), // Lock contended, skip this tick
    }
}

/// `GET /health`. Adds the origin guard's `originGuard` block, rendered for the
/// class of this caller (`recent` only for non-browser and same-origin callers).
pub async fn health(
    State(state): State<SharedState>,
    guard: Option<axum::Extension<crate::origin_guard::OriginGuardContext>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let response = build_health_response(&state).await;
    // Serialization failure answers 500, as axum's `Json<HealthResponse>` did.
    let mut value = match serde_json::to_value(&response) {
        Ok(v) => v,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("health serialization failed: {e}") })),
            )
                .into_response()
        }
    };
    if let (Some(axum::Extension(ctx)), Some(obj)) = (guard, value.as_object_mut()) {
        obj.insert("originGuard".to_string(), ctx.health_json());
    }
    Json(value).into_response()
}

pub async fn build_health_response(state: &SharedState) -> HealthResponse {
    let build = state.build.read().await;
    let expo = state.expo.read().await;
    let frontend_stale_any = state.build_pool.any_slot_has_stale_frontend().await;
    let lkg = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .map(|info| LkgHealth {
            built_at: info.built_at.to_rfc3339(),
            source_slot: info.source_slot,
            exe_size: info.exe_size,
            sha: info.sha.clone(),
            source: info.source,
        });

    // Read from background health cache instead of live port checks (~100µs vs ~3s)
    let cached = state.cached_health.read().await;
    let api_responding = cached.runner_responding;
    let api_in_use = cached.runner_port_open;
    drop(cached);

    // Use primary ManagedRunner state (not the legacy state.runner which is never
    // updated for user-managed runners, causing "stopped" even when the runner is UP).
    //
    // `liveness` is classified from the SAME cached pair read just above
    // (`api_in_use` / `api_responding`), which is the same pair
    // `GET /runners` uses — so the two surfaces cannot disagree about
    // whether the primary is wedged.
    let (
        primary_running,
        primary_pid,
        primary_started_at,
        primary_watchdog,
        primary_liveness,
        primary_last_seen,
        primary_frontend,
        primary_legacy_exe,
    ) = if let Some(primary) = state.get_primary().await {
        let frontend = PrimaryFrontend::from_snapshot(
            state
                .cached_runner_health
                .read()
                .await
                .iter()
                .find(|c| c.id == primary.config.id),
        );
        let legacy_exe = primary
            .resolved_exe
            .read()
            .await
            .as_ref()
            .is_some_and(|r| r.origin.is_legacy_fallback());
        let watchdog = {
            let wd = primary.watchdog.read().await;
            WatchdogHealth::from_state_with_arms(
                &wd,
                crate::process::manager::crash_restart_globally_armed(&state.config),
                crate::process::manager::serving_restart_globally_armed(&state.config),
            )
        };
        let pr = primary.runner.read().await;
        (
            pr.running,
            pr.pid,
            pr.started_at,
            watchdog,
            pr.liveness(api_in_use, api_responding),
            pr.last_seen_responding_at,
            frontend,
            legacy_exe,
        )
    } else {
        // Fallback to legacy state.runner if no managed primary exists
        let runner = state.runner.read().await;
        (
            runner.running,
            runner.pid,
            runner.started_at,
            WatchdogHealth::unavailable(),
            runner.liveness(api_in_use, api_responding),
            runner.last_seen_responding_at,
            PrimaryFrontend::default(),
            false,
        )
    };

    let overall = resolve_overall_status(
        primary_running,
        api_responding,
        build.build_in_progress,
        &primary_frontend,
        primary_legacy_exe,
        chrono::Utc::now(),
    );

    // Build multi-runner status array. The `watchdog_status` field reports
    // the per-runner crash-only watchdog (`WatchdogState`, maintained by
    // `process::manager::maybe_crash_restart`).
    //
    // ui_error + derived_status come from the background health-cache refresher
    // (which GETs each runner's /health every 2s). We index into that snapshot
    // by runner id to avoid issuing another round of HTTP calls on the hot path.
    let managed_runners = state.get_all_runners().await;
    let cached_snapshots = state.cached_runner_health.read().await;
    let mut runners_health = Vec::new();
    for managed in &managed_runners {
        let watchdog_status = {
            let wd = managed.watchdog.read().await;
            WatchdogHealth::from_state_with_arms(
                &wd,
                crate::process::manager::crash_restart_globally_armed(&state.config),
                crate::process::manager::serving_restart_globally_armed(&state.config),
            )
        };
        let mr = managed.runner.read().await;
        let mc = managed.cached_health.read().await;
        let cached = cached_snapshots.iter().find(|c| c.id == managed.config.id);
        runners_health.push(RunnerInstanceHealth {
            id: managed.config.id.clone(),
            name: managed.config.name.clone(),
            port: managed.config.port,
            kind: managed.config.kind(),
            running: mr.running,
            pid: mr.pid,
            started_at: mr.started_at.map(|t| t.to_rfc3339()),
            api_responding: mc.runner_responding,
            watchdog_status,
            ui_error: cached.and_then(|c| c.ui_error.clone()),
            recent_crash: cached.and_then(|c| c.recent_crash.clone()),
            derived_status: cached.map(|c| c.derived_status.clone()).unwrap_or_default(),
            liveness: mr.liveness(mc.runner_port_open, mc.runner_responding),
            last_seen_responding_at: mr.last_seen_responding_at.map(|t| t.to_rfc3339()),
            port_open: mc.runner_port_open,
        });
    }
    drop(cached_snapshots);

    HealthResponse {
        status: overall.status.to_string(),
        status_reason: overall.reason,
        runner: RunnerHealth {
            running: primary_running,
            pid: primary_pid,
            started_at: primary_started_at.map(|t| t.to_rfc3339()),
            api_responding,
            liveness: primary_liveness,
            last_seen_responding_at: primary_last_seen.map(|t| t.to_rfc3339()),
        },
        ports: PortsHealth {
            api_port: PortStatus {
                port: RUNNER_API_PORT,
                in_use: api_in_use,
            },
        },
        watchdog: primary_watchdog,
        build: BuildHealth {
            in_progress: build.build_in_progress,
            available_slots: state.build_pool.permits.available_permits(),
            error_detected: build.build_error_detected,
            last_error: build.last_build_error.clone(),
            last_build_at: build.last_build_at.map(|t| t.to_rfc3339()),
            frontend_stale_any,
            lkg,
        },
        expo: ExpoHealth {
            running: expo.running,
            pid: expo.pid,
            port: expo.port,
            configured: state.config.expo_dir.is_some(),
        },
        supervisor: SupervisorInfo::current(state.config.project_dir.display().to_string()),
        runners: runners_health,
        sdk_features: SDK_FEATURES.to_vec(),
        sdk_features_doc_url: SDK_FEATURE_DOC_URL,
        build_id: state.build_id.clone(),
        sse_active_connections: state.active_sse_connections.load(Ordering::Relaxed),
        // Hardcoded `true`: this binary contains
        // `process::job::should_assign_to_ephemeral_job`, so its job holds
        // temp runners only. Presence is the capability signal — see the
        // field's doc comment.
        ephemeral_job_temp_only: true,
    }
}

/// GET /health/stream — SSE stream that pushes health updates every 3s.
/// Only emits an event when the serialized health JSON changes from the previous tick.
///
/// The stream terminates as soon as `state.shutdown_signal()` fires so that
/// `axum::serve(..).with_graceful_shutdown(..)` can complete its drain phase
/// promptly. Without this, the supervisor's own dashboard webview keeps a
/// `/health/stream` connection open indefinitely, the drain never completes,
/// and `POST /supervisor/shutdown` results in a 30+ second hang before the
/// process exits.
/// Build a `HealthResponse` snapshot using sync-safe `try_read` calls so it
/// can run inside the SSE stream's `map` closure without blocking the tick
/// loop. When `build_id_override` is `Some(id)`, the returned response's
/// `build_id` field is set to that value verbatim — used by the synthetic
/// build-id injection path
/// (see [`crate::routes::dev_endpoints::emit_build_id`]). When `None`, the
/// supervisor's real `state.build_id` is used.
///
/// Returns `Err(())` when any lock is contended; the SSE caller should emit
/// a keepalive comment instead of a stale event in that case. The error is
/// `()` because every lock failure mode is identical from the stream's
/// perspective: skip this tick, try again at the next interval.
fn try_build_sse_health(
    state: &SharedState,
    build_id_override: Option<&str>,
) -> Result<HealthResponse, ()> {
    // Sync-safe: try_read on every lock to avoid blocking the stream.
    let build = state.build.try_read().map_err(|_| ())?;
    let expo = state.expo.try_read().map_err(|_| ())?;
    let cached = state.cached_health.try_read().map_err(|_| ())?;
    // `?`, not `.ok()`: a contended snapshot lock used to yield
    // `primary_snapshot = None`, so for one tick the SSE path reported a
    // different status from `GET /health`. Skip the tick instead (keepalive).
    let runner_snapshots = state.cached_runner_health.try_read().map_err(|_| ())?;

    let api_responding = cached.runner_responding;
    let api_in_use = cached.runner_port_open;

    // Use the primary runner from cached snapshots (not the legacy
    // state.runner which is never updated for user-managed runners).
    let primary_snapshot = runner_snapshots.iter().find(|r| r.kind.is_primary());
    let (primary_running, primary_pid) = match primary_snapshot {
        Some(p) => (p.running, p.pid),
        None => {
            // Fallback to legacy state.runner; contended = skip the tick, for
            // the same reason as the snapshot lock above.
            let r = state.runner.try_read().map_err(|_| ())?;
            (r.running, r.pid)
        }
    };
    let primary_watchdog = primary_snapshot
        .map(|p| p.watchdog.clone())
        .unwrap_or_else(WatchdogHealth::unavailable);
    // No primary snapshot yet (first ticks after boot) is not
    // evidence of health: it is `Unknown`, the same answer every other
    // surface gives for an un-probed runner.
    let (primary_liveness, primary_last_seen) = match primary_snapshot {
        Some(p) => (p.liveness, p.last_seen_responding_at),
        None => (crate::state::RunnerLiveness::Unknown, None),
    };

    // Sync-safe, and contention SKIPS the tick (keepalive) rather than
    // dropping the legacy-exe hint, so the SSE status line can never differ
    // from `GET /health`'s for the same state.
    let primary_managed = state
        .runners
        .try_read()
        .map_err(|_| ())?
        .values()
        .find(|r| r.config.kind().is_primary())
        .cloned();
    let primary_legacy_exe = match primary_managed {
        Some(m) => m
            .resolved_exe
            .try_read()
            .map_err(|_| ())?
            .as_ref()
            .is_some_and(|r| r.origin.is_legacy_fallback()),
        None => false,
    };
    let overall = resolve_overall_status(
        primary_running,
        api_responding,
        build.build_in_progress,
        &PrimaryFrontend::from_snapshot(primary_snapshot),
        primary_legacy_exe,
        chrono::Utc::now(),
    );

    // Sync-safe scan: use try_read on each slot's frontend_stale flag.
    // If any slot's lock is contended, skip reporting staleness for that
    // slot on this tick — the flag is a UX nudge, not a hard invariant, and
    // it's fine to miss one tick.
    let frontend_stale_any = state
        .build_pool
        .slots
        .iter()
        .any(|s| s.frontend_stale.try_read().map(|g| *g).unwrap_or(false));

    let build_id = match build_id_override {
        Some(s) => s.to_string(),
        None => state.build_id.clone(),
    };

    Ok(HealthResponse {
        status: overall.status.to_string(),
        status_reason: overall.reason,
        runner: RunnerHealth {
            running: primary_running,
            pid: primary_pid,
            started_at: None, // Not available from cached snapshot
            api_responding,
            liveness: primary_liveness,
            last_seen_responding_at: primary_last_seen.map(|t| t.to_rfc3339()),
        },
        ports: PortsHealth {
            api_port: PortStatus {
                port: RUNNER_API_PORT,
                in_use: api_in_use,
            },
        },
        watchdog: primary_watchdog,
        build: BuildHealth {
            in_progress: build.build_in_progress,
            available_slots: state.build_pool.permits.available_permits(),
            error_detected: build.build_error_detected,
            last_error: build.last_build_error.clone(),
            last_build_at: build.last_build_at.map(|t| t.to_rfc3339()),
            frontend_stale_any,
            // SSE path: try_read on the LKG lock — if contended, skip the
            // field this tick (the next tick will catch up).
            lkg: state
                .build_pool
                .last_known_good
                .try_read()
                .ok()
                .and_then(|g| {
                    g.as_ref().map(|info| LkgHealth {
                        built_at: info.built_at.to_rfc3339(),
                        source_slot: info.source_slot,
                        exe_size: info.exe_size,
                        sha: info.sha.clone(),
                        source: info.source,
                    })
                }),
        },
        expo: ExpoHealth {
            running: expo.running,
            pid: expo.pid,
            port: expo.port,
            configured: state.config.expo_dir.is_some(),
        },
        supervisor: SupervisorInfo::current(state.config.project_dir.display().to_string()),
        // Read cached runner snapshots (built by background health refresher)
        runners: build_sse_runners(state),
        sdk_features: SDK_FEATURES.to_vec(),
        sdk_features_doc_url: SDK_FEATURE_DOC_URL,
        build_id,
        sse_active_connections: state.active_sse_connections.load(Ordering::Relaxed),
        // Same capability flag as the `GET /health` path — the SSE stream
        // shares this struct, so it carries it too.
        ephemeral_job_temp_only: true,
    })
}

/// Internal stream item carrying either a regular tick (no override) or a
/// synthetic build-id event (override = Some).
enum HealthTick {
    Regular,
    SyntheticBuildId(String),
}

pub async fn health_stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let interval = IntervalStream::new(tokio::time::interval(Duration::from_secs(3)))
        .map(|_| HealthTick::Regular);

    // Subscribe to the synthetic build-id broadcast channel. Lagged messages
    // are ignored — see channel docs in [`SupervisorState::synthetic_build_id_tx`].
    // Closed channels (sender dropped) just terminate this side of the merge,
    // which is fine; the regular interval stream keeps running.
    let synthetic_rx = state.synthetic_build_id_tx.subscribe();
    let synthetic = BroadcastStream::new(synthetic_rx).filter_map(|r| match r {
        Ok(s) => Some(HealthTick::SyntheticBuildId(s)),
        Err(_lagged_or_closed) => None,
    });

    // Merge the two sources. `tokio_stream::StreamExt::merge` polls both
    // streams and yields whichever has an item first. The synthetic events
    // arrive on demand (manual test trigger) so they almost always interleave
    // between regular ticks.
    let merged = interval.merge(synthetic);

    let mut last_json = String::new();

    // Cap the stream's lifetime at the shutdown signal so axum's graceful
    // drain can release this connection.
    let shutdown_state = state.clone();
    let shutdown = Box::pin(async move { shutdown_state.shutdown_signal().await });

    // Tracks this connection in `state.active_sse_connections`. Captured
    // by-move into the per-tick closure below so it lives exactly as long as
    // the stream — drop happens when axum tears down the response (client
    // disconnect, take_until on shutdown_signal, server drain).
    let conn_guard = SseConnectionGuard::new(state.active_sse_connections.clone());

    let stream = merged.map(move |tick| {
        // Hold the guard for every yielded event so the stream owns it.
        let _hold = &conn_guard;
        let state = state.clone();

        let (override_build_id, is_synthetic) = match &tick {
            HealthTick::Regular => (None, false),
            HealthTick::SyntheticBuildId(id) => (Some(id.as_str()), true),
        };

        let health = match try_build_sse_health(&state, override_build_id) {
            Ok(h) => h,
            Err(()) => return Ok(Event::default().comment("keepalive")),
        };

        let json = serde_json::to_string(&health).unwrap_or_default();

        if is_synthetic {
            // Synthetic events MUST always emit (even if the JSON happens to
            // match `last_json`) — the whole point of the injection is to
            // wake up the dashboard's `useBuildIdWatcher`. Skip the
            // change-only filter and refresh `last_json` so the next regular
            // tick correctly compares against this synthetic frame.
            last_json = json.clone();
            Ok(Event::default().event("health").data(json))
        } else if json == last_json {
            Ok(Event::default().comment("keepalive"))
        } else {
            last_json = json.clone();
            Ok(Event::default().event("health").data(json))
        }
    });

    let stream = futures::StreamExt::take_until(stream, shutdown);

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healthy_when_runner_running_and_api_responding() {
        assert_eq!(determine_overall_status(true, true, false), "healthy");
    }

    #[test]
    fn test_healthy_even_when_building() {
        // runner_running + api_responding takes precedence over build_in_progress
        assert_eq!(determine_overall_status(true, true, true), "healthy");
    }

    #[test]
    fn test_degraded_when_runner_running_but_api_not_responding() {
        assert_eq!(determine_overall_status(true, false, false), "degraded");
    }

    #[test]
    fn test_degraded_when_runner_running_api_down_and_building() {
        // runner_running + !api_responding => degraded, regardless of build state
        assert_eq!(determine_overall_status(true, false, true), "degraded");
    }

    #[test]
    fn test_building_when_runner_not_running_and_build_in_progress() {
        assert_eq!(determine_overall_status(false, false, true), "building");
    }

    #[test]
    fn test_stopped_when_nothing_running() {
        assert_eq!(determine_overall_status(false, false, false), "stopped");
    }

    #[test]
    fn test_external_when_runner_not_tracked_but_api_responding() {
        // User-started (or otherwise externally-managed) runner: supervisor
        // didn't spawn it so `running` is false, but its API is reachable.
        assert_eq!(determine_overall_status(false, true, false), "external");
        assert_eq!(determine_overall_status(false, true, true), "external");
    }

    fn not_ready_for(
        secs: i64,
        state: Option<&str>,
    ) -> (PrimaryFrontend, chrono::DateTime<chrono::Utc>) {
        let now = chrono::Utc::now();
        (
            PrimaryFrontend {
                not_ready_since: Some(now - chrono::Duration::seconds(secs)),
                state: state.map(str::to_string),
            },
            now,
        )
    }

    /// (a) The 2026-10-05 shape: primary running + answering, frontend not
    /// ready past the grace. Top-level must NOT read healthy.
    #[test]
    fn test_degraded_when_primary_frontend_not_ready_past_grace() {
        let (fe, now) = not_ready_for(125, Some("window_not_visible"));
        let o = resolve_overall_status(true, true, false, &fe, false, now);
        assert_eq!(o.status, "degraded");
        assert_eq!(
            o.reason.as_deref(),
            Some("primary runner frontend not ready for 125s (frontendState=window_not_visible)")
        );
    }

    /// Inside the grace (boot, an in-process WebView recovery) stays healthy.
    #[test]
    fn test_frontend_not_ready_inside_grace_stays_healthy() {
        let (fe, now) = not_ready_for(119, Some("booting"));
        let o = resolve_overall_status(true, true, false, &fe, true, now);
        assert_eq!(o.status, "healthy");
        assert_eq!(o.reason, None);
    }

    /// (b) The legacy-exe hint appears for the legacy origin only.
    #[test]
    fn test_frontend_reason_carries_legacy_exe_hint_only_for_legacy_origin() {
        let (fe, now) = not_ready_for(300, Some("window_not_visible"));
        let legacy = resolve_overall_status(true, true, false, &fe, true, now);
        assert_eq!(legacy.status, "degraded");
        let r = legacy.reason.unwrap();
        assert!(r.contains("window_not_visible"), "{r}");
        assert!(r.contains("legacy cargo-target exe"), "{r}");
        assert!(
            r.contains(r#"POST /runner/restart {"rebuild":true}"#),
            "{r}"
        );

        let slot = resolve_overall_status(true, true, false, &fe, false, now);
        assert!(!slot.reason.unwrap().contains("legacy"));
    }

    /// (c) No not-ready observation — a ready frontend, an older runner with no
    /// `frontendReady` (UNKNOWN), or a runner the cache calls Errored/Degraded
    /// for any other reason (e.g. a stale `recent_crash`) — keeps `healthy`
    /// with no reason, even on the legacy exe.
    #[test]
    fn test_no_frontend_observation_stays_healthy_with_no_reason() {
        let now = chrono::Utc::now();
        let o = resolve_overall_status(true, true, false, &PrimaryFrontend::default(), true, now);
        assert_eq!(o.status, "healthy");
        assert_eq!(o.reason, None);
    }

    /// `frontend_not_ready_since` is what drives it, not the runner's status:
    /// a snapshot whose `derived_status` is Errored (the `recent_crash` case)
    /// but whose frontend is ready yields no observation.
    #[test]
    fn test_errored_snapshot_with_ready_frontend_is_not_an_observation() {
        let mut snap = cached_primary_snapshot();
        snap.derived_status = RunnerStatus::Errored {
            reason: "runner restarted after Rust panic (no message captured)".to_string(),
        };
        snap.frontend_ready = Some(true);
        snap.frontend_not_ready_since = None;
        let fe = PrimaryFrontend::from_snapshot(Some(&snap));
        let o = resolve_overall_status(true, true, false, &fe, true, chrono::Utc::now());
        assert_eq!(o.status, "healthy");
        assert_eq!(o.reason, None);
    }

    /// A not-ready frontend never overrides a non-healthy base verdict
    /// (stopped / external / building / API-down degraded keep their meaning).
    #[test]
    fn test_frontend_not_ready_does_not_override_non_healthy_verdicts() {
        let (fe, now) = not_ready_for(600, Some("window_not_visible"));
        assert_eq!(
            resolve_overall_status(false, false, false, &fe, true, now).status,
            "stopped"
        );
        assert_eq!(
            resolve_overall_status(false, true, false, &fe, true, now).status,
            "external"
        );
        assert_eq!(
            resolve_overall_status(false, false, true, &fe, true, now).status,
            "building"
        );
        let api_down = resolve_overall_status(true, false, false, &fe, true, now);
        assert_eq!(api_down.status, "degraded");
        assert_eq!(api_down.reason, None);
    }

    /// A primary snapshot with every field at a neutral value.
    fn cached_primary_snapshot() -> CachedRunnerHealth {
        CachedRunnerHealth {
            id: crate::config::RunnerConfig::default_primary().id,
            name: "Primary".to_string(),
            port: RUNNER_API_PORT,
            kind: RunnerKind::Primary,
            running: true,
            pid: Some(4242),
            api_responding: true,
            ui_error: None,
            recent_crash: None,
            derived_status: RunnerStatus::Healthy,
            watchdog: WatchdogHealth::unavailable(),
            liveness: crate::state::RunnerLiveness::Responding,
            last_seen_responding_at: None,
            port_open: true,
            frontend_ready: None,
            frontend_state: None,
            frontend_not_ready_since: None,
        }
    }

    /// A supervisor state with only a primary runner, which this test marks
    /// running + responding, started from the LEGACY cargo-target exe, and
    /// whose cached snapshot says the frontend has been not ready for 200 s.
    async fn state_with_placeholder_primary(root: &std::path::Path) -> SharedState {
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
            expo_port: 8081,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
            temp_runner_display: None,
        };
        let state: SharedState = std::sync::Arc::new(crate::state::SupervisorState::new(config));
        let primary = state
            .get_primary()
            .await
            .expect("config declares a primary");
        primary.runner.write().await.running = true;
        *primary.resolved_exe.write().await = Some(crate::process::manager::ResolvedRunnerExe {
            path: root.join("target/debug/qontinui-runner"),
            origin: crate::process::manager::ExeOrigin::CargoTargetDir(
                crate::config::TargetDirSource::WorkspaceDefault,
            ),
            mtime: None,
            provenance: None,
            unverified_warning: None,
        });
        state.cached_health.write().await.runner_responding = true;
        state.cached_health.write().await.runner_port_open = true;
        let mut snap = cached_primary_snapshot();
        snap.frontend_ready = Some(false);
        snap.frontend_state = Some("window_not_visible".to_string());
        snap.frontend_not_ready_since = Some(chrono::Utc::now() - chrono::Duration::seconds(200));
        state.cached_runner_health.write().await.push(snap);
        state
    }

    /// (d) WIRING: `GET /health`'s builder reads the cached primary frontend
    /// observation and the resolved exe origin, not just the pure function.
    #[tokio::test]
    async fn test_build_health_response_degrades_on_cached_placeholder_frontend() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_placeholder_primary(dir.path()).await;
        let r = build_health_response(&state).await;
        assert_eq!(r.status, "degraded");
        let reason = r
            .status_reason
            .expect("degraded must carry a status_reason");
        assert!(
            reason.starts_with("primary runner frontend not ready for "),
            "{reason}"
        );
        assert!(
            reason.contains("(frontendState=window_not_visible)"),
            "{reason}"
        );
        assert!(reason.contains("legacy cargo-target exe"), "{reason}");
    }

    /// (d') The same through the SSE builder, which must agree with the plain
    /// handler for the same state.
    #[tokio::test]
    async fn test_sse_health_degrades_on_cached_placeholder_frontend() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_placeholder_primary(dir.path()).await;
        let r = try_build_sse_health(&state, None).expect("no lock is contended");
        assert_eq!(r.status, "degraded");
        let reason = r
            .status_reason
            .expect("degraded must carry a status_reason");
        assert!(
            reason.contains("(frontendState=window_not_visible)"),
            "{reason}"
        );
        assert!(reason.contains("legacy cargo-target exe"), "{reason}");
    }

    /// A contended snapshot lock skips the SSE tick rather than emitting a
    /// status computed without the primary snapshot.
    #[tokio::test]
    async fn test_sse_health_skips_tick_when_snapshot_lock_is_contended() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_placeholder_primary(dir.path()).await;
        let _held = state.cached_runner_health.write().await;
        assert!(try_build_sse_health(&state, None).is_err());
    }

    /// `status_reason` is omitted from the JSON when None, present when Some.
    #[test]
    fn test_status_reason_serialization() {
        let mut r = build_minimal_health_response();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("status_reason").is_none());
        r.status = "degraded".to_string();
        r.status_reason = Some("primary runner reports itself errored: x".to_string());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v["status_reason"],
            "primary runner reports itself errored: x"
        );
    }

    #[test]
    fn test_health_response_serializes_to_json() {
        let response = HealthResponse {
            status: "healthy".to_string(),
            status_reason: None,
            runner: RunnerHealth {
                running: true,
                pid: Some(1234),
                started_at: None,
                api_responding: true,
                liveness: crate::state::RunnerLiveness::Responding,
                last_seen_responding_at: None,
            },
            ports: PortsHealth {
                api_port: PortStatus {
                    port: 9876,
                    in_use: true,
                },
            },
            watchdog: WatchdogHealth {
                enabled: true,
                restart_attempts: 0,
                last_restart_at: None,
                disabled_reason: None,
                crash_count: 0,
                crash_restart_armed: false,
                serving_restart_armed: false,
                serving_restart_attempts: 0,
                last_serving_restart_at: None,
                serving_disabled_reason: None,
            },
            build: BuildHealth {
                in_progress: false,
                available_slots: 0,
                error_detected: false,
                last_error: None,
                last_build_at: None,
                frontend_stale_any: false,
                lkg: None,
            },
            expo: ExpoHealth {
                running: false,
                pid: None,
                port: 8081,
                configured: false,
            },
            supervisor: SupervisorInfo::current("/tmp/test".to_string()),
            runners: Vec::new(),
            sdk_features: SDK_FEATURES.to_vec(),
            sdk_features_doc_url: SDK_FEATURE_DOC_URL,
            build_id: "2026-04-25T00:00:00+00:00".to_string(),
            sse_active_connections: 0,
            ephemeral_job_temp_only: true,
        };

        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"running\":true"));
        assert!(json.contains("\"pid\":1234"));
        assert!(json.contains("\"api_responding\":true"));
        assert!(json.contains("\"sdkFeatures\":["));
        assert!(json.contains("\"softNavigate\""));
        assert!(json.contains("\"sdkFeaturesDocUrl\":\"https://"));
        assert!(json.contains("\"buildId\":\"2026-04-25T00:00:00+00:00\""));
        // The global arm is surfaced additively alongside the per-runner `enabled`.
        assert!(json.contains("\"crash_restart_armed\":false"), "{json}");
        // The JobObject-scoping capability flag: `restart-supervisor.ps1`
        // matches on this EXACT key to decide whether its pre-flight
        // non-temp-runner guard applies to the RUNNING supervisor. It must
        // always be present and always `true` on this build — a pre-fix
        // supervisor omits it because its binary has no such code.
        assert!(
            json.contains("\"ephemeral_job_temp_only\":true"),
            "restart-supervisor.ps1 consumes this exact key from GET /health: {json}"
        );
    }

    /// The capability flag is unconditional — no `skip_serializing_if`, no
    /// rename. Serializing a `HealthResponse` must never produce a body
    /// without it, or the PowerShell guard silently falls back to treating a
    /// fixed supervisor as pre-fix and refuses every restart.
    #[test]
    fn test_ephemeral_job_temp_only_is_always_serialized() {
        let value = serde_json::to_value(build_minimal_health_response())
            .expect("should serialize to a JSON value");
        assert_eq!(
            value.get("ephemeral_job_temp_only"),
            Some(&serde_json::Value::Bool(true)),
            "the flag must be present and true: {value}"
        );
    }

    /// The supervisor's own build commit must be present on every `/health`
    /// body, under `supervisor`, as a **bare git sha or `null`** — never a
    /// timestamp, never a `<sha>-<suffix>` composite, never `""`.
    ///
    /// A companion script feeds this value straight to
    /// `git merge-base --is-ancestor <fix-sha> <value>`, so any other shape
    /// silently answers "is the running supervisor newer than fix X?" wrong.
    /// The neighbouring `buildId` on this same response is the RUNNER's and IS
    /// a timestamp — this test is what keeps the two from converging.
    #[test]
    fn test_supervisor_built_from_sha_is_a_bare_sha_or_null() {
        let value = serde_json::to_value(build_minimal_health_response())
            .expect("should serialize to a JSON value");
        let supervisor = value
            .get("supervisor")
            .expect("the supervisor object must be present");
        let sha = supervisor
            .get("built_from_sha")
            .expect("built_from_sha must always be present, even when unknown");
        match sha {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => {
                assert!(
                    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()),
                    "built_from_sha must be a bare git object id, got {s:?}"
                );
            }
            other => panic!("built_from_sha must be a string or null, got {other}"),
        }
        // The dirty marker is a SEPARATE tri-state field, so the sha above
        // never has to be de-suffixed before it reaches git.
        let dirty = supervisor
            .get("built_from_dirty")
            .expect("built_from_dirty must always be present, even when unknown");
        assert!(
            dirty.is_null() || dirty.is_boolean(),
            "built_from_dirty must be a bool or null, got {dirty}"
        );
    }

    /// `GET /health` must carry the wedge verdict, on the primary block AND
    /// on every `runners[]` row — in the same shape `GET /runners` uses.
    ///
    /// The health refresher had computed `liveness` on every tick since the
    /// field was added to `CachedRunnerHealth`, and both mappings into
    /// `RunnerInstanceHealth` dropped it. So the endpoint whose whole job is
    /// to answer "is this healthy?" was structurally unable to report the one
    /// state that most contradicts health, while the value sat computed one
    /// struct away.
    #[test]
    fn health_response_carries_the_liveness_verdict_on_the_primary_and_every_runner() {
        let seen = chrono::Utc::now();
        let mut response = build_minimal_health_response();
        response.runner.liveness = crate::state::RunnerLiveness::UnresponsiveSince(seen);
        response.runner.last_seen_responding_at = Some(seen.to_rfc3339());
        response.runners.push(RunnerInstanceHealth {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            port: RUNNER_API_PORT,
            kind: RunnerKind::Primary,
            running: true,
            pid: Some(148320),
            started_at: None,
            api_responding: false,
            watchdog_status: WatchdogHealth::unavailable(),
            ui_error: None,
            recent_crash: None,
            derived_status: RunnerStatus::default(),
            liveness: crate::state::RunnerLiveness::UnresponsiveSince(seen),
            last_seen_responding_at: Some(seen.to_rfc3339()),
            port_open: true,
        });

        let value = serde_json::to_value(&response).expect("health response serializes");

        assert_eq!(
            value["runner"]["liveness"]["state"], "wedged",
            "the primary block must render the verdict, not just `running`: {value}"
        );
        assert_eq!(
            value["runner"]["liveness"]["unresponsive_since"],
            seen.to_rfc3339()
        );
        assert_eq!(
            value["runner"]["last_seen_responding_at"],
            seen.to_rfc3339()
        );

        let row = &value["runners"][0];
        assert_eq!(row["liveness"]["state"], "wedged", "row: {row}");
        assert_eq!(row["port_open"], true);
        assert_eq!(row["last_seen_responding_at"], seen.to_rfc3339());
        // The pre-existing fields keep their meanings and values.
        assert_eq!(row["running"], true);
        assert_eq!(row["api_responding"], false);
    }

    /// Every state renders as an object with a `state` string and an
    /// always-present `unresponsive_since`, on this surface too — a consumer
    /// must never have to branch on the JSON type to read the verdict.
    #[test]
    fn health_liveness_is_the_same_uniform_object_as_on_get_runners() {
        for (variant, name) in [
            (crate::state::RunnerLiveness::Responding, "responding"),
            (crate::state::RunnerLiveness::Stopped, "stopped"),
            (crate::state::RunnerLiveness::Unknown, "unknown"),
        ] {
            let mut response = build_minimal_health_response();
            response.runner.liveness = variant;
            let value = serde_json::to_value(&response).expect("serializes");
            assert_eq!(value["runner"]["liveness"]["state"], name);
            assert!(
                value["runner"]["liveness"]["unresponsive_since"].is_null(),
                "{name} must carry an explicit null, never a missing key"
            );
        }
    }
    /// Minimal `HealthResponse` for serialization-shape assertions.
    fn build_minimal_health_response() -> HealthResponse {
        HealthResponse {
            status: "stopped".to_string(),
            status_reason: None,
            runner: RunnerHealth {
                running: false,
                pid: None,
                started_at: None,
                api_responding: false,
                liveness: crate::state::RunnerLiveness::Unknown,
                last_seen_responding_at: None,
            },
            ports: PortsHealth {
                api_port: PortStatus {
                    port: RUNNER_API_PORT,
                    in_use: false,
                },
            },
            watchdog: WatchdogHealth::unavailable(),
            build: BuildHealth {
                in_progress: false,
                available_slots: 3,
                error_detected: false,
                last_error: None,
                last_build_at: None,
                frontend_stale_any: false,
                lkg: None,
            },
            expo: ExpoHealth {
                running: false,
                pid: None,
                port: 8081,
                configured: false,
            },
            supervisor: SupervisorInfo::current("/tmp/test".to_string()),
            runners: Vec::new(),
            sdk_features: SDK_FEATURES.to_vec(),
            sdk_features_doc_url: SDK_FEATURE_DOC_URL,
            build_id: "test".to_string(),
            sse_active_connections: 0,
            ephemeral_job_temp_only: true,
        }
    }

    /// `crash_restart_armed` reflects the GLOBAL arm passed to `from_state`,
    /// NOT the per-runner `WatchdogState.enabled`. This is the anti-lie
    /// invariant: an `enabled: true` primary under a supervisor launched
    /// without `--watchdog` (`armed = false`) must read `crash_restart_armed:
    /// false`, so "watchdog enabled" can never imply protection that isn't
    /// there.
    #[test]
    fn test_crash_restart_armed_follows_global_arm_not_per_runner_enabled() {
        let wd_enabled = crate::state::WatchdogState::new(true);
        // Per-runner enabled=true, but globally unarmed → armed must be false.
        let unarmed = WatchdogHealth::from_state_with_arms(&wd_enabled, false, false);
        assert!(unarmed.enabled, "per-runner enabled is preserved");
        assert!(
            !unarmed.crash_restart_armed,
            "crash_restart_armed must follow the global arm (false), not per-runner enabled (true)"
        );
        // Per-runner enabled=true and globally armed → armed true.
        let armed = WatchdogHealth::from_state_with_arms(&wd_enabled, true, true);
        assert!(armed.crash_restart_armed);
        // Per-runner disabled but globally armed → the arm still tracks the
        // global bit; the two axes are independent.
        let wd_disabled = crate::state::WatchdogState::new(false);
        let armed_disabled = WatchdogHealth::from_state_with_arms(&wd_disabled, true, true);
        assert!(!armed_disabled.enabled);
        assert!(armed_disabled.crash_restart_armed);
        // The unavailable fallback is never armed — on EITHER arm.
        assert!(!WatchdogHealth::unavailable().crash_restart_armed);
        assert!(!WatchdogHealth::unavailable().serving_restart_armed);
    }

    /// The serving arm is independent of the crash arm on every axis, and the
    /// JSON keeps `serving_restart_armed` even when no watchdog state is
    /// reachable — absence of the key would read as "no such protection
    /// exists" rather than "this one is off".
    #[test]
    fn serving_arm_is_reported_independently_of_the_crash_arm() {
        let mut wd = crate::state::WatchdogState::new(true);
        wd.serving_restart_attempts = 2;
        wd.serving_disabled_reason = Some("serving restart loop — operator required".to_string());

        // Crash armed, serving disarmed: the two must not track each other.
        let h = WatchdogHealth::from_state_with_arms(&wd, true, false);
        assert!(h.crash_restart_armed);
        assert!(!h.serving_restart_armed);
        assert_eq!(h.serving_restart_attempts, 2);
        assert_eq!(
            h.serving_disabled_reason.as_deref(),
            Some("serving restart loop — operator required")
        );
        // A serving-loop disarm must not be readable as a crash-loop disarm.
        assert!(h.disabled_reason.is_none());

        // And the reverse.
        let h = WatchdogHealth::from_state_with_arms(&wd, false, true);
        assert!(!h.crash_restart_armed);
        assert!(h.serving_restart_armed);

        let json = serde_json::to_value(WatchdogHealth::unavailable()).unwrap();
        assert_eq!(json["serving_restart_armed"], serde_json::json!(false));
        assert_eq!(json["serving_restart_attempts"], serde_json::json!(0));
    }

    /// `LkgHealth` surfaces the #65 provenance fields: `sha` (when present)
    /// and `source` (serialized via `BuildSource` as `live_tree`/`override`).
    #[test]
    fn test_lkg_health_serializes_sha_and_source() {
        let lkg = LkgHealth {
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
            source_slot: 1,
            exe_size: 253749760,
            sha: Some("a1b2c3d4e5f6".to_string()),
            source: crate::process::manager::BuildSource::LiveTree,
        };
        let json = serde_json::to_string(&lkg).expect("should serialize");
        assert!(json.contains("\"sha\":\"a1b2c3d4e5f6\""), "{json}");
        assert!(json.contains("\"source\":\"live_tree\""), "{json}");
    }

    /// A null `sha` (git probe failed / legacy record) is skipped from the
    /// wire (matching `skip_serializing_if`), while `source` still defaults
    /// honestly to `live_tree`.
    #[test]
    fn test_lkg_health_omits_null_sha() {
        let lkg = LkgHealth {
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
            source_slot: 0,
            exe_size: 1,
            sha: None,
            source: crate::process::manager::BuildSource::LiveTree,
        };
        let json = serde_json::to_string(&lkg).expect("should serialize");
        assert!(
            !json.contains("\"sha\""),
            "null sha should be omitted: {json}"
        );
        assert!(json.contains("\"source\":\"live_tree\""), "{json}");
    }

    #[test]
    fn test_port_status_values() {
        let port_status = PortStatus {
            port: RUNNER_API_PORT,
            in_use: false,
        };
        assert_eq!(port_status.port, 9876);
        assert!(!port_status.in_use);
    }
}

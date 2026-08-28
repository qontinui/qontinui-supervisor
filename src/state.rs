use crate::build_submissions::BuildSubmissionStore;
use crate::ci_runner_probe::CiRunnerState;
use crate::config::{RunnerConfig, SupervisorConfig};
use crate::diagnostics::DiagnosticsState;
use crate::health_cache::{CachedPortHealth, CachedRunnerHealth};
use crate::log_capture::{LogLevel, LogSource, LogState};
use crate::process::job::RunnerJob;
use crate::process::panic_log::RecentPanic;
use crate::process::stopped_cache::StoppedRunnerSnapshot;
use crate::routes::supervisor_bridge::CommandRelay;
use crate::velocity_improvement::VelocityImprovementState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{broadcast, watch, Notify, RwLock, Semaphore};

pub type SharedState = Arc<SupervisorState>;

/// Per-runner state container. Each managed runner has its own state.
pub struct ManagedRunner {
    pub config: RunnerConfig,
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub cached_health: RwLock<CachedPortHealth>,
    pub health_cache_notify: Notify,
    pub logs: LogState,
    /// Runtime-mutable protection flag. When true, this runner cannot be stopped
    /// or restarted by smart rebuild, watchdog, AI sessions, or workflow loop.
    pub protected: RwLock<bool>,
    /// When this entry was inserted into the registry. Used by the reaper to
    /// avoid removing runners that were just created but haven't started yet.
    pub created_at: std::time::Instant,
    /// Most recent startup panic detected for this runner. Populated by
    /// `monitor_runner_process_exit` when the process exits non-zero AND a
    /// fresh `runner-panic.log` is on disk. Read by `GET /runners`,
    /// `GET /runners/{id}/logs`, and the spawn-test 500/502 response.
    pub recent_panic: RwLock<Option<RecentPanic>>,
    /// Filesystem path where the runner was told to write its panic log.
    /// Set at spawn time via `QONTINUI_RUNNER_LOG_DIR`. `None` when the
    /// runner is using its default path (which the supervisor falls back to).
    pub panic_log_dir: RwLock<Option<PathBuf>>,
    /// Path to the per-spawn early-death log file (if one was opened).
    /// Surfaced via the spawn-test 500/502 error response (`early_log_path`)
    /// and `GET /runners/{id}/logs` while the runner is alive. `None` if the
    /// supervisor failed to open the file or if this runner was constructed
    /// outside the spawn flow (primary, user-imported registry entry, etc.).
    /// See `crate::process::early_log` for the lifecycle.
    pub early_log_path: RwLock<Option<PathBuf>>,
    /// Per-spawn override for the source exe to copy when starting this
    /// runner. When `Some(path)`, `start_exe_mode_for_runner` skips the
    /// usual slot-resolution chain and copies this exact path instead.
    /// Set by `spawn_test` when the caller passes `use_lkg: true` so the
    /// runner is pinned to the last-known-good binary regardless of slot
    /// state. Persists across restarts of *this* runner so a crash + manual
    /// restart still gets the LKG; cleared only by replacing the runner
    /// (which spawn-test does anyway since each call creates a fresh id).
    pub source_exe_override: RwLock<Option<PathBuf>>,
    /// Most recent observed auto-login attempt for this runner, derived from
    /// the runner's stdout/stderr stream by `log_capture` matching the
    /// `AUTH_PATTERNS` regex. `None` means we have no evidence either way —
    /// either auto-login wasn't attempted, or the runner hasn't logged yet.
    /// Surfaced via the `auth_state` field of the `spawn-test` response.
    pub last_auth_result: RwLock<Option<LastAuthResult>>,
    /// Optional work-unit / attempt correlation for previews spawned by the
    /// autonomous-dev loop (Track 2 UI-Bridge preview-verification). When a
    /// `spawn-test` request carries `unit_id`/`attempt_id`, they are stored
    /// here so the runner round-trips them in the spawn-test response and is
    /// resolvable via `GET /runners/by-unit/{unit_id}`. `None` for ordinary
    /// (non-preview) runners.
    pub preview_binding: RwLock<Option<PreviewBinding>>,
    /// Identity of the caller that spawned this runner (the `requester_id`
    /// passed to `spawn-test` / `spawn-named`). `None` for runners with no
    /// declared owner (primary, user-imported registry entries, or callers
    /// that omitted the hint).
    ///
    /// Used to (a) surface ownership in `GET /runners` so a session can pin a
    /// runner by id rather than by its (reusable) port, and (b) scope the
    /// `purge-stale` reaper to a single requester so one session's purge can
    /// never evict another session's runner.
    pub requester_id: RwLock<Option<String>>,
    /// Provenance of the exe this runner is ACTUALLY running — recorded at
    /// start time from whichever artifact was resolved (the pinned LKG record,
    /// or the picked build slot's provenance sidecar).
    ///
    /// This is the per-runner answer to "does this runner contain commit X?":
    /// `git merge-base --is-ancestor <fix-sha> <build_provenance.sha>`. Before
    /// this existed the only per-runner staleness signal was `stale_binary`,
    /// which is an **mtime** comparison and therefore blind to a build from a
    /// branch parked behind `origin/main` — a fresh mtime over stale commits is
    /// exactly the case that made a landed fix read as a regression.
    ///
    /// `None` when the runner has never been started by this supervisor, or the
    /// resolved artifact carried no provenance record (legacy `target/debug/`
    /// exe, pre-upgrade slot). Absence is honest "unknown", never "current".
    pub build_provenance: RwLock<Option<crate::process::manager::BuildProvenance>>,
    /// Which artifact this runner was actually started from, and how it was
    /// resolved: the path, the origin (build-pool slot / non-pool cargo target
    /// dir + WHICH target-dir precedence level won / an explicit pin), the
    /// mtime, and any opt-in staleness warning.
    ///
    /// Surfaced on the spawn responses so a caller can see where the binary
    /// came from. Nothing reported the path before, which is how a
    /// `spawn-test {rebuild:false}` came to launch a 54-day-old exe from
    /// `<runner>/target/debug/` (cargo had been writing to the
    /// `CARGO_TARGET_DIR` override at `src-tauri/target/` for weeks) while
    /// looking perfectly healthy.
    ///
    /// `None` until this supervisor starts the runner.
    pub resolved_exe: RwLock<Option<crate::process::manager::ResolvedRunnerExe>>,
    /// Per-spawn opt-in: allow starting from a resolved exe whose build
    /// identity cannot be established (see
    /// [`crate::process::manager::unverified_exe_gate`]). Set from the
    /// `allow_stale_fallback` field of `spawn-test` / `spawn-named` — the
    /// caller explicitly asking for "whatever exists". The staleness is still
    /// logged and still reported on the response; only the refusal is waived.
    pub allow_unverified_exe: RwLock<bool>,
}

/// Work-unit → preview correlation for a runner spawned as an attempt's
/// preview. Keyed externally by `(unit_id, attempt_id)`; stored on the
/// `ManagedRunner` so the supervisor's existing `runner_id -> ManagedRunner`
/// map is the single source of truth (no parallel side map to keep in sync).
#[derive(Clone, Debug)]
pub struct PreviewBinding {
    /// The autonomous-loop work unit this preview is built for.
    pub unit_id: String,
    /// The specific attempt within that unit (its git ref was the build source).
    pub attempt_id: Option<String>,
    /// Git SHA the preview actually ran, as reported by the runner's `/health`
    /// probe at spawn time. `None` until the health probe completes (or if the
    /// probe was skipped / the runner reported no SHA). This is the provenance
    /// fact the verifier resolves via `GET /runners/by-unit/{unit_id}`.
    pub git_sha: Option<String>,
}

/// Snapshot of the most recent auto-login attempt observed for a runner.
///
/// Populated by the log-capture path when a line in the runner's stdout/stderr
/// matches `crate::log_capture::AUTH_PATTERNS`. Only failure cases are tracked
/// here — a successful login emits no diagnostic line, and the supervisor has
/// no other observability into the runner's auth state today.
#[derive(Clone, Debug)]
pub struct LastAuthResult {
    /// True when an auto-login attempt was observed (i.e. a matching
    /// log line surfaced).
    pub attempted: bool,
    /// `Some(true)` for a confirmed success (currently never set — no
    /// success-line pattern is matched), `Some(false)` for an observed
    /// failure, `None` if the outcome could not be determined.
    pub succeeded: Option<bool>,
    /// Wall-clock time the matching log line was captured.
    pub attempt_at: DateTime<Utc>,
    /// Up to ~200 chars of the offending log line. Kept short so it can
    /// be safely embedded in API responses.
    pub rate_limit_hint: Option<String>,
}

impl ManagedRunner {
    #[allow(dead_code)]
    pub fn new(config: RunnerConfig, watchdog_enabled: bool) -> Self {
        Self::new_with_log_dir(config, watchdog_enabled, None)
    }

    /// Construct a ManagedRunner and, when `log_dir` is set, attach a
    /// per-runner persistent log file at `<log_dir>/<runner_id>.log`.
    /// Every stdout/stderr line captured by `spawn_stdout_reader` /
    /// `spawn_stderr_reader` is tee'd to this file in append mode. If the
    /// file can't be opened, the runner still starts — persistent logging
    /// is strictly best-effort.
    pub fn new_with_log_dir(
        config: RunnerConfig,
        watchdog_enabled: bool,
        log_dir: Option<&std::path::Path>,
    ) -> Self {
        let protected = config.protected;
        let logs = LogState::new();
        if let Some(dir) = log_dir {
            let path = dir.join(format!("{}.log", config.id));
            if let Some(writer) = crate::log_capture::open_append_log(&path) {
                logs.set_file_writer(Some(writer));
            }
        }
        Self {
            config,
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            cached_health: RwLock::new(CachedPortHealth::default()),
            health_cache_notify: Notify::new(),
            logs,
            protected: RwLock::new(protected),
            created_at: std::time::Instant::now(),
            recent_panic: RwLock::new(None),
            panic_log_dir: RwLock::new(None),
            early_log_path: RwLock::new(None),
            source_exe_override: RwLock::new(None),
            last_auth_result: RwLock::new(None),
            preview_binding: RwLock::new(None),
            requester_id: RwLock::new(None),
            build_provenance: RwLock::new(None),
            resolved_exe: RwLock::new(None),
            allow_unverified_exe: RwLock::new(false),
        }
    }

    /// Check if this runner is protected.
    pub async fn is_protected(&self) -> bool {
        *self.protected.read().await
    }
}

/// A timestamped origin/main drift reading, cached on
/// [`SupervisorState::origin_drift`].
///
/// `computed_at` is carried so `GET /builds` can report the reading's AGE
/// rather than presenting a cached value as current. Per fleet policy
/// `verification-and-evidence` / `unknown-must-not-render-as-a-default`, a
/// never-computed cache must be distinguishable from "no drift" -- both used to
/// serialize as `null`, which is exactly the confident-looking default that
/// clause forbids.
#[derive(Debug, Clone)]
pub struct OriginDriftSnapshot {
    /// The LKG sha this reading was computed for. Compared against the CURRENT
    /// LKG sha at read time: if the LKG moved since, the reading describes a
    /// build that is no longer current and must be reported as stale.
    pub built_sha: String,
    pub drift: crate::git_provenance::OriginMainDrift,
    pub computed_at: chrono::DateTime<chrono::Utc>,
}

pub struct SupervisorState {
    pub config: SupervisorConfig,
    /// Multi-runner map: runner_id -> ManagedRunner
    pub runners: RwLock<HashMap<String, Arc<ManagedRunner>>>,
    // Legacy single-runner fields kept for backward compat during transition.
    // These point to the primary runner's state.
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub build: RwLock<BuildState>,
    /// Parallel cargo build slot pool. Semaphore permits + per-slot target dirs.
    pub build_pool: BuildPool,
    /// Generalized build-submissions store — Row 2 Phase 3.
    /// Backs `POST /build/submit` + `GET /build/:id/status`. Submissions
    /// share the build_pool's semaphore but bring their own external
    /// worktree path (not the slot dirs). Bounded at 1000 entries with
    /// terminal-oldest LRU eviction.
    pub build_submissions: Arc<BuildSubmissionStore>,
    /// Row 10 Items 6-7 — bazel-remote CAS+AC client for the
    /// content-addressed build cache. Fail-open: a down backend never
    /// stalls or fails a build.
    pub bazel_remote: crate::bazel_remote::BazelRemoteClient,
    /// Row 10 Items 6-7 — content-hash cache telemetry (ac_hit_rate per
    /// worker/repo/profile + dual-write shadow counters). Exposed at
    /// `GET /builds/cache-stats`.
    pub cache_telemetry: crate::cache_telemetry::CacheTelemetry,
    pub ai: RwLock<AiState>,
    pub expo: RwLock<ExpoState>,
    pub diagnostics: RwLock<DiagnosticsState>,
    /// In-memory capped store of dev-action snapshots (Phase 1 of the
    /// dev-event cause-effect ledger,
    /// `plans/2026-06-07-twin-dev-event-cause-effect-ledger.md`). Mirrors
    /// `diagnostics`'s ring discipline: keyed by `action_id`, oldest-evicted.
    /// Written by the action routes (restart/spawn/build) at mint time and by
    /// the detached attribution watcher at window close; read by
    /// `GET /actions/{id}/outcome`. Coord persistence is Phase 3.
    pub dev_actions: RwLock<crate::dev_action::ActionStore>,
    pub evaluation: RwLock<EvaluationState>,
    pub velocity_tests: RwLock<VelocityTestState>,
    pub velocity_improvement: RwLock<VelocityImprovementState>,
    pub command_relay: Arc<CommandRelay>,
    pub logs: LogState,
    pub health_tx: broadcast::Sender<()>,
    pub shutdown_tx: broadcast::Sender<()>,
    /// Latched shutdown flag. Flips to `true` the first time a shutdown is
    /// signaled (HTTP endpoint or Ctrl+C). Used by [`SupervisorState::shutdown_signal`]
    /// so handlers that subscribe to `shutdown_tx` *after* the broadcast
    /// already fired still observe the shutdown — broadcast channels do
    /// not replay missed messages, but this latched bool does.
    pub shutdown_latched: AtomicBool,
    pub cached_health: RwLock<CachedPortHealth>,
    /// Cached per-runner health snapshots, updated by the background health refresher.
    /// Readable via `try_read()` in sync contexts (SSE streams).
    pub cached_runner_health: RwLock<Vec<CachedRunnerHealth>>,
    pub health_cache_notify: Notify,
    pub http_client: reqwest::Client,
    /// Runtime-configurable auto-login credentials for temp test runners.
    /// Set via `POST /test-login` and read by `forward_test_auto_login_env`.
    pub test_auto_login: RwLock<Option<(String, String)>>,
    /// Post-mortem log cache for runners removed from the active registry.
    /// Keyed by runner id. Bounded at 100 entries / 10 min TTL (see
    /// `process::stopped_cache`). Queryable via
    /// `GET /runners/{id}/logs?include_stopped=true`.
    pub stopped_runners: Arc<RwLock<HashMap<String, StoppedRunnerSnapshot>>>,
    /// Stable identifier for this supervisor *installation* (not this process).
    /// Persisted to a file under the user's local data directory
    /// (`%LOCALAPPDATA%\qontinui-supervisor\boot.id` on Windows,
    /// `~/.local/share/qontinui-supervisor/boot.id` on Linux,
    /// `~/Library/Application Support/qontinui-supervisor/boot.id` on macOS)
    /// and re-read on every supervisor startup, so it survives both bare
    /// restarts and rebuild-then-restart cycles.
    ///
    /// Returned in heartbeat responses and surfaced via
    /// `GET /supervisor-bridge/boot-id`. The dashboard's `BootIdWatcher` polls
    /// this value and reloads the page when it changes — but because the value
    /// is now stable across normal restarts, that reload acts as a *fallback*
    /// for catastrophic situations (the persistence file got deleted or
    /// corrupted, or the install was wiped) rather than the primary
    /// "new bundle available" signal. The primary signal for "a fresh frontend
    /// bundle is now being served" is `build_id` (see below), which the
    /// `BuildRefreshBanner` watches via `/health/stream`.
    pub boot_id: String,
    /// Identifier for the embedded frontend bundle this supervisor is serving.
    /// Computed at startup from the mtime of `dist/index.html` (RFC3339
    /// timestamp), or "unknown" if the file is missing. Surfaced in
    /// `GET /health`, `GET /health/stream`, and injected as a
    /// `<meta name="build-id">` tag into the served `index.html`. Connected
    /// dashboard tabs compare the meta tag value against the SSE stream so a
    /// supervisor rebuild + restart can prompt the user to refresh.
    pub build_id: String,
    /// Windows JobObject used to enforce kill-on-supervisor-exit semantics
    /// for **supervisor-owned ephemeral (temp) runners only**. Created once
    /// at startup; `start_managed_runner` assigns a spawned runner to it via
    /// `RunnerJob::assign` **iff**
    /// [`crate::process::job::should_assign_to_ephemeral_job`] says the kind
    /// is `Temp`. When this `Arc` drops (at supervisor exit) the kernel
    /// closes the last handle to the job and terminates every assigned
    /// process — so a force-killed or panicked supervisor cannot leave
    /// orphan temp runners holding slot binaries.
    ///
    /// The operator's primary, named, and external runners are deliberately
    /// assigned to **no job at all**: they are user-owned, they hold user
    /// sessions, and they must survive every supervisor exit path
    /// (`/supervisor/shutdown`, `/supervisor/restart`, `Stop-Process`,
    /// panic, BSOD). Assigning them was the 2026-07-27 incident.
    ///
    /// `None` when the OS refused to create the job (extremely rare on
    /// Windows; non-Windows builds always get a no-op stub via
    /// `process::job`'s cross-platform shim). Spawning continues either
    /// way — without the safety net, but functional.
    pub ephemeral_job: Option<Arc<RunnerJob>>,
    /// Log messages captured during synchronous `SupervisorState::new`
    /// construction that need to be routed through `state.logs.emit` once
    /// async context is available. The `logs` field is initialized inside the
    /// struct literal, so callers like the JobObject creation step (which
    /// runs *before* the `Self { ... }` block completes) can't `.await` on
    /// `state.logs.emit(...)`. They push to this buffer instead, and
    /// [`SupervisorState::flush_pending_startup_logs`] drains it after the
    /// state is constructed and the runtime is ready.
    pub pending_startup_logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
    /// Live count of in-flight SSE connections across every long-lived
    /// streaming endpoint (`/health/stream`, `/logs/stream`,
    /// `/expo/logs/stream`, `/runners/{id}/logs/stream`,
    /// `/supervisor-bridge/commands/stream`). Each handler acquires an
    /// [`SseConnectionGuard`] on entry whose `Drop` decrements this counter
    /// when the response future is torn down.
    ///
    /// Surfaced via `GET /health` as `sse_active_connections` so ops can
    /// verify the graceful-shutdown drain is actually releasing connections
    /// without having to open a stream + trigger shutdown by hand.
    pub active_sse_connections: Arc<AtomicUsize>,
    /// True when debug-only HTTP endpoints (under `/control/dev/*`) are
    /// admitted. Cached at startup from
    /// `QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS=1` so handlers don't re-read the
    /// env on every request. Off by default — debug endpoints are local-dev
    /// only and must never be exposed in shared / multi-tenant deployments.
    pub debug_endpoints_enabled: bool,
    /// Wall-clock time the current supervisor process started. Used by
    /// startup-time slot pre-flight to distinguish stale `.cargo-lock`
    /// advisory files left behind by a previous supervisor (older mtime)
    /// from locks placed by a build that's just now starting on this slot.
    pub supervisor_started_at: std::time::SystemTime,
    /// Broadcast channel for synthetic build-id injection events from the
    /// debug endpoint `POST /control/dev/emit-build-id`. Each `String` sent
    /// is a build-id value that the `/health/stream` SSE handler should
    /// emit as a one-shot synthetic `event: health` to all currently
    /// connected dashboard tabs, overriding the real
    /// [`SupervisorState::build_id`] in the JSON payload (without changing
    /// the on-disk value). Capacity 8; if multiple synthetic events arrive
    /// faster than every SSE consumer drains, the oldest is dropped — the
    /// channel is best-effort, since the goal is exercising the watcher's
    /// divergence path during manual tests, not durable delivery.
    pub synthetic_build_id_tx: broadcast::Sender<String>,
    /// CI runner state probed via WSL. Updated every 30s by
    /// `ci_runner_probe::ci_runner_probe_loop`.
    ///
    /// **The fleet heartbeat does NOT read this** — that claim was stale. The
    /// `ci_runner_labels` / `ci_runner_status` keys were deleted from the budget
    /// payload because coord's `BudgetPublishRequest` declares no such fields
    /// and `upsert_budget` never wrote them (see `fleet.rs:45-53`, pinned by a
    /// test at `fleet.rs:637`). The single consumer is `GET /ci-runner/status`,
    /// which the runner's CI Runner settings panel polls every 10 s.
    /// `coord.devices.ci_runner_status` is written independently by coord's
    /// `ci_runner_registrar` from GitHub's runners API, so the honest
    /// `distro_down` / `probe_failed` readings are a LOCAL view and never reach
    /// fleet metrics.
    pub ci_runner_state: RwLock<CiRunnerState>,
    /// Submission id of the most recent in-flight `POST /runner/fix-and-rebuild`
    /// detached rebuild, if any. `fix-and-rebuild` rebuilds the *single* live
    /// runner tree into the build-pool slots; two concurrent ones would race the
    /// same slots + LKG bookkeeping. The handler reads this under the lock: if it
    /// points at a still-non-terminal submission, the second request is an
    /// idempotent accept (returns the existing submission id) instead of kicking
    /// off a duplicate build. Cleared/overwritten when a new rebuild is started
    /// (the previous id having reached terminal). `None` until the first rebuild.
    pub fix_and_rebuild_inflight: RwLock<Option<uuid::Uuid>>,
    /// Single-flight index for `POST /runners/spawn-test`, keyed by
    /// **(requester, build target)** — see [`SpawnDedupKey`].
    ///
    /// The build pool has three slots. A caller whose `curl` appears to have
    /// returned nothing (a dropped connection, a client-side timeout, a
    /// swallowed 202) retries — and before this index the retry claimed a
    /// SECOND slot to compile the exact same tree for the exact same caller,
    /// spending a third of the pool on a build whose output already existed.
    ///
    /// [`SupervisorState::admit_spawn_build`] is the only door: a request whose
    /// key is already present JOINS the running build (same `build_id`, same
    /// runner, `"deduplicated": true`) instead of starting a second one. The
    /// entry is removed when the build reaches a terminal state.
    ///
    /// Generalises the global `fix_and_rebuild_inflight` above: that one
    /// single-flights the *one* live-tree rebuild with no key at all, which is
    /// only correct because there is exactly one such tree.
    pub spawn_test_inflight: RwLock<HashMap<SpawnDedupKey, SpawnInflight>>,
    /// Spawn-worktree containers (`<workspace_root>/.spawn-<ref>/`) that are
    /// currently being built. A `git_ref` spawn-test materializes such a
    /// container, then holds a build-pool slot while compiling the tree inside
    /// it. The container is the supervisor's to delete once idle, but it is
    /// NEVER safe to remove while that build is in flight.
    ///
    /// [`crate::routes::runners::spawn_test`] inserts the container path before
    /// the build and removes it after (success or failure). The spawn-worktree
    /// pruner ([`crate::spawn_worktree::prune_spawn_worktrees`]) consults this
    /// set as its active-build exclusion: any candidate present here is skipped,
    /// regardless of age. Keyed by the canonical container path string so the
    /// pruner's directory enumeration and the build wiring agree on identity.
    pub active_spawn_worktrees: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// Per-spawn-container **mutual exclusion** for materialize-then-build.
    ///
    /// `prepare_worktree` REUSES a container by `git checkout --force` +
    /// `git reset --hard`. Doing that while another spawn is mid-`cargo build`
    /// in the same container yanks source files out from under the compiler —
    /// producing failures that look exactly like code errors. Before
    /// 2026-07-22 this could only happen when two callers passed the SAME
    /// explicit `git_ref`; now that `origin/main` is the spawn-test default,
    /// every default spawn targets one shared `.spawn-origin_main` container,
    /// so it would be the common case rather than a rare collision.
    ///
    /// The guard is held across BOTH the materialization and the build, so a
    /// container is never reset while a build reads it. Scope is per container
    /// path: spawns of DIFFERENT refs never contend. Concurrent spawns of the
    /// SAME ref serialize — an accepted trade, since they would otherwise
    /// compile byte-identical source twice in parallel for no benefit.
    ///
    /// `std::sync::Mutex` guards only the map (never held across an await);
    /// the per-container `tokio::sync::Mutex` is the async lock actually held
    /// across the build. Entries are never evicted — one empty mutex per
    /// distinct ref ever spawned is negligible, and evicting a lock another
    /// task is queued on is the classic way to reintroduce the race.
    pub spawn_container_locks:
        std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// Cached build-artifact footprint snapshot (plan
    /// `2026-06-05-supervisor-build-artifact-footprint`). Walking the GB-scale
    /// `target-pool/slot-*` + `.spawn-*` trees is minutes-slow, so the snapshot
    /// is computed off the hot path: a background timer refreshes it (default
    /// 15 min) and `GET /builds?refresh_footprint=1` forces a synchronous
    /// recompute. The prune endpoints invalidate it after freeing bytes.
    /// `None` until the first refresh completes. Each snapshot carries its own
    /// `computed_at` so readers can judge staleness.
    pub footprint: RwLock<Option<crate::footprint::FootprintSnapshot>>,

    /// Cached origin/main drift for the LKG sha, refreshed on a timer.
    ///
    /// `GET /builds` used to compute this INLINE, and computing it runs
    /// `git fetch origin` -- a NETWORK call, with no deadline. Measured on this
    /// fleet 2026-08-10: `/builds` took 4.07s to 27.12s across 12 samples while
    /// `/health` answered in 0.0-0.1s and the disk queue sat at 0. The cost was
    /// neither disk nor memory; it was a per-request fetch. Any consumer with a
    /// timeout under ~30s saw intermittent failures, and the scoped-cleanup
    /// harness silently degraded to a guessed pool size because of it.
    ///
    /// A read endpoint must not perform network I/O. This is the same
    /// stale-while-revalidate shape as `footprint` above: the timer refreshes
    /// it, readers serve whatever is cached and NEVER block on git.
    pub origin_drift: RwLock<Option<OriginDriftSnapshot>>,
}

/// RAII guard that increments [`SupervisorState::active_sse_connections`]
/// on construction and decrements it on drop.
///
/// The guard is owned by the SSE stream itself (typically captured into the
/// stream's combinator state), so it lives exactly as long as the response
/// future. When axum drops the response — whether the client disconnected,
/// `take_until(shutdown_signal)` fired, or the server is being torn down —
/// the stream is dropped, the guard is dropped, and the counter ticks down.
pub struct SseConnectionGuard {
    counter: Arc<AtomicUsize>,
}

impl SseConnectionGuard {
    pub fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct RunnerState {
    pub process: Option<Child>,
    /// Whether the runner's HTTP API answered on the most recent probe.
    ///
    /// **This is a two-state answer to a three-state question**, which is why
    /// [`Self::last_seen_responding_at`] exists beside it. `false` conflates
    /// "stopped" with "alive but not answering" — and the second is the
    /// wedge case, where the process is running fine and holding its port.
    /// Never render this alone as "gone"; use [`Self::liveness`].
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_requested: bool,
    pub stop_requested: bool,
    pub pid: Option<u32>,
    /// When the API was last observed responding.
    ///
    /// `None` means "never seen responding since the supervisor started
    /// tracking it", which is NOT the same as "not responding now". Set on
    /// every successful probe; never cleared. Together with `running` it
    /// separates a runner that stopped from one that went quiet and how long
    /// ago (plan `2026-08-07-runner-wedge-and-supervisor-hung-blindness`,
    /// Phase 3b).
    pub last_seen_responding_at: Option<DateTime<Utc>>,
}

/// What the supervisor actually knows about a runner's liveness.
///
/// Replaces reading the `running` boolean as if it were the whole truth. The
/// dashboard lost 7 hours to that conflation on 2026-08-08: it reported
/// `running: false, pid: null` for a 411 MB process that was holding its port
/// in the same JSON document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerLiveness {
    /// API answered on the most recent probe.
    Responding,
    /// API is silent but the port is still held — the process is alive and
    /// not answering. This is the wedge state, and the one that used to be
    /// mislabelled "stopped".
    UnresponsiveSince(DateTime<Utc>),
    /// API is silent and the port is not held. Positive evidence of absence.
    Stopped,
    /// API is silent, the port is not held, and we have never seen it
    /// respond — so we cannot distinguish "never started" from "stopped".
    Unknown,
}

impl RunnerState {
    /// Classify liveness from the API probe, the port observation, and the
    /// last-responding stamp.
    ///
    /// `port_open` MUST come from an actual listener probe. Passing a stale
    /// or assumed value re-creates the conflation this exists to remove.
    pub fn liveness(&self, port_open: bool) -> RunnerLiveness {
        if self.running {
            return RunnerLiveness::Responding;
        }
        match (port_open, self.last_seen_responding_at) {
            (true, Some(at)) => RunnerLiveness::UnresponsiveSince(at),
            // Port held but never seen responding: still alive-and-quiet, and
            // the honest stamp is when we started watching, not "stopped".
            (true, None) => RunnerLiveness::Unknown,
            (false, Some(_)) => RunnerLiveness::Stopped,
            (false, None) => RunnerLiveness::Unknown,
        }
    }
}

pub struct WatchdogState {
    pub enabled: bool,
    pub restart_attempts: u32,
    pub last_restart_at: Option<DateTime<Utc>>,
    pub crash_history: Vec<DateTime<Utc>>,
    pub disabled_reason: Option<String>,
}

pub struct BuildState {
    /// True when at least one build slot is busy.
    ///
    /// Maintained by `run_cargo_build`: set to true whenever a permit is
    /// acquired (first slot goes busy), cleared when the last active slot
    /// releases its permit. Existing readers (health endpoint, smart rebuild,
    /// overnight watchdog, process manager) observe this as a coarse
    /// "is the supervisor currently compiling anything" signal.
    pub build_in_progress: bool,
    pub build_error_detected: bool,
    pub last_build_error: Option<String>,
    pub last_build_at: Option<DateTime<Utc>>,
    pub last_build_stderr: Option<String>,
}

/// Metadata for an active build on a specific slot.
#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub started_at: DateTime<Utc>,
    pub requester_id: Option<String>,
    /// What kind of rebuild: "dev" or "exe" (custom-protocol/embedded frontend).
    pub rebuild_kind: String,
}

// State of the frontend (`npm run build`) for a specific slot.
//
// `BuildSlot::frontend_stale` = true means the most recent attempt to rebuild
// the frontend for this slot failed (e.g. tsc errors). The cargo build still
// proceeded, but it re-used whatever `dist/` happened to be on disk from a
// previous successful frontend build. Callers of `spawn-test {rebuild: true}`
// surface this so they don't debug a binary that embeds a stale UI. Cleared
// on the next successful `npm run build` for this slot.

/// Cap on the per-slot rolling duration window.
pub const RECENT_BUILD_SAMPLE_COUNT: usize = 10;

/// Cap on `SlotHistory::last_error_detail` size. When a captured cargo stderr
/// blob exceeds this, the front is truncated so the tail (where the actual
/// failure message lives in cargo output) is preserved.
pub const LAST_ERROR_DETAIL_MAX_BYTES: usize = 4 * 1024;

/// Cap on `SlotHistory::last_error_log` size. Sized for inline surfacing in
/// `GET /builds` so a single curl reveals the gist without dumping a wall of
/// text. The full last build's stderr lives on the slot (`BuildSlot::last_build_log`)
/// and is fetched on demand via `GET /builds/{slot_id}/log`.
pub const LAST_ERROR_LOG_MAX_BYTES: usize = 1024;

/// Cap on `BuildSlot::last_build_log` size. Hard upper bound to avoid
/// pathological retention if a build dumps gigabytes of output. Tail is
/// preserved (where the actual error message lives in cargo output).
pub const LAST_BUILD_LOG_MAX_BYTES: usize = 1024 * 1024;

/// Per-slot build duration history. In-memory only; resets on supervisor
/// restart. Used by `GET /builds` and the 503 `build_pool_full` response to
/// estimate wait times for callers.
#[derive(Debug, Clone)]
pub struct SlotHistory {
    pub recent_durations_secs: VecDeque<f64>,
    pub total_builds: u64,
    pub successful_builds: u64,
    pub last_completed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Tail of the captured cargo stderr from the most recent failed build on
    /// this slot. Capped at [`LAST_ERROR_DETAIL_MAX_BYTES`]; oldest bytes are
    /// truncated to keep the tail (where the actual failure lives in cargo
    /// output). `None` until a failure is recorded with detail.
    pub last_error_detail: Option<String>,
    /// Short (≤[`LAST_ERROR_LOG_MAX_BYTES`]) tail of the most recent FAILED
    /// build's stderr. Surfaced inline in `GET /builds` so a single curl
    /// reveals the gist of the failure without paging through the full log.
    /// Cleared on the next successful build (success supersedes prior
    /// failures for that slot — the failure is no longer the current state).
    /// Use `GET /builds/{slot_id}/log` for the full untruncated log.
    pub last_error_log: Option<String>,
}

impl Default for SlotHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotHistory {
    pub fn new() -> Self {
        Self {
            recent_durations_secs: VecDeque::with_capacity(RECENT_BUILD_SAMPLE_COUNT),
            total_builds: 0,
            successful_builds: 0,
            last_completed_at: None,
            last_error: None,
            last_error_detail: None,
            last_error_log: None,
        }
    }

    pub fn record(
        &mut self,
        duration_secs: f64,
        success: bool,
        error: Option<String>,
        error_detail: Option<String>,
    ) {
        if self.recent_durations_secs.len() >= RECENT_BUILD_SAMPLE_COUNT {
            self.recent_durations_secs.pop_front();
        }
        self.recent_durations_secs.push_back(duration_secs);
        self.total_builds += 1;
        if success {
            self.successful_builds += 1;
            // Clear the inline error log on a green build. Rationale: the
            // surfaced field reflects the slot's CURRENT failure state, not
            // its history — a successful build supersedes any prior failure.
            // Full last-build log stays at the slot level for forensics.
            self.last_error_log = None;
        } else {
            // `last_error_log` is the 1 KiB summary surfaced inline in
            // `GET /builds`; `last_error_detail` is the longer 4 KiB view.
            // Both derive from the same captured blob so they tell a
            // consistent story.
            //
            // It is a HEAD, not a tail. `error_detail` is no longer raw cargo
            // stderr — it is a rendered document whose whole point is that the
            // cause is hoisted to the FRONT (see `build_diagnostics`). Taking
            // its last 1 KiB would hand `GET /builds` the tail of an excerpt
            // that is mostly linker flags, faithfully reproducing the
            // 2026-07-31 defect on the most-consulted surface of all.
            self.last_error_log = error_detail
                .as_deref()
                .map(|s| head_bytes_keep_utf8(s, LAST_ERROR_LOG_MAX_BYTES));
            self.last_error = error;
            self.last_error_detail = error_detail.map(truncate_error_detail_keep_tail);
        }
        self.last_completed_at = Some(Utc::now());
    }

    pub fn avg_duration_secs(&self) -> Option<f64> {
        if self.recent_durations_secs.is_empty() {
            return None;
        }
        let sum: f64 = self.recent_durations_secs.iter().sum();
        Some(sum / self.recent_durations_secs.len() as f64)
    }

    pub fn p50_duration_secs(&self) -> Option<f64> {
        if self.recent_durations_secs.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.recent_durations_secs.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[sorted.len() / 2])
    }
}

/// Return the FIRST `max_bytes` bytes of `s`, snapped back to a UTF-8
/// character boundary, with a marker when a cut was made.
///
/// The counterpart to [`tail_bytes_keep_utf8`], for the surfaces whose input
/// is a *rendered* diagnostic rather than raw cargo output. Those put the
/// cause at the front by construction, so keeping the tail throws away
/// exactly the part that was hoisted there to be seen.
pub fn head_bytes_keep_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[...truncated]", &s[..cut])
}

/// Return the last `max_bytes` bytes of `s`, snapped forward to a UTF-8
/// character boundary so the result is always valid UTF-8. Returns `s`
/// unchanged when it's already shorter than `max_bytes`.
pub fn tail_bytes_keep_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = s.len() - max_bytes;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[cut..].to_string()
}

/// Truncate `s` to at most [`LAST_ERROR_DETAIL_MAX_BYTES`] bytes by removing
/// from the front, preserving the tail. Truncates on a UTF-8 boundary so the
/// result is always valid UTF-8. When a cut is performed, a leading marker
/// is prepended so consumers know the prefix was elided.
pub fn truncate_error_detail_keep_tail(s: String) -> String {
    if s.len() <= LAST_ERROR_DETAIL_MAX_BYTES {
        return s;
    }
    let cut_target = s.len().saturating_sub(LAST_ERROR_DETAIL_MAX_BYTES);
    let mut cut = cut_target;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    let mut out = String::with_capacity(s.len() - cut + 32);
    out.push_str("[...truncated]\n");
    out.push_str(&s[cut..]);
    out
}

/// One slot in the parallel build pool.
///
/// Each slot has its own `CARGO_TARGET_DIR` so concurrent `cargo build`s do
/// not clobber each other's `target/`. The `busy` field is guarded by its
/// own lock so the slot state can be inspected without holding the larger
/// `SupervisorState::build` lock.
pub struct BuildSlot {
    pub id: usize,
    pub target_dir: PathBuf,
    pub busy: RwLock<Option<BuildInfo>>,
    /// Rolling per-slot build duration history. Separate lock from `busy` so
    /// `list_builds` can `try_read` it without blocking in-progress builds.
    pub history: RwLock<SlotHistory>,
    /// True when the most recent `npm run build` for this slot failed but the
    /// cargo build proceeded anyway using a stale `dist/` snapshot. Cleared on
    /// the next successful npm build.
    ///
    /// This is a failure-propagation surface — set in `run_build_inner` after
    /// a non-zero npm exit or spawn failure, cleared after a zero-exit npm
    /// build. Independent from the `busy`/`history` locks so readers can check
    /// it cheaply without blocking in-progress builds.
    pub frontend_stale: RwLock<bool>,
    /// Tail of cargo stderr captured by the most recent build attempt on this
    /// slot. Populated by `run_build_inner` on a non-zero cargo exit so the
    /// outer `run_cargo_build_with_requester` can fold it into
    /// [`SlotHistory::last_error_detail`] alongside the duration record.
    /// Cleared when populated; readers consume by `take`.
    pub last_build_stderr_capture: RwLock<Option<String>>,
    /// Full combined cargo stderr (and stdout, if cargo wrote any) of the
    /// most recent build attempt on this slot — success or failure. Capped
    /// at [`LAST_BUILD_LOG_MAX_BYTES`] to avoid pathological retention.
    /// Populated at the end of every cargo build, replacing whatever was
    /// there. Surfaced via `GET /builds/{slot_id}/log`.
    ///
    /// Tuple: `(captured_at, log_bytes)`. The timestamp is when the build
    /// finished, not when the log was read.
    pub last_build_log: RwLock<Option<(DateTime<Utc>, String)>>,
    /// Live broadcast of cargo stderr lines for THIS slot's currently-running
    /// build. Each subscriber sees every line `run_build_inner` reads from
    /// `child.stderr` for this slot's cargo invocation. Lines are pushed
    /// untagged (plain `String`) — the SSE handler at
    /// `GET /builds/{slot_id}/log/stream` wraps them as `event: cargo` data
    /// frames.
    ///
    /// Channel capacity is intentionally small (256): cargo's output isn't
    /// dense relative to typical SSE consumers, and the broadcast channel
    /// drops slow subscribers via `RecvError::Lagged` rather than blocking
    /// the cargo reader. Lagged events are surfaced to clients as a
    /// `event: lagged` frame so they know to fetch the full log from
    /// `GET /builds/{slot_id}/log` on completion.
    ///
    /// The channel is created once at slot construction and reused across
    /// builds — receivers from a previous build naturally start seeing the
    /// next build's lines, which is the desired "tail -f" semantics.
    pub log_stream: tokio::sync::broadcast::Sender<String>,
}

/// Metadata for the last-known-good (LKG) runner binary preserved at
/// `target-pool/lkg/qontinui-runner.exe`.
///
/// The LKG copy is captured after every successful `cargo build` and is
/// independent of slot state — a subsequent failed build that clobbers a
/// slot's exe still leaves LKG intact. Callers consult `built_at` to decide
/// whether their pending changes are reflected in the LKG binary:
///
///   if max(mtime of changed files) <= LKG.built_at  ⇒ LKG includes the
///       changes; safe to spawn with `{rebuild: false, use_lkg: true}`.
///   if max(mtime of changed files) >  LKG.built_at  ⇒ LKG predates the
///       changes; the runner would be stale. Rebuild instead.
///
/// `built_at` is the wall-clock time the cargo build completed, recorded
/// just before the exe was copied into the LKG dir. `source_slot` is the
/// pool slot the build ran on. `exe_size` is the byte size of the LKG exe
/// — useful for spotting truncated or partial copies after a crash.
///
/// `sha` and `source` record the [`BuildProvenance`](crate::process::manager::BuildProvenance)
/// of the build that produced this LKG. Since the LKG promotion gate skips
/// override builds, `source` is always [`BuildSource::LiveTree`] for any LKG
/// written from this code forward — it is recorded from provenance (not
/// hard-coded) so the record is honest by construction. Both fields are
/// `#[serde(default)]` so legacy `lkg.json` files predating this change still
/// hydrate: their `sha` reads back as `None` and `source` defaults to
/// `LiveTree` (the only kind of build that has ever been deployed from LKG).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LkgInfo {
    pub built_at: DateTime<Utc>,
    pub source_slot: usize,
    pub exe_size: u64,
    /// Git SHA of the tree that was built, or `None` when the git probe failed
    /// (or when hydrated from a legacy sidecar without this field).
    #[serde(default)]
    pub sha: Option<String>,
    /// Which tree the LKG exe was built from. Always `LiveTree` going forward
    /// (override builds never reach LKG promotion); defaults to `LiveTree` when
    /// hydrated from a legacy sidecar without this field.
    #[serde(default = "default_lkg_source")]
    pub source: crate::process::manager::BuildSource,
}

/// Default `source` for an `LkgInfo` hydrated from a legacy `lkg.json` that
/// predates the provenance fields. LKG has only ever held live-tree deploys,
/// so `LiveTree` is the honest assumption for those records.
fn default_lkg_source() -> crate::process::manager::BuildSource {
    crate::process::manager::BuildSource::LiveTree
}

/// Pool of parallel build slots.
///
/// Acquisition protocol:
/// 1. Wait on `permits.acquire_owned().await` (blocks until a slot is free).
/// 2. Scan `slots` for the first one whose `busy.is_none()`, flip it to `Some(..)`.
/// 3. Run cargo build with `CARGO_TARGET_DIR = slot.target_dir`.
/// 4. On completion, flip `slot.busy = None`; the permit is dropped automatically.
///
/// `npm_lock` serializes frontend (`npm run build`) invocations: the Tauri
/// binary embeds a single `dist/` directory via `rust-embed`, and two
/// concurrent npm builds would corrupt it. The lock is held only for the npm
/// step (~12s), not the whole cargo build (~3min), so it's a much smaller
/// serialization point than the legacy global build flag.
pub struct BuildPool {
    pub slots: Vec<Arc<BuildSlot>>,
    pub permits: Arc<Semaphore>,
    pub npm_lock: Arc<tokio::sync::Mutex<()>>,
    /// Number of callers currently waiting on `permits.acquire_owned()`.
    /// Incremented by `spawn-test` handler before awaiting, decremented after
    /// acquiring or timing out.
    pub queue_depth: Arc<AtomicUsize>,
    /// Number of callers currently waiting on `npm_lock.lock_owned()` (the
    /// serialized frontend `pnpm run build` step). Bracketed around the lock
    /// acquire in `build_monitor::run_build_inner` exactly as `queue_depth`
    /// brackets the permit acquire — `npm_lock` is a bare `Mutex<()>` with no
    /// native waiter accounting, so this counter is what lets `GET /builds`
    /// surface frontend-lock contention (`npm_lock_waiters`) and distinguish
    /// frontend starvation from cargo-slot exhaustion.
    pub npm_lock_waiters: Arc<AtomicUsize>,
    /// The slot id whose target dir holds the most recently successfully built
    /// binary. Used by `spawn-test {rebuild: false}` to locate the exe to copy.
    /// `None` at startup until the first successful build.
    pub last_successful_slot: RwLock<Option<usize>>,
    /// Metadata for the preserved last-known-good runner exe at
    /// `target-pool/lkg/qontinui-runner.exe`. Updated after every successful
    /// build; read by `spawn_test` when `use_lkg: true`. Hydrated from the
    /// `lkg.json` sidecar at supervisor startup so the value survives
    /// restarts. `None` only when no successful build has ever produced an
    /// LKG (fresh checkout) or the sidecar failed to load.
    pub last_known_good: RwLock<Option<LkgInfo>>,
}

impl BuildPool {
    pub fn new(config: &SupervisorConfig) -> Self {
        let pool_size = config.build_pool.pool_size.max(1);
        let mut slots = Vec::with_capacity(pool_size);
        for id in 0..pool_size {
            let target_dir = config.runner_slot_target_dir(id);
            // Create the dir eagerly so cargo doesn't race on it.
            if let Err(e) = std::fs::create_dir_all(&target_dir) {
                tracing::warn!(
                    "Failed to create build slot target dir {:?}: {}",
                    target_dir,
                    e
                );
            }
            let (log_tx, _) = tokio::sync::broadcast::channel::<String>(256);
            slots.push(Arc::new(BuildSlot {
                id,
                target_dir,
                busy: RwLock::new(None),
                history: RwLock::new(SlotHistory::new()),
                frontend_stale: RwLock::new(false),
                last_build_stderr_capture: RwLock::new(None),
                last_build_log: RwLock::new(None),
                log_stream: log_tx,
            }));
        }
        // Try to hydrate LKG metadata from the on-disk sidecar. We do this
        // synchronously in `BuildPool::new` (which runs once at startup) so
        // the field is correctly populated before any HTTP handler can read
        // it. A missing or unparsable sidecar is non-fatal — the LKG will
        // simply be considered absent until the next successful build.
        let lkg = load_lkg_from_disk(config);

        Self {
            slots,
            permits: Arc::new(Semaphore::new(pool_size)),
            npm_lock: Arc::new(tokio::sync::Mutex::new(())),
            queue_depth: Arc::new(AtomicUsize::new(0)),
            npm_lock_waiters: Arc::new(AtomicUsize::new(0)),
            last_successful_slot: RwLock::new(None),
            last_known_good: RwLock::new(lkg),
        }
    }

    /// Scan slots and return a snapshot of (slot_id, Option<BuildInfo>) pairs
    /// for the `GET /builds` endpoint.
    pub async fn snapshot(&self) -> Vec<(usize, PathBuf, Option<BuildInfo>)> {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let info = slot.busy.read().await.clone();
            out.push((slot.id, slot.target_dir.clone(), info));
        }
        out
    }

    /// Returns true when at least one slot has its `frontend_stale` flag set —
    /// i.e. its most recent `npm run build` failed but a cargo build proceeded
    /// anyway using a pre-existing `dist/`. Surfaced in `GET /builds` and
    /// `GET /health` so callers can notice a potentially-stale UI.
    pub async fn any_slot_has_stale_frontend(&self) -> bool {
        for slot in &self.slots {
            if *slot.frontend_stale.read().await {
                return true;
            }
        }
        false
    }

    /// Claim the first idle slot, marking it busy with the given metadata.
    /// Assumes the caller has already acquired a permit, so at least one slot
    /// is idle.
    pub async fn claim_idle_slot(&self, info: BuildInfo) -> Arc<BuildSlot> {
        for slot in &self.slots {
            let mut busy = slot.busy.write().await;
            if busy.is_none() {
                *busy = Some(info.clone());
                return slot.clone();
            }
        }
        // Unreachable: semaphore guarantees an idle slot exists.
        panic!("claim_idle_slot called with no idle slots; semaphore invariant violated");
    }
}

/// Hydrate `LkgInfo` from `target-pool/lkg/lkg.json`. Returns `None` if the
/// sidecar is missing, unreadable, malformed, or its companion exe doesn't
/// exist. Called once during `BuildPool::new`. A missing LKG is benign — it
/// just means the supervisor was started before any successful build, or the
/// dir was wiped. Build success will rewrite both files.
fn load_lkg_from_disk(config: &SupervisorConfig) -> Option<LkgInfo> {
    let exe = config.lkg_exe_path();
    let meta = config.lkg_metadata_path();
    if !exe.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&meta).ok()?;
    let parsed: LkgInfo = serde_json::from_str(&raw).ok()?;
    Some(parsed)
}

pub struct AiState {
    pub provider: String,
    pub model: String,
    pub auto_debug_enabled: bool,
}

pub struct ExpoState {
    pub process: Option<Child>,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub port: u16,
}

/// Compute the supervisor build identifier from the embedded `dist/index.html`.
///
/// rust-embed captures the file's mtime at compile time, so this string changes
/// every time the supervisor's frontend (`npm run build`) + cargo build pair
/// re-runs. Format: RFC3339 UTC timestamp (`2026-04-25T12:34:56+00:00`). When
/// the file is missing or rust-embed couldn't capture an mtime on this
/// platform, fall back to fixed sentinel strings so callers can still
/// distinguish "no signal" from a real change.
pub fn compute_build_id() -> String {
    use crate::routes::dashboard::Assets;
    match Assets::get("index.html") {
        Some(file) => match file.metadata.last_modified() {
            Some(secs) => {
                // rust-embed-utils returns seconds since UNIX epoch.
                match chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0) {
                    Some(dt) => dt.to_rfc3339(),
                    None => "embed-error".to_string(),
                }
            }
            None => "embed-no-mtime".to_string(),
        },
        None => "unknown".to_string(),
    }
}

/// Env var name that admits the supervisor's debug-only HTTP endpoints
/// (currently just `POST /control/dev/emit-build-id`). Set to `1` to enable;
/// any other value (or unset) keeps the endpoints returning 403. The env is
/// read once at startup and cached on
/// [`SupervisorState::debug_endpoints_enabled`] so per-request env reads
/// stay off the hot path.
pub const DEBUG_ENDPOINTS_ENV: &str = "QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS";

/// Read the debug-endpoints gate from the environment. Returns true only when
/// `DEBUG_ENDPOINTS_ENV` is exactly `"1"`. Empty / unset / `"0"` / anything
/// else is treated as disabled. We intentionally do not accept `"true"` /
/// `"yes"` to keep the activation surface as narrow and unambiguous as
/// possible — this gate guards endpoints that bypass production safety
/// checks, so the on-state must be deliberate.
pub fn read_debug_endpoints_env() -> bool {
    std::env::var(DEBUG_ENDPOINTS_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Default platform-appropriate path for the persisted `boot.id` file.
///
/// - Windows: `%LOCALAPPDATA%\qontinui-supervisor\boot.id`
/// - Linux:   `~/.local/share/qontinui-supervisor/boot.id`
/// - macOS:   `~/Library/Application Support/qontinui-supervisor/boot.id`
///
/// Falls back to `./qontinui-supervisor/boot.id` if `dirs::data_local_dir()`
/// can't determine a home/data dir (extremely rare — e.g. a stripped-down
/// container with no `HOME` set). The fallback keeps the supervisor running
/// rather than panicking; persistence may simply not survive a CWD change.
fn default_boot_id_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qontinui-supervisor")
        .join("boot.id")
}

/// Load a previously-persisted `boot_id` from `path`, or generate a fresh
/// UUID v4 and persist it there. Failure to write the new UUID is non-fatal —
/// the in-memory UUID is returned and a warning is logged; the next startup
/// will simply generate a new one. The returned string is always a
/// well-formed UUID.
///
/// This is the testable variant of [`load_or_create_boot_id`]. The public
/// function calls this with [`default_boot_id_path()`].
pub fn load_or_create_boot_id_at(path: &std::path::Path) -> String {
    if let Ok(contents) = std::fs::read_to_string(path) {
        let trimmed = contents.trim();
        if uuid::Uuid::parse_str(trimmed).is_ok() {
            return trimmed.to_string();
        }
        tracing::warn!(
            path = %path.display(),
            "boot.id file exists but does not contain a valid UUID; regenerating"
        );
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                path = %parent.display(),
                error = %e,
                "failed to create boot.id parent directory; boot_id will not persist"
            );
            return fresh;
        }
    }
    if let Err(e) = std::fs::write(path, &fresh) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to write boot.id; boot_id will not persist across restarts"
        );
    }
    fresh
}

/// Resolve the persistent `boot_id` for this supervisor installation.
/// Reads the persisted UUID from the platform-default path if present,
/// otherwise generates and persists a fresh one. See
/// [`SupervisorState::boot_id`] for semantics.
pub fn load_or_create_boot_id() -> String {
    load_or_create_boot_id_at(&default_boot_id_path())
}

/// Identity of a spawn-test build for single-flight purposes: **who** asked,
/// and **what tree** they asked to have compiled.
///
/// Both halves are load-bearing:
///
/// * `requester_id` — two different agents that happen to want the same tree
///   each want their OWN runner, so they must not collapse. A request with no
///   `requester_id` produces no key at all (see
///   `routes::runners::spawn_dedup_key`): anonymous requests are
///   indistinguishable from one another, and joining them would hand one
///   caller a runner another caller owns.
/// * `build_target` — derived from the resolved `SpawnBuildSource` (plus the
///   `frontend_only` flag, which changes what is compiled from the same tree),
///   so `origin/main`, an explicit ref, a caller's worktree path and the live
///   tree are four distinct targets that never join each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpawnDedupKey {
    pub requester_id: String,
    pub build_target: String,
}

/// The in-flight spawn-test build that a duplicate request joins: the same
/// `build_id` (= submission id) and the same reserved runner.
#[derive(Debug, Clone)]
pub struct SpawnInflight {
    /// `build_id` — the build-submission id, identical on the sync, async and
    /// poll paths.
    pub submission_id: uuid::Uuid,
    /// Registry id of the runner reserved for this build.
    pub runner_id: String,
    /// Port reserved for that runner.
    pub port: u16,
}

/// Result of asking for the spawn-test single flight for one key.
pub enum SpawnTicket<'a> {
    /// An equivalent build is already running: join it. Nothing was claimed.
    Join(SpawnInflight),
    /// This request owns the key. It MUST call [`SpawnClaimGuard::commit`] once
    /// its build is registered, or drop the guard to abandon the claim.
    Claim(SpawnClaimGuard<'a>),
}

/// Exclusive hold on one [`SpawnDedupKey`] while its build is being registered.
///
/// The index's write lock is held for the guard's whole life, so a concurrent
/// same-key request blocks here and then observes the committed entry rather
/// than starting a second build. Dropping the guard without committing (the
/// handler bailed out on a validation error, no port was free, …) leaves the
/// key free for the next request — an abandoned claim never wedges a key.
///
/// An unkeyed request (anonymous, or one that claims no build slot) gets a
/// guard with `key: None`: it still passes through the same code path, and
/// `commit` records nothing.
pub struct SpawnClaimGuard<'a> {
    key: Option<SpawnDedupKey>,
    index: tokio::sync::RwLockWriteGuard<'a, HashMap<SpawnDedupKey, SpawnInflight>>,
}

impl SpawnClaimGuard<'_> {
    /// Record the build this claim started, so duplicates can join it.
    pub fn commit(mut self, inflight: SpawnInflight) {
        if let Some(key) = self.key.take() {
            self.index.insert(key, inflight);
        }
    }
}

impl SupervisorState {
    /// Enter the `(requester, build target)` single flight for a spawn-test
    /// request: either join the build already running for that key, or take an
    /// exclusive claim on it.
    ///
    /// `key: None` (anonymous request, or one that claims no build-pool slot)
    /// always yields a claim that records nothing — such requests never join
    /// and are never joined.
    ///
    /// This serializes spawn-test *admission*, not the builds themselves (which
    /// run detached on the build pool). The handler already serializes on the
    /// runner-registry write lock immediately afterwards, so no new class of
    /// contention is introduced.
    pub async fn claim_spawn_build(&self, key: Option<SpawnDedupKey>) -> SpawnTicket<'_> {
        let mut index = self.spawn_test_inflight.write().await;
        if let Some(key) = &key {
            if let Some(existing) = index.get(key).cloned() {
                if self.spawn_build_in_flight(existing.submission_id).await {
                    return SpawnTicket::Join(existing);
                }
                // Terminal but never released — only reachable if a release was
                // lost. Drop it rather than refusing this key forever.
                index.remove(key);
            }
        }
        SpawnTicket::Claim(SpawnClaimGuard { key, index })
    }

    /// Is this build submission still running?
    ///
    /// An id the store does not know is **UNKNOWN, not finished**:
    /// `build_submissions::submit_spawn` registers the submission from a
    /// spawned task, so a just-started build is briefly absent from the store.
    /// Reading that absence as "terminal" would let a retry arriving in exactly
    /// that window start the duplicate build this index exists to prevent.
    /// Entries are removed on completion, so a genuinely finished build has no
    /// index entry to consult in the first place.
    async fn spawn_build_in_flight(&self, submission_id: uuid::Uuid) -> bool {
        match self.build_submissions.get(&submission_id).await {
            Some(arc) => !arc.read().await.status.is_terminal(),
            None => true,
        }
    }

    /// Snapshot the spawn-test single-flight index, optionally narrowed to one
    /// requester.
    ///
    /// **Why this is public.** The index already holds exactly the triple a
    /// caller needs to recover from a lost `POST /runners/spawn-test` answer —
    /// `(submission_id, runner_id, port)` — keyed by the requester. A caller
    /// whose connection died before the response was written (client timeout,
    /// supervisor restart) otherwise has no way to learn the runner id it just
    /// created, and without the id it cannot reach `/runners/{id}/logs` or
    /// `/runners/{id}/stop`: it created a runner it cannot follow or clean up.
    /// Polling `GET /runners` and pattern-matching on `requester_id` is the
    /// workaround that recovery previously required, and it is a guess — it
    /// cannot distinguish this request's runner from that requester's previous
    /// one. This is the exact answer.
    ///
    /// Returns entries sorted by key so the response is stable across calls.
    pub async fn spawn_inflight_snapshot(
        &self,
        requester_id: Option<&str>,
    ) -> Vec<(SpawnDedupKey, SpawnInflight)> {
        let index = self.spawn_test_inflight.read().await;
        let mut out: Vec<(SpawnDedupKey, SpawnInflight)> = index
            .iter()
            .filter(|(k, _)| requester_id.is_none_or(|r| k.requester_id == r))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| {
            (&a.0.requester_id, &a.0.build_target).cmp(&(&b.0.requester_id, &b.0.build_target))
        });
        out
    }

    /// Drop a spawn-test single-flight entry once its build is terminal, so the
    /// next request for the same key starts a fresh build.
    ///
    /// Removes only an entry that still points at `submission_id`, so a build
    /// finishing after a newer one claimed the key cannot evict its successor.
    pub async fn release_spawn_inflight(&self, key: &SpawnDedupKey, submission_id: uuid::Uuid) {
        let mut index = self.spawn_test_inflight.write().await;
        if index
            .get(key)
            .is_some_and(|e| e.submission_id == submission_id)
        {
            index.remove(key);
        }
    }

    pub fn new(config: SupervisorConfig) -> Self {
        let watchdog_enabled = config.watchdog_enabled_at_start;
        let auto_debug = config.auto_debug;
        let expo_port = config.expo_port;
        let (health_tx, _) = broadcast::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        // Capacity 8 with `broadcast` semantics: lagging receivers see Lagged,
        // not Closed, and we ignore Lagged in the SSE consumer (it just means
        // we missed an injection — fine for a debug-only "kick the watcher"
        // signal).
        let (synthetic_build_id_tx, _) = broadcast::channel::<String>(8);
        let debug_endpoints_enabled = read_debug_endpoints_env();
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        // Build multi-runner map from config. Thread the optional log dir
        // through so each ManagedRunner's LogState gets a per-runner append
        // file at <log_dir>/<runner_id>.log.
        //
        // Crash-only watchdog default scope: under `--watchdog` only the
        // PRIMARY is armed for crash auto-restart. Named/temp/external
        // runners default off (cheap to respawn, often killed deliberately
        // by agents) — arm one explicitly via `POST /runners/{id}/watchdog`.
        let log_dir = config.log_dir.as_deref();
        let mut runners_map = HashMap::new();
        for rc in &config.runners {
            let managed = Arc::new(ManagedRunner::new_with_log_dir(
                rc.clone(),
                watchdog_enabled && rc.kind().is_primary(),
                log_dir,
            ));
            runners_map.insert(rc.id.clone(), managed);
        }

        let build_pool = BuildPool::new(&config);

        // Create the kill-on-exit JobObject. On Windows this enforces that
        // every supervisor-spawned *temp* runner dies when the supervisor
        // exits (graceful, panic, force-kill); user-owned runners (primary,
        // named, external) are never assigned to it and survive. On
        // non-Windows it's a no-op stub. Failure to create is logged loudly
        // but never aborts startup — the supervisor still functions, just
        // without the safety net.
        //
        // The `state.logs` collector isn't constructed yet at this point
        // (it's initialized inside the `Self { ... }` block below), so we
        // can't directly emit to the dashboard log stream. Instead, we
        // capture the success/failure message into `pending_startup_logs`,
        // which `flush_pending_startup_logs` drains right after the state
        // is wrapped in an `Arc` (see `main.rs`).
        let mut startup_logs: Vec<(LogLevel, String)> = Vec::new();
        let ephemeral_job = match RunnerJob::create() {
            Ok(j) => {
                let msg = "Created kill-on-exit JobObject for spawned temp runners \
                           (KILL_ON_JOB_CLOSE; user-owned runners are never assigned)"
                    .to_string();
                tracing::info!("{}", msg);
                startup_logs.push((LogLevel::Info, msg));
                Some(Arc::new(j))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to create ephemeral-runner JobObject — supervisor exit will NOT \
                     terminate spawned temp runners; orphans may linger and lock build slots"
                );
                startup_logs.push((
                    LogLevel::Warn,
                    format!(
                        "Failed to create ephemeral-runner JobObject: {} — supervisor exit will \
                         NOT terminate spawned temp runners",
                        e
                    ),
                ));
                None
            }
        };

        Self {
            config,
            runners: RwLock::new(runners_map),
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            build: RwLock::new(BuildState::new()),
            build_pool,
            build_submissions: Arc::new(BuildSubmissionStore::new(1000)),
            bazel_remote: crate::bazel_remote::BazelRemoteClient::from_env(),
            cache_telemetry: crate::cache_telemetry::CacheTelemetry::new(),
            ai: RwLock::new(AiState::new(auto_debug)),
            expo: RwLock::new(ExpoState::new(expo_port)),
            diagnostics: RwLock::new(DiagnosticsState::new()),
            dev_actions: RwLock::new(crate::dev_action::ActionStore::new()),
            evaluation: RwLock::new(EvaluationState::new()),
            velocity_tests: RwLock::new(VelocityTestState::new()),
            velocity_improvement: RwLock::new(VelocityImprovementState::new()),
            command_relay: CommandRelay::new(),
            logs: LogState::new(),
            health_tx,
            shutdown_tx,
            shutdown_latched: AtomicBool::new(false),
            cached_health: RwLock::new(CachedPortHealth::default()),
            cached_runner_health: RwLock::new(Vec::new()),
            health_cache_notify: Notify::new(),
            http_client,
            test_auto_login: RwLock::new(None),
            stopped_runners: Arc::new(RwLock::new(HashMap::new())),
            boot_id: load_or_create_boot_id(),
            build_id: compute_build_id(),
            ephemeral_job,
            pending_startup_logs: std::sync::Mutex::new(startup_logs),
            active_sse_connections: Arc::new(AtomicUsize::new(0)),
            debug_endpoints_enabled,
            supervisor_started_at: std::time::SystemTime::now(),
            synthetic_build_id_tx,
            ci_runner_state: RwLock::new(CiRunnerState::default()),
            fix_and_rebuild_inflight: RwLock::new(None),
            spawn_test_inflight: RwLock::new(HashMap::new()),
            active_spawn_worktrees: std::sync::Mutex::new(std::collections::HashSet::new()),
            spawn_container_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
            footprint: RwLock::new(None),
            origin_drift: RwLock::new(None),
        }
    }

    /// Recompute the build-artifact footprint snapshot and store it on
    /// `self.footprint`. The walk is CPU/IO-bound and slow on real trees, so it
    /// runs inside `spawn_blocking`; the only async work is taking the write
    /// lock to publish the result. Best-effort: returns the fresh snapshot to
    /// the caller as well (so the on-demand `?refresh_footprint=1` path can
    /// serialize it immediately without a re-read).
    pub async fn refresh_footprint(
        self: &std::sync::Arc<Self>,
    ) -> crate::footprint::FootprintSnapshot {
        let this = self.clone();
        let snapshot =
            tokio::task::spawn_blocking(move || crate::footprint::compute_snapshot(&this.config))
                .await
                .unwrap_or_else(|_| crate::footprint::compute_snapshot(&self.config));
        *self.footprint.write().await = Some(snapshot.clone());
        snapshot
    }

    /// Recompute the origin/main drift snapshot and store it on
    /// `self.origin_drift`. Runs `git fetch` (bounded by
    /// `QONTINUI_SUPERVISOR_GIT_FETCH_TIMEOUT_SECS`), so it belongs on the
    /// timer, never on a request path -- see the field's doc comment.
    ///
    /// Returns `None` when there is nothing to compute (no LKG sha recorded, or
    /// `project_dir` has no parent), leaving any previous snapshot in place: a
    /// transiently missing LKG should not erase a good reading.
    pub async fn refresh_origin_drift(&self) -> Option<OriginDriftSnapshot> {
        let lkg_sha: Option<String> = self
            .build_pool
            .last_known_good
            .read()
            .await
            .as_ref()
            .and_then(|info| info.sha.clone());
        let repo_root = self.config.project_dir.parent()?.to_path_buf();
        let sha = lkg_sha?;

        let drift = crate::git_provenance::origin_main_drift(&repo_root, &sha).await;
        let snapshot = OriginDriftSnapshot {
            built_sha: sha,
            drift,
            computed_at: chrono::Utc::now(),
        };
        *self.origin_drift.write().await = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Drain `pending_startup_logs` into `state.logs`.
    ///
    /// Called once from `main.rs` right after `SupervisorState::new` wraps
    /// the state in an `Arc`. Messages captured during synchronous
    /// construction (currently just JobObject create success/failure) are
    /// routed through the same `state.logs.emit` path that the rest of the
    /// supervisor uses, so they appear in `/logs/history`, the SSE stream,
    /// and any persistent log file.
    pub async fn flush_pending_startup_logs(&self) {
        let drained: Vec<(LogLevel, String)> = {
            let mut guard = match self.pending_startup_logs.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *guard)
        };
        for (level, msg) in drained {
            self.logs.emit(LogSource::Supervisor, level, msg).await;
        }
    }

    pub fn notify_health_change(&self) {
        let _ = self.health_tx.send(());
    }

    /// Get (creating on first use) the mutual-exclusion lock for a spawn
    /// container. See [`SupervisorState::spawn_container_locks`] for why this
    /// exists — in short, `prepare_worktree` hard-resets a reused container and
    /// must never do so while a build is reading it.
    ///
    /// Returns an `Arc` so the caller can `lock_owned()` and hold the guard
    /// across the whole materialize+build sequence. The map mutex is released
    /// before the caller ever awaits, so this never blocks the runtime.
    pub fn spawn_container_lock(&self, container: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .spawn_container_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry(container.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Future that completes when a shutdown is signaled on `shutdown_tx`.
    ///
    /// Long-lived streams (SSE handlers, polling loops) call this to learn
    /// that the supervisor is exiting so they can terminate promptly. This
    /// is what unblocks `axum::serve(..).with_graceful_shutdown(..)`'s
    /// drain phase: until every in-flight response future resolves, axum
    /// keeps the listener alive and `serve_future.await` does not return.
    /// Without this hook, SSE handlers (`/health/stream`, `/logs/stream`,
    /// `/supervisor-bridge/commands/stream`, etc.) hold their connections
    /// open forever and the supervisor process lingers for 30+ seconds
    /// after `POST /supervisor/shutdown` fires.
    ///
    /// Resolution order:
    /// 1. **Latched check** — if `shutdown_latched` is already `true`,
    ///    return immediately. This handles handlers that subscribed AFTER
    ///    the broadcast already fired (broadcast channels don't replay).
    /// 2. **Subscribe + recv** — otherwise, subscribe a fresh receiver and
    ///    await. `recv()` returning either `Ok(())` (broadcast) or
    ///    `Err(_)` (sender dropped) both indicate "shut down now"; we
    ///    treat them identically.
    pub async fn shutdown_signal(&self) {
        // Subscribe BEFORE the latch check so we don't lose a broadcast
        // that fires between the check and the recv.
        let mut rx = self.shutdown_tx.subscribe();
        if self
            .shutdown_latched
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let _ = rx.recv().await;
    }

    /// Mark the supervisor as shutting down and broadcast to all subscribers.
    ///
    /// Sets the latched flag *before* the broadcast so handlers that race in
    /// (subscribing between the broadcast and observing it) still see the
    /// latch on their next poll. Idempotent — repeated calls are cheap and
    /// safe; the broadcast just goes to whoever subscribed since last time.
    pub fn signal_shutdown(&self) {
        self.shutdown_latched
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.shutdown_tx.send(());
    }

    /// Get a managed runner by ID.
    pub async fn get_runner(&self, id: &str) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.get(id).cloned()
    }

    /// Get the primary runner.
    pub async fn get_primary(&self) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners
            .values()
            .find(|r| r.config.kind().is_primary())
            .cloned()
    }

    /// Get all runners as a Vec.
    pub async fn get_all_runners(&self) -> Vec<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.values().cloned().collect()
    }
}

impl Default for RunnerState {
    fn default() -> Self {
        Self::new()
    }
}

impl RunnerState {
    pub fn new() -> Self {
        Self {
            process: None,
            running: false,
            started_at: None,
            restart_requested: false,
            stop_requested: false,
            pid: None,
            last_seen_responding_at: None,
        }
    }
}

impl WatchdogState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            restart_attempts: 0,
            last_restart_at: None,
            crash_history: Vec::new(),
            disabled_reason: None,
        }
    }
}

impl Default for BuildState {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildState {
    pub fn new() -> Self {
        Self {
            build_in_progress: false,
            build_error_detected: false,
            last_build_error: None,
            last_build_at: None,
            last_build_stderr: None,
        }
    }
}

impl AiState {
    pub fn new(auto_debug_enabled: bool) -> Self {
        Self {
            provider: "claude".to_string(),
            model: "opus".to_string(),
            auto_debug_enabled,
        }
    }
}

impl ExpoState {
    pub fn new(port: u16) -> Self {
        Self {
            process: None,
            running: false,
            pid: None,
            started_at: None,
            port,
        }
    }
}

pub struct EvaluationState {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub continuous_mode: bool,
    pub continuous_interval_secs: u64,
    pub current_prompt_index: usize,
    pub total_prompts: usize,
    pub stop_tx: Option<watch::Sender<bool>>,
}

impl EvaluationState {
    pub fn new() -> Self {
        Self {
            running: false,
            current_run_id: None,
            continuous_mode: false,
            continuous_interval_secs: 3600,
            current_prompt_index: 0,
            total_prompts: 0,
            stop_tx: None,
        }
    }
}

impl Default for EvaluationState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct VelocityTestState {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub current_test_index: usize,
    pub total_tests: usize,
    pub stop_tx: Option<watch::Sender<bool>>,
}

impl VelocityTestState {
    pub fn new() -> Self {
        Self {
            running: false,
            current_run_id: None,
            current_test_index: 0,
            total_tests: 0,
            stop_tx: None,
        }
    }
}

impl Default for VelocityTestState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RunnerConfig, SupervisorConfig, DEFAULT_SUPERVISOR_PORT, EXPO_PORT};
    use std::path::PathBuf;

    fn make_test_config() -> SupervisorConfig {
        SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: DEFAULT_SUPERVISOR_PORT,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: EXPO_PORT,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: crate::config::BuildPoolConfig { pool_size: 1 },
            no_prewarm: false,
            no_webview: true,
        }
    }

    // --- RunnerState tests ---

    #[test]
    fn test_runner_state_new_defaults() {
        let state = RunnerState::new();
        assert!(!state.running);
        assert!(state.process.is_none());
        assert!(state.started_at.is_none());
        assert!(!state.restart_requested);
        assert!(!state.stop_requested);
        assert!(state.pid.is_none());
    }

    #[test]
    fn test_runner_state_default_matches_new() {
        let from_new = RunnerState::new();
        let from_default = RunnerState::default();
        assert_eq!(from_new.running, from_default.running);
        assert_eq!(from_new.pid, from_default.pid);
        assert_eq!(from_new.restart_requested, from_default.restart_requested);
        assert_eq!(from_new.stop_requested, from_default.stop_requested);
    }

    // --- WatchdogState tests ---

    #[test]
    fn test_watchdog_state_new_enabled() {
        let state = WatchdogState::new(true);
        assert!(state.enabled);
        assert_eq!(state.restart_attempts, 0);
        assert!(state.last_restart_at.is_none());
        assert!(state.crash_history.is_empty());
        assert!(state.disabled_reason.is_none());
    }

    #[test]
    fn test_watchdog_state_new_disabled() {
        let state = WatchdogState::new(false);
        assert!(!state.enabled);
    }

    // --- BuildState tests ---

    #[test]
    fn test_build_state_new_defaults() {
        let state = BuildState::new();
        assert!(!state.build_in_progress);
        assert!(!state.build_error_detected);
        assert!(state.last_build_error.is_none());
        assert!(state.last_build_at.is_none());
    }

    #[test]
    fn test_build_state_default_matches_new() {
        let from_new = BuildState::new();
        let from_default = BuildState::default();
        assert_eq!(from_new.build_in_progress, from_default.build_in_progress);
        assert_eq!(
            from_new.build_error_detected,
            from_default.build_error_detected
        );
    }

    // --- AiState tests ---

    #[test]
    fn test_ai_state_new_with_auto_debug_enabled() {
        let state = AiState::new(true);
        assert_eq!(state.provider, "claude");
        assert_eq!(state.model, "opus");
        assert!(state.auto_debug_enabled);
    }

    #[test]
    fn test_ai_state_new_with_auto_debug_disabled() {
        let state = AiState::new(false);
        assert!(!state.auto_debug_enabled);
    }

    // --- ExpoState tests ---

    #[test]
    fn test_expo_state_new() {
        let state = ExpoState::new(8081);
        assert!(!state.running);
        assert!(state.process.is_none());
        assert!(state.pid.is_none());
        assert!(state.started_at.is_none());
        assert_eq!(state.port, 8081);
    }

    #[test]
    fn test_expo_state_custom_port() {
        let state = ExpoState::new(3000);
        assert_eq!(state.port, 3000);
    }

    // --- EvaluationState tests ---

    #[test]
    fn test_evaluation_state_new_defaults() {
        let state = EvaluationState::new();
        assert!(!state.running);
        assert!(state.current_run_id.is_none());
        assert!(!state.continuous_mode);
        assert_eq!(state.continuous_interval_secs, 3600);
        assert_eq!(state.current_prompt_index, 0);
        assert_eq!(state.total_prompts, 0);
        assert!(state.stop_tx.is_none());
    }

    #[test]
    fn test_evaluation_state_default_matches_new() {
        let from_new = EvaluationState::new();
        let from_default = EvaluationState::default();
        assert_eq!(from_new.running, from_default.running);
        assert_eq!(
            from_new.continuous_interval_secs,
            from_default.continuous_interval_secs
        );
    }

    // --- VelocityTestState tests ---

    #[test]
    fn test_velocity_test_state_new_defaults() {
        let state = VelocityTestState::new();
        assert!(!state.running);
        assert!(state.current_run_id.is_none());
        assert_eq!(state.current_test_index, 0);
        assert_eq!(state.total_tests, 0);
        assert!(state.stop_tx.is_none());
    }

    // --- SupervisorState tests ---

    #[tokio::test]
    async fn test_supervisor_state_construction() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        assert_eq!(state.config.port, DEFAULT_SUPERVISOR_PORT);
    }

    #[tokio::test]
    async fn test_supervisor_state_runner_initial_state() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let runner = state.runner.try_read().unwrap();
        assert!(!runner.running);
        assert!(runner.pid.is_none());
    }

    #[tokio::test]
    async fn test_supervisor_state_watchdog_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(!watchdog.enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_watchdog_enabled_from_config() {
        let mut config = make_test_config();
        config.watchdog_enabled_at_start = true;
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(watchdog.enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_auto_debug_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(!ai.auto_debug_enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_auto_debug_enabled_from_config() {
        let mut config = make_test_config();
        config.auto_debug = true;
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(ai.auto_debug_enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_cached_health_defaults_to_all_false() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let cached = state.cached_health.try_read().unwrap();
        assert!(!cached.runner_port_open);
        assert!(!cached.runner_responding);
    }

    #[tokio::test]
    async fn test_supervisor_state_build_not_in_progress() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let build = state.build.try_read().unwrap();
        assert!(!build.build_in_progress);
        assert!(!build.build_error_detected);
    }

    #[tokio::test]
    async fn test_supervisor_state_notify_health_change_does_not_panic() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        // Should not panic even with no subscribers
        state.notify_health_change();
    }

    #[tokio::test]
    async fn test_shutdown_signal_unblocks_live_subscriber() {
        // Regression test for the /supervisor/shutdown hang fix — live path.
        //
        // Long-lived SSE handlers race their work against
        // `state.shutdown_signal()`; if this future doesn't resolve when
        // shutdown is signaled, axum's graceful drain never completes and
        // the supervisor process lingers.
        let config = make_test_config();
        let state = Arc::new(SupervisorState::new(config));

        // Subscribe BEFORE the signal so the broadcast catches the
        // pre-existing receiver — this is the steady-state production path
        // (handlers connect, then shutdown fires later).
        let signal_state = state.clone();
        let signal = tokio::spawn(async move { signal_state.shutdown_signal().await });
        // Yield briefly so the spawned task reaches `rx.recv().await`
        // before we send. Without this, the test passes for the wrong
        // reason (it would hit the latched-fast-path instead).
        tokio::task::yield_now().await;

        // Fires the same path that `routes/runner.rs::supervisor_shutdown` uses.
        state.signal_shutdown();

        // Generous timeout: anything more than a few millis is a regression.
        tokio::time::timeout(std::time::Duration::from_secs(1), signal)
            .await
            .expect("shutdown_signal must resolve once signal_shutdown fires")
            .expect("signal task must not panic");
    }

    #[tokio::test]
    async fn test_shutdown_signal_late_subscriber_sees_latch() {
        // The other half of the regression: handlers that race in *after*
        // shutdown was already signaled. Broadcast channels do not replay
        // missed messages, so without the latched bool, a late subscriber
        // would block forever — wedging axum's graceful drain just as
        // surely as a never-terminating SSE stream.
        let config = make_test_config();
        let state = Arc::new(SupervisorState::new(config));

        // Signal first, subscribe second.
        state.signal_shutdown();

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), state.shutdown_signal()).await;
        assert!(
            result.is_ok(),
            "late shutdown_signal subscriber must see the latched flag"
        );
    }

    // --- SlotHistory tests ---

    #[test]
    fn test_slot_history_new_empty() {
        let h = SlotHistory::new();
        assert_eq!(h.total_builds, 0);
        assert!(h.avg_duration_secs().is_none());
        assert!(h.p50_duration_secs().is_none());
    }

    #[test]
    fn test_slot_history_record_and_avg() {
        let mut h = SlotHistory::new();
        h.record(10.0, true, None, None);
        h.record(20.0, true, None, None);
        h.record(
            30.0,
            false,
            Some("boom".into()),
            Some("stderr detail".into()),
        );
        assert_eq!(h.total_builds, 3);
        assert_eq!(h.successful_builds, 2);
        assert!((h.avg_duration_secs().unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(h.last_error.as_deref(), Some("boom"));
        assert_eq!(h.last_error_detail.as_deref(), Some("stderr detail"));
        // Short error log mirrors the detail when below the cap.
        assert_eq!(h.last_error_log.as_deref(), Some("stderr detail"));
    }

    #[test]
    fn test_slot_history_success_clears_last_error_log() {
        let mut h = SlotHistory::new();
        h.record(1.0, false, Some("err".into()), Some("boom".into()));
        assert_eq!(h.last_error_log.as_deref(), Some("boom"));
        // A subsequent green build supersedes the failure for inline display.
        h.record(2.0, true, None, None);
        assert!(h.last_error_log.is_none());
        // The longer detail / error string can stay (forensic history).
        // We only enforce the inline summary clears.
    }

    #[test]
    /// `last_error_log` keeps the HEAD of the detail, not the tail.
    ///
    /// This assertion was inverted on 2026-07-31 along with the behaviour.
    /// `error_detail` is no longer raw cargo stderr — it is a rendered
    /// diagnostic (`build_diagnostics::render_capped_detail`) that hoists the
    /// cause to the FRONT precisely so a size cap cannot drop it. Keeping the
    /// tail of that document hands `GET /builds` an excerpt of linker flags,
    /// which is the exact defect the rendering was introduced to fix.
    fn test_slot_history_last_error_log_keeps_the_head_within_1k() {
        let mut h = SlotHistory::new();
        let big = "Z".repeat(LAST_ERROR_LOG_MAX_BYTES * 4);
        let detail = format!("HEAD_LOG_START{}TAIL_LOG_END", big);
        h.record(1.0, false, Some("err".into()), Some(detail));
        let stored = h.last_error_log.as_ref().expect("log recorded");
        assert!(
            stored.starts_with("HEAD_LOG_START"),
            "the head — where the rendered cause lives — must survive: {stored:.60}"
        );
        assert!(
            !stored.contains("TAIL_LOG_END"),
            "a 4x-oversized detail must actually be cut"
        );
        // The head helper appends a short truncation marker; allow for it.
        assert!(
            stored.len() <= LAST_ERROR_LOG_MAX_BYTES + 32,
            "log too large: {}",
            stored.len()
        );
    }

    #[test]
    fn test_tail_bytes_keep_utf8_short_passthrough() {
        let s = "short";
        assert_eq!(tail_bytes_keep_utf8(s, 1024), "short");
    }

    #[test]
    fn test_tail_bytes_keep_utf8_long_keeps_tail_on_boundary() {
        // Build a multi-byte string and ensure the result is valid UTF-8.
        let s: String = "ééééééééééééé".repeat(200); // 'é' is 2 bytes in UTF-8
        let out = tail_bytes_keep_utf8(&s, 50);
        assert!(out.len() <= 50 + 1); // up to 1 byte of slack to land on boundary
        assert!(s.ends_with(&out));
    }

    #[test]
    fn test_head_bytes_keep_utf8_short_passthrough() {
        assert_eq!(head_bytes_keep_utf8("short", 1024), "short");
    }

    /// The head counterpart must be UTF-8 safe at the cut and must mark that
    /// it cut — an unmarked crop invites the reader to treat a truncated
    /// document as complete.
    #[test]
    fn test_head_bytes_keep_utf8_long_keeps_head_on_boundary() {
        let s: String = "ééééééééééééé".repeat(200); // 'é' is 2 bytes in UTF-8
        let out = head_bytes_keep_utf8(&s, 51); // odd cap lands mid-character
        let body = out.strip_suffix("\n[...truncated]").expect("cut is marked");
        assert!(body.len() <= 51);
        assert!(s.starts_with(body), "must be a prefix of the input");
        assert!(body.chars().all(|c| c == 'é'), "must stay valid UTF-8");
    }

    #[test]
    fn test_slot_history_window_evicts() {
        let mut h = SlotHistory::new();
        for i in 0..(RECENT_BUILD_SAMPLE_COUNT + 3) {
            h.record(i as f64, true, None, None);
        }
        assert_eq!(h.recent_durations_secs.len(), RECENT_BUILD_SAMPLE_COUNT);
        assert_eq!(h.recent_durations_secs.front().copied(), Some(3.0));
    }

    #[test]
    fn test_slot_history_p50() {
        let mut h = SlotHistory::new();
        h.record(5.0, true, None, None);
        h.record(1.0, true, None, None);
        h.record(9.0, true, None, None);
        assert_eq!(h.p50_duration_secs(), Some(5.0));
    }

    #[test]
    fn test_truncate_error_detail_keep_tail_short_passthrough() {
        let s = "short string".to_string();
        let out = truncate_error_detail_keep_tail(s.clone());
        assert_eq!(out, s);
    }

    #[test]
    fn test_truncate_error_detail_keep_tail_truncates_front() {
        // Build a string substantially larger than the cap.
        let big = "X".repeat(LAST_ERROR_DETAIL_MAX_BYTES * 2);
        // Make the tail uniquely identifiable so we can confirm it survived.
        let s = format!("{}TAIL_MARKER_END", big);
        let out = truncate_error_detail_keep_tail(s.clone());
        // The tail must be preserved verbatim.
        assert!(
            out.ends_with("TAIL_MARKER_END"),
            "tail not preserved; got: {:?}",
            &out[out.len().saturating_sub(40)..]
        );
        // The result must be near the cap (cap + small marker).
        assert!(
            out.len() <= LAST_ERROR_DETAIL_MAX_BYTES + 64,
            "result too large: {}",
            out.len()
        );
        // And a truncation marker must appear at the start so consumers know.
        assert!(out.starts_with("[...truncated]"));
    }

    #[test]
    fn test_slot_history_record_truncates_long_detail() {
        let mut h = SlotHistory::new();
        let big = "Y".repeat(LAST_ERROR_DETAIL_MAX_BYTES + 1024);
        let detail = format!("{}END_OF_STDERR", big);
        h.record(1.0, false, Some("err".into()), Some(detail));
        let stored = h.last_error_detail.as_ref().expect("detail recorded");
        assert!(stored.ends_with("END_OF_STDERR"));
        assert!(stored.len() <= LAST_ERROR_DETAIL_MAX_BYTES + 64);
    }

    #[test]
    fn test_slot_history_success_clears_no_detail() {
        let mut h = SlotHistory::new();
        h.record(1.0, true, None, Some("should be ignored".into()));
        assert!(h.last_error_detail.is_none());
    }

    #[test]
    fn test_sse_connection_guard_increments_and_decrements() {
        // Construction must increment, drop must decrement back to zero.
        // The /health endpoint reads this via Ordering::Relaxed.
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
        {
            let _g = SseConnectionGuard::new(counter.clone());
            assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
            let _g2 = SseConnectionGuard::new(counter.clone());
            assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
        // Both guards dropped — counter must be back at zero.
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_supervisor_state_active_sse_connections_starts_at_zero() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        assert_eq!(
            state
                .active_sse_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "fresh supervisor must report zero active SSE connections"
        );
    }

    #[tokio::test]
    async fn test_supervisor_state_expo_port_from_config() {
        let mut config = make_test_config();
        config.expo_port = 9999;
        let state = SupervisorState::new(config);
        let expo = state.expo.try_read().unwrap();
        assert_eq!(expo.port, 9999);
    }

    // --- boot_id persistence tests ---

    #[test]
    fn test_load_or_create_boot_id_at_persists_across_calls() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("nested").join("boot.id");
        // First call: file does not exist, helper must generate + persist.
        let first = load_or_create_boot_id_at(&path);
        assert!(
            uuid::Uuid::parse_str(&first).is_ok(),
            "first call must return a valid UUID, got {first:?}"
        );
        assert!(path.exists(), "first call must create the boot.id file");
        // Second call: must read the persisted UUID, not generate a fresh one.
        let second = load_or_create_boot_id_at(&path);
        assert_eq!(
            first, second,
            "second call must return the same UUID as the first"
        );
    }

    #[test]
    fn test_load_or_create_boot_id_at_regenerates_on_invalid_contents() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("boot.id");
        std::fs::write(&path, "not-a-uuid").expect("seed invalid contents");
        let id = load_or_create_boot_id_at(&path);
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "invalid contents must be replaced with a valid UUID"
        );
        // The file should now contain the freshly-generated UUID.
        let persisted = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(persisted.trim(), id);
    }

    #[test]
    fn test_load_or_create_boot_id_at_tolerates_trailing_whitespace() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("boot.id");
        let seeded = uuid::Uuid::new_v4().to_string();
        std::fs::write(&path, format!("{seeded}\n")).expect("seed valid contents");
        let id = load_or_create_boot_id_at(&path);
        assert_eq!(id, seeded, "trailing whitespace must be trimmed");
    }

    // ---- Phase 3b: liveness must not conflate "quiet" with "gone" ----

    #[test]
    fn responding_runner_is_responding() {
        let mut st = RunnerState::new();
        st.running = true;
        assert_eq!(st.liveness(true), RunnerLiveness::Responding);
        // Even with the port probe somehow false, a positive API answer wins:
        // it is direct evidence the process is serving.
        assert_eq!(st.liveness(false), RunnerLiveness::Responding);
    }

    #[test]
    fn silent_api_with_a_held_port_is_unresponsive_not_stopped() {
        // This is the 2026-08-08 incident shape: PID alive, port held, API
        // silent. The old boolean reported this as "not running, pid null".
        let seen = Utc::now();
        let mut st = RunnerState::new();
        st.running = false;
        st.last_seen_responding_at = Some(seen);
        st.pid = Some(148320);

        assert_eq!(st.liveness(true), RunnerLiveness::UnresponsiveSince(seen));
        assert_ne!(st.liveness(true), RunnerLiveness::Stopped);
    }

    #[test]
    fn silent_api_with_a_closed_port_is_stopped() {
        let mut st = RunnerState::new();
        st.running = false;
        st.last_seen_responding_at = Some(Utc::now());
        assert_eq!(st.liveness(false), RunnerLiveness::Stopped);
    }

    #[test]
    fn never_seen_responding_is_unknown_not_stopped() {
        // No stamp means we cannot distinguish "never started" from
        // "stopped before we looked" — say UNKNOWN rather than guessing.
        let st = RunnerState::new();
        assert_eq!(st.liveness(false), RunnerLiveness::Unknown);
        assert_eq!(st.liveness(true), RunnerLiveness::Unknown);
    }
    // --- spawn-test single flight ---

    fn dedup_key(requester: &str, target: &str) -> SpawnDedupKey {
        SpawnDedupKey {
            requester_id: requester.to_string(),
            build_target: target.to_string(),
        }
    }

    fn spawn_state() -> Arc<SupervisorState> {
        Arc::new(SupervisorState::new(make_test_config()))
    }

    /// A submission the store will report as still running.
    fn running_submission(id: uuid::Uuid) -> crate::build_submissions::BuildSubmission {
        crate::build_submissions::BuildSubmission {
            id,
            worktree_path: PathBuf::from("/tmp/test/src-tauri"),
            source: None,
            build_kind: crate::build_submissions::BuildKind::Build,
            agent_id: None,
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: Utc::now(),
            status: crate::build_submissions::BuildStatus::Running {
                started_at: Utc::now(),
            },
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
            spawn: None,
            detached: None,
        }
    }

    fn finished_submission(id: uuid::Uuid) -> crate::build_submissions::BuildSubmission {
        let mut sub = running_submission(id);
        sub.status = crate::build_submissions::BuildStatus::Succeeded {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            duration_secs: 1.0,
        };
        sub
    }

    fn inflight(id: uuid::Uuid, port: u16) -> SpawnInflight {
        SpawnInflight {
            submission_id: id,
            runner_id: format!("test-{port}"),
            port,
        }
    }

    /// The property the whole index exists for: two rapid same-key spawn-test
    /// requests must produce ONE build, and both must see the same `build_id`.
    /// Raced through a barrier so the second genuinely contends for the claim.
    #[tokio::test]
    async fn two_concurrent_same_key_spawns_start_one_build_with_one_build_id() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let starts = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let state = state.clone();
            let key = key.clone();
            let starts = starts.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                match state.claim_spawn_build(Some(key)).await {
                    SpawnTicket::Join(existing) => existing.submission_id,
                    SpawnTicket::Claim(claim) => {
                        starts.fetch_add(1, Ordering::SeqCst);
                        let id = uuid::Uuid::new_v4();
                        // Stand in for `submit_spawn`: register a live build,
                        // then publish the claim (both under the guard, exactly
                        // as `spawn_test` does).
                        state.build_submissions.insert(running_submission(id)).await;
                        claim.commit(inflight(id, 9877));
                        id
                    }
                }
            }));
        }

        let mut ids = Vec::new();
        for h in handles {
            ids.push(h.await.expect("task"));
        }

        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "exactly one of two concurrent same-key spawns may claim a build slot"
        );
        assert_eq!(
            ids[0], ids[1],
            "the joining request must be handed the SAME build_id, not a fresh one"
        );
    }

    /// Different build targets are different builds: same requester, four
    /// distinct targets, four independent claims.
    #[tokio::test]
    async fn different_build_targets_never_join_each_other() {
        let state = spawn_state();
        let occupied = dedup_key("agent-1", "origin_main");
        let id = uuid::Uuid::new_v4();
        state.build_submissions.insert(running_submission(id)).await;
        match state.claim_spawn_build(Some(occupied.clone())).await {
            SpawnTicket::Claim(claim) => claim.commit(inflight(id, 9877)),
            SpawnTicket::Join(_) => panic!("first claim on an empty index must not join"),
        }

        for target in ["live_tree", "git_ref:feature/x", "worktree_path:D:/wt"] {
            match state
                .claim_spawn_build(Some(dedup_key("agent-1", target)))
                .await
            {
                SpawnTicket::Claim(_) => {}
                SpawnTicket::Join(_) => {
                    panic!("target {target} must not join a build of a different target")
                }
            }
        }

        // A different requester on the SAME target is also its own build — two
        // agents each want their own runner.
        match state
            .claim_spawn_build(Some(dedup_key("agent-2", "origin_main")))
            .await
        {
            SpawnTicket::Claim(_) => {}
            SpawnTicket::Join(_) => panic!("a different requester must never join another's build"),
        };
    }

    /// Anonymous requests are indistinguishable from one another, so they never
    /// join — not each other, and not a keyed build. They also record nothing.
    #[tokio::test]
    async fn anonymous_requests_never_join_and_never_record() {
        let state = spawn_state();
        for _ in 0..3 {
            match state.claim_spawn_build(None).await {
                SpawnTicket::Claim(claim) => claim.commit(inflight(uuid::Uuid::new_v4(), 9877)),
                SpawnTicket::Join(_) => panic!("an unkeyed request must never join"),
            }
        }
        assert!(
            state.spawn_test_inflight.read().await.is_empty(),
            "an unkeyed claim must record nothing — otherwise the next anonymous \
             request would be handed another caller's runner"
        );
    }

    /// A finished build is not joinable: its entry is dropped and the new
    /// request starts a real build.
    #[tokio::test]
    async fn a_terminal_build_is_replaced_not_joined() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let old = uuid::Uuid::new_v4();
        state
            .build_submissions
            .insert(finished_submission(old))
            .await;
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(old, 9877));

        match state.claim_spawn_build(Some(key.clone())).await {
            SpawnTicket::Claim(claim) => claim.commit(inflight(uuid::Uuid::new_v4(), 9878)),
            SpawnTicket::Join(_) => panic!("a terminal build must not be joined"),
        }
        assert_ne!(
            state.spawn_test_inflight.read().await[&key].submission_id,
            old,
            "the stale entry must be replaced by the new build"
        );
    }

    /// A build id the store has never heard of is UNKNOWN, not finished:
    /// `submit_spawn` registers from a spawned task, so a just-started build is
    /// briefly absent. Reading that as terminal would let the retry arriving in
    /// exactly that window start the duplicate build.
    #[tokio::test]
    async fn an_unregistered_submission_reads_as_in_flight() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let id = uuid::Uuid::new_v4();
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(id, 9877));

        match state.claim_spawn_build(Some(key)).await {
            SpawnTicket::Join(existing) => assert_eq!(existing.submission_id, id),
            SpawnTicket::Claim(_) => {
                panic!("an unregistered submission must read as in flight, not as terminal")
            }
        };
    }

    /// Releasing is scoped to the submission that owns the key, so a build
    /// finishing late cannot evict the build that replaced it.
    #[tokio::test]
    async fn release_only_removes_its_own_submission() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let current = uuid::Uuid::new_v4();
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(current, 9878));

        state
            .release_spawn_inflight(&key, uuid::Uuid::new_v4())
            .await;
        assert!(
            state.spawn_test_inflight.read().await.contains_key(&key),
            "a stale build's release must not evict its successor"
        );

        state.release_spawn_inflight(&key, current).await;
        assert!(
            !state.spawn_test_inflight.read().await.contains_key(&key),
            "the owning build's release must free the key"
        );
    }

    /// The recovery surface for a lost `POST /runners/spawn-test` answer.
    ///
    /// A caller whose connection died before the response was written (its own
    /// timeout, or a supervisor restart during a 20-50 minute build) has
    /// created a runner whose id it does not know — and the id is the only
    /// handle on `/runners/{id}/logs` and `/runners/{id}/stop`. The index
    /// already holds the exact triple; the snapshot is what lets the caller
    /// read it back instead of polling `GET /runners` and guessing by
    /// `requester_id`.
    #[tokio::test]
    async fn inflight_snapshot_answers_which_runner_a_requester_just_created() {
        let state = spawn_state();
        let mine = uuid::Uuid::new_v4();
        {
            let mut index = state.spawn_test_inflight.write().await;
            index.insert(dedup_key("agent-1", "origin_main"), inflight(mine, 9878));
            index.insert(
                dedup_key("agent-2", "origin_main"),
                inflight(uuid::Uuid::new_v4(), 9879),
            );
        }

        let mine_only = state.spawn_inflight_snapshot(Some("agent-1")).await;
        assert_eq!(mine_only.len(), 1, "must not leak a peer's spawn");
        assert_eq!(mine_only[0].0.requester_id, "agent-1");
        assert_eq!(mine_only[0].1.submission_id, mine);
        assert_eq!(
            mine_only[0].1.runner_id, "test-9878",
            "the snapshot must name the RUNNER ID, which is the caller's only handle on \
             /runners/{{id}}/logs and /runners/{{id}}/stop"
        );
        assert_eq!(mine_only[0].1.port, 9878);

        // Unfiltered lists everything, stably ordered.
        let all = state.spawn_inflight_snapshot(None).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.requester_id, "agent-1");
        assert_eq!(all[1].0.requester_id, "agent-2");

        // A requester with nothing in flight gets an empty answer, not a peer's.
        assert!(state
            .spawn_inflight_snapshot(Some("agent-3"))
            .await
            .is_empty());
    }
}

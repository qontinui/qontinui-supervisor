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
        }
    }

    /// Check if this runner is protected.
    pub async fn is_protected(&self) -> bool {
        *self.protected.read().await
    }
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
    pub ai: RwLock<AiState>,
    pub expo: RwLock<ExpoState>,
    pub diagnostics: RwLock<DiagnosticsState>,
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
    /// for spawned runners. Created once at startup; every runner spawned
    /// via `start_managed_runner` is assigned to it via
    /// `RunnerJob::assign`. When this `Arc` drops (at supervisor exit) the
    /// kernel closes the last handle to the job and terminates every
    /// assigned process — so a force-killed or panicked supervisor cannot
    /// leave orphan runners holding slot binaries.
    ///
    /// `None` when the OS refused to create the job (extremely rare on
    /// Windows; non-Windows builds always get a no-op stub via
    /// `process::job`'s cross-platform shim). Spawning continues either
    /// way — without the safety net, but functional.
    pub runner_job: Option<Arc<RunnerJob>>,
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
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_requested: bool,
    pub stop_requested: bool,
    pub pid: Option<u32>,
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
            // Compute the short inline tail from the same source as the 4 KiB
            // detail so they tell a consistent story. `last_error_log` is the
            // 1 KiB summary surfaced in `GET /builds`; `last_error_detail` is
            // the longer tail. Both are derived from the captured stderr.
            self.last_error_log = error_detail
                .as_deref()
                .map(|s| tail_bytes_keep_utf8(s, LAST_ERROR_LOG_MAX_BYTES));
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
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LkgInfo {
    pub built_at: DateTime<Utc>,
    pub source_slot: usize,
    pub exe_size: u64,
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
            slots.push(Arc::new(BuildSlot {
                id,
                target_dir,
                busy: RwLock::new(None),
                history: RwLock::new(SlotHistory::new()),
                frontend_stale: RwLock::new(false),
                last_build_stderr_capture: RwLock::new(None),
                last_build_log: RwLock::new(None),
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

impl SupervisorState {
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
        let log_dir = config.log_dir.as_deref();
        let mut runners_map = HashMap::new();
        for rc in &config.runners {
            let managed = Arc::new(ManagedRunner::new_with_log_dir(
                rc.clone(),
                watchdog_enabled,
                log_dir,
            ));
            runners_map.insert(rc.id.clone(), managed);
        }

        let build_pool = BuildPool::new(&config);

        // Create the kill-on-exit JobObject. On Windows this enforces that
        // every supervisor-spawned runner dies when the supervisor exits
        // (graceful, panic, force-kill). On non-Windows it's a no-op stub.
        // Failure to create is logged loudly but never aborts startup —
        // the supervisor still functions, just without the safety net.
        //
        // The `state.logs` collector isn't constructed yet at this point
        // (it's initialized inside the `Self { ... }` block below), so we
        // can't directly emit to the dashboard log stream. Instead, we
        // capture the success/failure message into `pending_startup_logs`,
        // which `flush_pending_startup_logs` drains right after the state
        // is wrapped in an `Arc` (see `main.rs`).
        let mut startup_logs: Vec<(LogLevel, String)> = Vec::new();
        let runner_job = match RunnerJob::create() {
            Ok(j) => {
                let msg = "Created kill-on-exit JobObject for spawned runners \
                           (KILL_ON_JOB_CLOSE)"
                    .to_string();
                tracing::info!("{}", msg);
                startup_logs.push((LogLevel::Info, msg));
                Some(Arc::new(j))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to create runner JobObject — supervisor exit will NOT \
                     terminate spawned runners; orphans may linger and lock build slots"
                );
                startup_logs.push((
                    LogLevel::Warn,
                    format!(
                        "Failed to create runner JobObject: {} — supervisor exit will NOT \
                         terminate spawned runners",
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
            ai: RwLock::new(AiState::new(auto_debug)),
            expo: RwLock::new(ExpoState::new(expo_port)),
            diagnostics: RwLock::new(DiagnosticsState::new()),
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
            runner_job,
            pending_startup_logs: std::sync::Mutex::new(startup_logs),
            active_sse_connections: Arc::new(AtomicUsize::new(0)),
            debug_endpoints_enabled,
            supervisor_started_at: std::time::SystemTime::now(),
            synthetic_build_id_tx,
        }
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
    fn test_slot_history_last_error_log_truncates_to_1k() {
        let mut h = SlotHistory::new();
        let big = "Z".repeat(LAST_ERROR_LOG_MAX_BYTES * 4);
        let detail = format!("{}TAIL_LOG_END", big);
        h.record(1.0, false, Some("err".into()), Some(detail));
        let stored = h.last_error_log.as_ref().expect("log recorded");
        assert!(stored.ends_with("TAIL_LOG_END"));
        // Tail-only helper does not prepend a marker; the cap is the cap.
        assert!(
            stored.len() <= LAST_ERROR_LOG_MAX_BYTES,
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
}

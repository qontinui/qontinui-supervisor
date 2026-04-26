use crate::config::{RunnerConfig, SupervisorConfig};
use crate::diagnostics::DiagnosticsState;
use crate::health_cache::{CachedPortHealth, CachedRunnerHealth};
use crate::log_capture::LogState;
use crate::process::panic_log::RecentPanic;
use crate::process::stopped_cache::StoppedRunnerSnapshot;
use crate::routes::supervisor_bridge::CommandRelay;
use crate::velocity_improvement::VelocityImprovementState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
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
    /// Monitor placement targets for spawned temp runners. Loaded from
    /// `supervisor-settings.json` on startup, mutated by the dashboard via
    /// `PUT /spawn-monitors`. Round-robin pick uses `next_monitor_index`.
    pub spawn_monitors: RwLock<Vec<crate::settings::MonitorConfig>>,
    /// Round-robin counter for choosing which enabled monitor a new temp
    /// runner should land on. Wraps modulo enabled-count at pick time.
    pub next_monitor_index: AtomicUsize,
    /// Unique identifier for this supervisor process instance. Generated once
    /// at startup. Returned in heartbeat responses so the dashboard SPA can
    /// detect a supervisor restart and force-reload itself, recovering from
    /// the wedged-tab scenario where the SSE/command channel is dead but the
    /// fetch layer still works.
    pub boot_id: String,
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
        }
    }

    pub fn record(&mut self, duration_secs: f64, success: bool, error: Option<String>) {
        if self.recent_durations_secs.len() >= RECENT_BUILD_SAMPLE_COUNT {
            self.recent_durations_secs.pop_front();
        }
        self.recent_durations_secs.push_back(duration_secs);
        self.total_builds += 1;
        if success {
            self.successful_builds += 1;
        } else {
            self.last_error = error;
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
            }));
        }
        Self {
            slots,
            permits: Arc::new(Semaphore::new(pool_size)),
            npm_lock: Arc::new(tokio::sync::Mutex::new(())),
            queue_depth: Arc::new(AtomicUsize::new(0)),
            last_successful_slot: RwLock::new(None),
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

impl SupervisorState {
    pub fn new(config: SupervisorConfig) -> Self {
        let watchdog_enabled = config.watchdog_enabled_at_start;
        let auto_debug = config.auto_debug;
        let expo_port = config.expo_port;
        let (health_tx, _) = broadcast::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
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
            cached_health: RwLock::new(CachedPortHealth::default()),
            cached_runner_health: RwLock::new(Vec::new()),
            health_cache_notify: Notify::new(),
            http_client,
            test_auto_login: RwLock::new(None),
            stopped_runners: Arc::new(RwLock::new(HashMap::new())),
            spawn_monitors: RwLock::new(Vec::new()),
            next_monitor_index: AtomicUsize::new(0),
            boot_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Pick the next enabled monitor in round-robin order. Returns `None`
    /// if no monitors are configured or all are disabled.
    pub async fn pick_next_monitor(&self) -> Option<crate::settings::MonitorConfig> {
        use std::sync::atomic::Ordering;
        let monitors = self.spawn_monitors.read().await;
        let enabled: Vec<&crate::settings::MonitorConfig> =
            monitors.iter().filter(|m| m.enabled).collect();
        if enabled.is_empty() {
            return None;
        }
        let idx = self.next_monitor_index.fetch_add(1, Ordering::Relaxed) % enabled.len();
        Some(enabled[idx].clone())
    }

    pub fn notify_health_change(&self) {
        let _ = self.health_tx.send(());
    }

    /// Get a managed runner by ID.
    pub async fn get_runner(&self, id: &str) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.get(id).cloned()
    }

    /// Get the primary runner.
    pub async fn get_primary(&self) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.values().find(|r| r.config.is_primary).cloned()
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
        h.record(10.0, true, None);
        h.record(20.0, true, None);
        h.record(30.0, false, Some("boom".into()));
        assert_eq!(h.total_builds, 3);
        assert_eq!(h.successful_builds, 2);
        assert!((h.avg_duration_secs().unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(h.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn test_slot_history_window_evicts() {
        let mut h = SlotHistory::new();
        for i in 0..(RECENT_BUILD_SAMPLE_COUNT + 3) {
            h.record(i as f64, true, None);
        }
        assert_eq!(h.recent_durations_secs.len(), RECENT_BUILD_SAMPLE_COUNT);
        assert_eq!(h.recent_durations_secs.front().copied(), Some(3.0));
    }

    #[test]
    fn test_slot_history_p50() {
        let mut h = SlotHistory::new();
        h.record(5.0, true, None);
        h.record(1.0, true, None);
        h.record(9.0, true, None);
        assert_eq!(h.p50_duration_secs(), Some(5.0));
    }

    #[tokio::test]
    async fn test_supervisor_state_expo_port_from_config() {
        let mut config = make_test_config();
        config.expo_port = 9999;
        let state = SupervisorState::new(config);
        let expo = state.expo.try_read().unwrap();
        assert_eq!(expo.port, 9999);
    }
}

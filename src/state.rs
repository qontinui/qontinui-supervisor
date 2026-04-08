use crate::config::{RunnerConfig, SupervisorConfig, AI_OUTPUT_BUFFER_SIZE};
use crate::diagnostics::DiagnosticsState;
use crate::health_cache::{CachedPortHealth, CachedRunnerHealth};
use crate::log_capture::LogState;
use crate::routes::supervisor_bridge::CommandRelay;
use crate::smart_rebuild::SmartRebuildState;
use crate::velocity_improvement::VelocityImprovementState;
use crate::workflow_loop::WorkflowLoopState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
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
}

impl ManagedRunner {
    pub fn new(config: RunnerConfig, watchdog_enabled: bool) -> Self {
        let protected = config.protected;
        Self {
            config,
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            cached_health: RwLock::new(CachedPortHealth::default()),
            health_cache_notify: Notify::new(),
            logs: LogState::new(),
            protected: RwLock::new(protected),
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
    pub code_activity: RwLock<CodeActivityState>,
    pub expo: RwLock<ExpoState>,
    pub overnight_watchdog: RwLock<OvernightWatchdogState>,
    pub workflow_loop: RwLock<WorkflowLoopState>,
    pub diagnostics: RwLock<DiagnosticsState>,
    pub evaluation: RwLock<EvaluationState>,
    pub velocity_tests: RwLock<VelocityTestState>,
    pub velocity_improvement: RwLock<VelocityImprovementState>,
    pub smart_rebuild: RwLock<SmartRebuildState>,
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
    pub db: Option<Arc<Mutex<rusqlite::Connection>>>,
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
    pub process: Option<Child>,
    pub running: bool,
    pub provider: String,
    pub model: String,
    pub auto_debug_enabled: bool,
    pub last_debug_at: Option<DateTime<Utc>>,
    pub session_started_at: Option<DateTime<Utc>>,
    pub output_buffer: VecDeque<AiOutputEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiOutputEntry {
    pub timestamp: DateTime<Utc>,
    pub stream: String, // "stdout" or "stderr"
    pub line: String,
}

pub struct CodeActivityState {
    pub last_code_change_at: Option<DateTime<Utc>>,
    pub code_being_edited: bool,
    pub external_claude_session: bool,
    pub pending_debug: bool,
    pub pending_debug_reason: Option<String>,
}

pub struct OvernightWatchdogState {
    pub active: bool,
    pub last_check_at: Option<DateTime<Utc>>,
    pub last_successful_check_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub last_failure_reason: Option<String>,
    pub last_action_taken: Option<String>,
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
        let smart_rebuild_enabled = config.smart_rebuild;
        let expo_port = config.expo_port;
        let (health_tx, _) = broadcast::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        // Build multi-runner map from config
        let mut runners_map = HashMap::new();
        for rc in &config.runners {
            let managed = Arc::new(ManagedRunner::new(rc.clone(), watchdog_enabled));
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
            code_activity: RwLock::new(CodeActivityState::new()),
            expo: RwLock::new(ExpoState::new(expo_port)),
            overnight_watchdog: RwLock::new(OvernightWatchdogState::new()),
            workflow_loop: RwLock::new(WorkflowLoopState::new()),
            diagnostics: RwLock::new(DiagnosticsState::new()),
            evaluation: RwLock::new(EvaluationState::new()),
            velocity_tests: RwLock::new(VelocityTestState::new()),
            velocity_improvement: RwLock::new(VelocityImprovementState::new()),
            smart_rebuild: RwLock::new(SmartRebuildState::new(smart_rebuild_enabled)),
            command_relay: CommandRelay::new(),
            logs: LogState::new(),
            health_tx,
            shutdown_tx,
            cached_health: RwLock::new(CachedPortHealth::default()),
            cached_runner_health: RwLock::new(Vec::new()),
            health_cache_notify: Notify::new(),
            http_client,
            db: None,
        }
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

    pub fn record_crash(&mut self) {
        self.crash_history.push(Utc::now());
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
            process: None,
            running: false,
            provider: "claude".to_string(),
            model: "opus".to_string(),
            auto_debug_enabled,
            last_debug_at: None,
            session_started_at: None,
            output_buffer: VecDeque::with_capacity(AI_OUTPUT_BUFFER_SIZE),
        }
    }

    pub fn push_output(&mut self, stream: &str, line: String) {
        if self.output_buffer.len() >= AI_OUTPUT_BUFFER_SIZE {
            self.output_buffer.pop_front();
        }
        self.output_buffer.push_back(AiOutputEntry {
            timestamp: Utc::now(),
            stream: stream.to_string(),
            line,
        });
    }
}

impl Default for CodeActivityState {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeActivityState {
    pub fn new() -> Self {
        Self {
            last_code_change_at: None,
            code_being_edited: false,
            external_claude_session: false,
            pending_debug: false,
            pending_debug_reason: None,
        }
    }
}

impl OvernightWatchdogState {
    pub fn new() -> Self {
        Self {
            active: false,
            last_check_at: None,
            last_successful_check_at: None,
            consecutive_failures: 0,
            last_failure_reason: None,
            last_action_taken: None,
        }
    }
}

impl Default for OvernightWatchdogState {
    fn default() -> Self {
        Self::new()
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
    use crate::config::{
        RunnerConfig, SupervisorConfig, AI_OUTPUT_BUFFER_SIZE, DEFAULT_SUPERVISOR_PORT, EXPO_PORT,
    };
    use std::path::PathBuf;

    fn make_test_config() -> SupervisorConfig {
        SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            dev_mode: true,
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            smart_rebuild: false,
            log_file: None,
            port: DEFAULT_SUPERVISOR_PORT,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: EXPO_PORT,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: crate::config::BuildPoolConfig { pool_size: 1 },
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

    #[test]
    fn test_watchdog_record_crash_adds_to_history() {
        let mut state = WatchdogState::new(true);
        assert_eq!(state.crash_history.len(), 0);
        state.record_crash();
        assert_eq!(state.crash_history.len(), 1);
        state.record_crash();
        assert_eq!(state.crash_history.len(), 2);
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
        assert!(!state.running);
        assert!(state.process.is_none());
        assert_eq!(state.provider, "claude");
        assert_eq!(state.model, "opus");
        assert!(state.auto_debug_enabled);
        assert!(state.last_debug_at.is_none());
        assert!(state.session_started_at.is_none());
        assert!(state.output_buffer.is_empty());
    }

    #[test]
    fn test_ai_state_new_with_auto_debug_disabled() {
        let state = AiState::new(false);
        assert!(!state.auto_debug_enabled);
    }

    #[test]
    fn test_ai_state_push_output_adds_entries() {
        let mut state = AiState::new(false);
        state.push_output("stdout", "Hello world".to_string());
        assert_eq!(state.output_buffer.len(), 1);
        assert_eq!(state.output_buffer[0].stream, "stdout");
        assert_eq!(state.output_buffer[0].line, "Hello world");
    }

    #[test]
    fn test_ai_state_push_output_respects_buffer_limit() {
        let mut state = AiState::new(false);
        // Fill the buffer to capacity
        for i in 0..AI_OUTPUT_BUFFER_SIZE {
            state.push_output("stdout", format!("line {}", i));
        }
        assert_eq!(state.output_buffer.len(), AI_OUTPUT_BUFFER_SIZE);

        // Push one more — should evict the oldest
        state.push_output("stdout", "overflow line".to_string());
        assert_eq!(state.output_buffer.len(), AI_OUTPUT_BUFFER_SIZE);
        // The oldest ("line 0") should be gone; the front is now "line 1"
        assert_eq!(state.output_buffer.front().unwrap().line, "line 1");
        assert_eq!(state.output_buffer.back().unwrap().line, "overflow line");
    }

    #[test]
    fn test_ai_state_push_output_stderr() {
        let mut state = AiState::new(false);
        state.push_output("stderr", "error message".to_string());
        assert_eq!(state.output_buffer[0].stream, "stderr");
    }

    // --- CodeActivityState tests ---

    #[test]
    fn test_code_activity_state_new_defaults() {
        let state = CodeActivityState::new();
        assert!(state.last_code_change_at.is_none());
        assert!(!state.code_being_edited);
        assert!(!state.external_claude_session);
        assert!(!state.pending_debug);
        assert!(state.pending_debug_reason.is_none());
    }

    #[test]
    fn test_code_activity_state_default_matches_new() {
        let from_new = CodeActivityState::new();
        let from_default = CodeActivityState::default();
        assert_eq!(from_new.code_being_edited, from_default.code_being_edited);
        assert_eq!(from_new.pending_debug, from_default.pending_debug);
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
        assert!(state.config.dev_mode);
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
        assert!(!cached.vite_port_open);
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
    async fn test_supervisor_state_expo_port_from_config() {
        let mut config = make_test_config();
        config.expo_port = 9999;
        let state = SupervisorState::new(config);
        let expo = state.expo.try_read().unwrap();
        assert_eq!(expo.port, 9999);
    }
}

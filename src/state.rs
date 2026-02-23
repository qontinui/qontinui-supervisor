use crate::config::{SupervisorConfig, AI_OUTPUT_BUFFER_SIZE};
use crate::diagnostics::DiagnosticsState;
use crate::health_cache::CachedPortHealth;
use crate::log_capture::LogState;
use crate::velocity_improvement::VelocityImprovementState;
use crate::workflow_loop::WorkflowLoopState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{broadcast, watch, Notify, RwLock};

pub type SharedState = Arc<SupervisorState>;

pub struct SupervisorState {
    pub config: SupervisorConfig,
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub build: RwLock<BuildState>,
    pub ai: RwLock<AiState>,
    pub code_activity: RwLock<CodeActivityState>,
    pub expo: RwLock<ExpoState>,
    pub workflow_loop: RwLock<WorkflowLoopState>,
    pub diagnostics: RwLock<DiagnosticsState>,
    pub evaluation: RwLock<EvaluationState>,
    pub velocity_tests: RwLock<VelocityTestState>,
    pub velocity_improvement: RwLock<VelocityImprovementState>,
    pub logs: LogState,
    pub health_tx: broadcast::Sender<()>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub cached_health: RwLock<CachedPortHealth>,
    pub health_cache_notify: Notify,
    pub http_client: reqwest::Client,
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
    pub build_in_progress: bool,
    pub build_error_detected: bool,
    pub last_build_error: Option<String>,
    pub last_build_at: Option<DateTime<Utc>>,
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
        Self {
            config,
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            build: RwLock::new(BuildState::new()),
            ai: RwLock::new(AiState::new(auto_debug)),
            code_activity: RwLock::new(CodeActivityState::new()),
            expo: RwLock::new(ExpoState::new(expo_port)),
            workflow_loop: RwLock::new(WorkflowLoopState::new()),
            diagnostics: RwLock::new(DiagnosticsState::new()),
            evaluation: RwLock::new(EvaluationState::new()),
            velocity_tests: RwLock::new(VelocityTestState::new()),
            velocity_improvement: RwLock::new(VelocityImprovementState::new()),
            logs: LogState::new(),
            health_tx,
            shutdown_tx,
            cached_health: RwLock::new(CachedPortHealth::default()),
            health_cache_notify: Notify::new(),
            http_client,
        }
    }

    pub fn notify_health_change(&self) {
        let _ = self.health_tx.send(());
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

    pub fn is_crash_loop(&self, threshold: usize, window_secs: i64) -> bool {
        let cutoff = Utc::now() - chrono::Duration::seconds(window_secs);
        let recent = self.crash_history.iter().filter(|t| **t > cutoff).count();
        recent >= threshold
    }

    pub fn is_in_cooldown(&self, cooldown_secs: i64) -> bool {
        if let Some(last) = self.last_restart_at {
            let elapsed = (Utc::now() - last).num_seconds();
            elapsed < cooldown_secs
        } else {
            false
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
        SupervisorConfig, AI_OUTPUT_BUFFER_SIZE, DEFAULT_SUPERVISOR_PORT, EXPO_PORT,
    };
    use std::path::PathBuf;

    fn make_test_config() -> SupervisorConfig {
        SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            dev_mode: true,
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            port: DEFAULT_SUPERVISOR_PORT,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: EXPO_PORT,
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

    #[test]
    fn test_watchdog_is_crash_loop_below_threshold() {
        let mut state = WatchdogState::new(true);
        // Add 4 crashes (below threshold of 5)
        for _ in 0..4 {
            state.record_crash();
        }
        assert!(!state.is_crash_loop(5, 600));
    }

    #[test]
    fn test_watchdog_is_crash_loop_at_threshold() {
        let mut state = WatchdogState::new(true);
        // Add 5 crashes (at threshold of 5)
        for _ in 0..5 {
            state.record_crash();
        }
        assert!(state.is_crash_loop(5, 600));
    }

    #[test]
    fn test_watchdog_is_crash_loop_above_threshold() {
        let mut state = WatchdogState::new(true);
        for _ in 0..10 {
            state.record_crash();
        }
        assert!(state.is_crash_loop(5, 600));
    }

    #[test]
    fn test_watchdog_is_not_in_cooldown_when_never_restarted() {
        let state = WatchdogState::new(true);
        assert!(!state.is_in_cooldown(60));
    }

    #[test]
    fn test_watchdog_is_in_cooldown_when_just_restarted() {
        let mut state = WatchdogState::new(true);
        state.last_restart_at = Some(Utc::now());
        assert!(state.is_in_cooldown(60));
    }

    #[test]
    fn test_watchdog_is_not_in_cooldown_after_long_time() {
        let mut state = WatchdogState::new(true);
        state.last_restart_at = Some(Utc::now() - chrono::Duration::seconds(120));
        assert!(!state.is_in_cooldown(60));
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

    #[test]
    fn test_supervisor_state_construction() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        assert!(state.config.dev_mode);
        assert_eq!(state.config.port, DEFAULT_SUPERVISOR_PORT);
    }

    #[test]
    fn test_supervisor_state_runner_initial_state() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let runner = state.runner.try_read().unwrap();
        assert!(!runner.running);
        assert!(runner.pid.is_none());
    }

    #[test]
    fn test_supervisor_state_watchdog_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(!watchdog.enabled);
    }

    #[test]
    fn test_supervisor_state_watchdog_enabled_from_config() {
        let mut config = make_test_config();
        config.watchdog_enabled_at_start = true;
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(watchdog.enabled);
    }

    #[test]
    fn test_supervisor_state_auto_debug_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(!ai.auto_debug_enabled);
    }

    #[test]
    fn test_supervisor_state_auto_debug_enabled_from_config() {
        let mut config = make_test_config();
        config.auto_debug = true;
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(ai.auto_debug_enabled);
    }

    #[test]
    fn test_supervisor_state_cached_health_defaults_to_all_false() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let cached = state.cached_health.try_read().unwrap();
        assert!(!cached.runner_port_open);
        assert!(!cached.runner_responding);
        assert!(!cached.vite_port_open);
    }

    #[test]
    fn test_supervisor_state_build_not_in_progress() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let build = state.build.try_read().unwrap();
        assert!(!build.build_in_progress);
        assert!(!build.build_error_detected);
    }

    #[test]
    fn test_supervisor_state_notify_health_change_does_not_panic() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        // Should not panic even with no subscribers
        state.notify_health_change();
    }

    #[test]
    fn test_supervisor_state_expo_port_from_config() {
        let mut config = make_test_config();
        config.expo_port = 9999;
        let state = SupervisorState::new(config);
        let expo = state.expo.try_read().unwrap();
        assert_eq!(expo.port, 9999);
    }
}

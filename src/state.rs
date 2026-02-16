use crate::config::{SupervisorConfig, AI_OUTPUT_BUFFER_SIZE};
use crate::health_cache::CachedPortHealth;
use crate::log_capture::LogState;
use crate::workflow_loop::WorkflowLoopState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{broadcast, Notify, RwLock};

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

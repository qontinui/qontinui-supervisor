use crate::config::SupervisorConfig;
use crate::log_capture::LogState;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::RwLock;

pub type SharedState = Arc<SupervisorState>;

pub struct SupervisorState {
    pub config: SupervisorConfig,
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub build: RwLock<BuildState>,
    pub logs: LogState,
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

impl SupervisorState {
    pub fn new(config: SupervisorConfig) -> Self {
        let watchdog_enabled = config.watchdog_enabled_at_start;
        Self {
            config,
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            build: RwLock::new(BuildState::new()),
            logs: LogState::new(),
        }
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
        let recent = self.crash_history.iter()
            .filter(|t| **t > cutoff)
            .count();
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

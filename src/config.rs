use clap::Parser;
use std::path::PathBuf;

/// Qontinui Supervisor — manages the qontinui-runner process lifecycle.
#[derive(Parser, Debug, Clone)]
#[command(name = "qontinui-supervisor")]
pub struct CliArgs {
    /// Path to qontinui-runner/src-tauri directory
    #[arg(short = 'p', long = "project-dir")]
    pub project_dir: PathBuf,

    /// Run 'npm run tauri dev' instead of compiled exe
    #[arg(short = 'd', long = "dev-mode")]
    pub dev_mode: bool,

    /// Enable watchdog (implies auto-start)
    #[arg(short = 'w', long = "watchdog")]
    pub watchdog: bool,

    /// Start runner on supervisor launch
    #[arg(short = 'a', long = "auto-start")]
    pub auto_start: bool,

    /// Log file for runner output
    #[arg(short = 'l', long = "log-file")]
    pub log_file: Option<PathBuf>,

    /// Supervisor HTTP port
    #[arg(long = "port", default_value_t = DEFAULT_SUPERVISOR_PORT)]
    pub port: u16,

    /// Enable AI auto-debug on startup
    #[arg(long = "auto-debug")]
    pub auto_debug: bool,

    /// Path to Expo/React Native project directory
    #[arg(long = "expo-dir")]
    pub expo_dir: Option<PathBuf>,
}

#[allow(dead_code)]
pub struct SupervisorConfig {
    pub project_dir: PathBuf,
    pub dev_mode: bool,
    pub watchdog_enabled_at_start: bool,
    pub auto_start: bool,
    pub auto_debug: bool,
    pub log_file: Option<PathBuf>,
    pub port: u16,
    pub dev_logs_dir: PathBuf,
    pub cli_args: Vec<String>,
    pub expo_dir: Option<PathBuf>,
    pub expo_port: u16,
}

// Port constants
pub const DEFAULT_SUPERVISOR_PORT: u16 = 9875;
pub const RUNNER_API_PORT: u16 = 9876;
pub const RUNNER_VITE_PORT: u16 = 1420;
pub const EXPO_PORT: u16 = 8081;

// Watchdog constants
pub const WATCHDOG_CHECK_INTERVAL_SECS: u64 = 10;
pub const WATCHDOG_MAX_RESTART_ATTEMPTS: u32 = 3;
pub const WATCHDOG_CRASH_LOOP_THRESHOLD: usize = 5;
pub const WATCHDOG_CRASH_LOOP_WINDOW_SECS: i64 = 600; // 10 minutes
pub const WATCHDOG_COOLDOWN_SECS: i64 = 60;

// Process constants
pub const GRACEFUL_KILL_TIMEOUT_SECS: u64 = 5;
pub const BUILD_TIMEOUT_SECS: u64 = 300; // 5 minutes
#[allow(dead_code)]
pub const PORT_WAIT_TIMEOUT_SECS: u64 = 120;
pub const PORT_CHECK_INTERVAL_MS: u64 = 500;

// Build monitor constants
pub const BUILD_MONITOR_WINDOW_SECS: u64 = 60;

// Log constants
pub const LOG_BUFFER_SIZE: usize = 500;

// AI debug constants
pub const AI_DEBUG_COOLDOWN_SECS: i64 = 300; // 5 minutes between sessions
pub const AI_OUTPUT_BUFFER_SIZE: usize = 2000;

// Code activity constants
pub const CODE_QUIET_PERIOD_SECS: i64 = 300; // 5 minutes
pub const CODE_CHECK_RETRY_INTERVAL_SECS: u64 = 30;

// Workflow loop constants
pub const WORKFLOW_LOOP_POLL_INTERVAL_SECS: u64 = 5;
pub const WORKFLOW_LOOP_MAX_ITERATIONS_DEFAULT: u32 = 5;
pub const WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS: u64 = 120;
pub const WORKFLOW_LOOP_RUNNER_HEALTH_POLL_SECS: u64 = 2;
pub const WORKFLOW_LOOP_FIX_TIMEOUT_SECS: u64 = 600;

// AI model definitions: (provider, key, model_id, display_name)
pub const AI_MODELS: &[(&str, &str, &str, &str)] = &[
    ("claude", "opus", "claude-opus-4-6", "Claude Opus 4.6"),
    (
        "claude",
        "sonnet",
        "claude-sonnet-4-5-20250929",
        "Claude Sonnet 4.5",
    ),
    (
        "gemini",
        "flash",
        "gemini-3-flash-preview",
        "Gemini 3 Flash",
    ),
    ("gemini", "pro", "gemini-3-pro-preview", "Gemini 3 Pro"),
];

// Dev-start service ports for status checking
pub const SERVICE_PORTS: &[(&str, u16)] = &[
    ("postgresql", 5432),
    ("redis", 6379),
    ("minio", 9000),
    ("backend", 8000),
    ("frontend", 3001),
    ("runner_api", 9876),
    ("vite", 1420),
];

impl SupervisorConfig {
    pub fn from_args(args: CliArgs) -> Self {
        let auto_start = args.auto_start || args.watchdog;
        let dev_logs_dir = args
            .project_dir
            .parent()
            .unwrap_or(&args.project_dir)
            .parent()
            .unwrap_or(&args.project_dir)
            .join(".dev-logs");

        let cli_args = std::env::args().collect();

        SupervisorConfig {
            project_dir: args.project_dir,
            dev_mode: args.dev_mode,
            watchdog_enabled_at_start: args.watchdog,
            auto_start,
            auto_debug: args.auto_debug,
            log_file: args.log_file,
            port: args.port,
            dev_logs_dir,
            cli_args,
            expo_dir: args.expo_dir,
            expo_port: EXPO_PORT,
        }
    }

    /// Path to the runner executable (for exe mode)
    pub fn runner_exe_path(&self) -> PathBuf {
        self.project_dir
            .join("target")
            .join("debug")
            .join("qontinui-runner.exe")
    }

    /// Path to the runner npm project root (parent of src-tauri)
    pub fn runner_npm_dir(&self) -> PathBuf {
        self.project_dir
            .parent()
            .unwrap_or(&self.project_dir)
            .to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Port constant tests ---

    #[test]
    fn test_default_supervisor_port() {
        assert_eq!(DEFAULT_SUPERVISOR_PORT, 9875);
    }

    #[test]
    fn test_runner_api_port() {
        assert_eq!(RUNNER_API_PORT, 9876);
    }

    #[test]
    fn test_runner_vite_port() {
        assert_eq!(RUNNER_VITE_PORT, 1420);
    }

    #[test]
    fn test_expo_port() {
        assert_eq!(EXPO_PORT, 8081);
    }

    // --- Watchdog constant tests ---

    #[test]
    fn test_watchdog_max_restart_attempts() {
        assert_eq!(WATCHDOG_MAX_RESTART_ATTEMPTS, 3);
    }

    #[test]
    fn test_watchdog_crash_loop_threshold() {
        assert_eq!(WATCHDOG_CRASH_LOOP_THRESHOLD, 5);
    }

    #[test]
    fn test_watchdog_crash_loop_window_is_10_minutes() {
        assert_eq!(WATCHDOG_CRASH_LOOP_WINDOW_SECS, 600);
    }

    #[test]
    fn test_watchdog_cooldown_is_60_seconds() {
        assert_eq!(WATCHDOG_COOLDOWN_SECS, 60);
    }

    // --- Process constant tests ---

    #[test]
    fn test_build_timeout_is_5_minutes() {
        assert_eq!(BUILD_TIMEOUT_SECS, 300);
    }

    #[test]
    fn test_ai_debug_cooldown_is_5_minutes() {
        assert_eq!(AI_DEBUG_COOLDOWN_SECS, 300);
    }

    // --- SERVICE_PORTS tests ---

    #[test]
    fn test_service_ports_has_expected_count() {
        assert_eq!(SERVICE_PORTS.len(), 7);
    }

    #[test]
    fn test_service_ports_contains_postgresql() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "postgresql" && *port == 5432));
    }

    #[test]
    fn test_service_ports_contains_redis() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "redis" && *port == 6379));
    }

    #[test]
    fn test_service_ports_contains_minio() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "minio" && *port == 9000));
    }

    #[test]
    fn test_service_ports_contains_backend() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "backend" && *port == 8000));
    }

    #[test]
    fn test_service_ports_contains_frontend() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "frontend" && *port == 3001));
    }

    #[test]
    fn test_service_ports_contains_runner_api() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "runner_api" && *port == RUNNER_API_PORT));
    }

    #[test]
    fn test_service_ports_contains_vite() {
        assert!(SERVICE_PORTS
            .iter()
            .any(|(name, port)| *name == "vite" && *port == RUNNER_VITE_PORT));
    }

    // --- AI_MODELS tests ---

    #[test]
    fn test_ai_models_has_expected_count() {
        assert_eq!(AI_MODELS.len(), 4);
    }

    #[test]
    fn test_ai_models_contains_claude_opus() {
        assert!(AI_MODELS
            .iter()
            .any(|(provider, key, _, _)| *provider == "claude" && *key == "opus"));
    }

    #[test]
    fn test_ai_models_contains_claude_sonnet() {
        assert!(AI_MODELS
            .iter()
            .any(|(provider, key, _, _)| *provider == "claude" && *key == "sonnet"));
    }

    #[test]
    fn test_ai_models_contains_gemini_flash() {
        assert!(AI_MODELS
            .iter()
            .any(|(provider, key, _, _)| *provider == "gemini" && *key == "flash"));
    }

    #[test]
    fn test_ai_models_contains_gemini_pro() {
        assert!(AI_MODELS
            .iter()
            .any(|(provider, key, _, _)| *provider == "gemini" && *key == "pro"));
    }

    #[test]
    fn test_ai_models_all_have_model_ids() {
        for (_, _, model_id, _) in AI_MODELS {
            assert!(!model_id.is_empty(), "Model ID should not be empty");
        }
    }

    #[test]
    fn test_ai_models_all_have_display_names() {
        for (_, _, _, display_name) in AI_MODELS {
            assert!(!display_name.is_empty(), "Display name should not be empty");
        }
    }

    // --- SupervisorConfig tests ---

    fn make_test_args(watchdog: bool, auto_start: bool) -> CliArgs {
        CliArgs {
            project_dir: PathBuf::from("/tmp/qontinui-runner/src-tauri"),
            dev_mode: true,
            watchdog,
            auto_start,
            log_file: None,
            port: DEFAULT_SUPERVISOR_PORT,
            auto_debug: false,
            expo_dir: None,
        }
    }

    #[test]
    fn test_from_args_basic() {
        let args = make_test_args(false, false);
        let config = SupervisorConfig::from_args(args);
        assert_eq!(
            config.project_dir,
            PathBuf::from("/tmp/qontinui-runner/src-tauri")
        );
        assert!(config.dev_mode);
        assert!(!config.watchdog_enabled_at_start);
        assert!(!config.auto_start);
        assert!(!config.auto_debug);
        assert_eq!(config.port, DEFAULT_SUPERVISOR_PORT);
        assert_eq!(config.expo_port, EXPO_PORT);
        assert!(config.expo_dir.is_none());
    }

    #[test]
    fn test_from_args_watchdog_implies_auto_start() {
        let args = make_test_args(true, false);
        let config = SupervisorConfig::from_args(args);
        assert!(config.watchdog_enabled_at_start);
        assert!(config.auto_start, "watchdog should imply auto_start");
    }

    #[test]
    fn test_from_args_auto_start_without_watchdog() {
        let args = make_test_args(false, true);
        let config = SupervisorConfig::from_args(args);
        assert!(!config.watchdog_enabled_at_start);
        assert!(config.auto_start);
    }

    #[test]
    fn test_runner_exe_path() {
        let args = make_test_args(false, false);
        let config = SupervisorConfig::from_args(args);
        let exe_path = config.runner_exe_path();
        assert!(
            exe_path.ends_with("target/debug/qontinui-runner.exe")
                || exe_path.ends_with("target\\debug\\qontinui-runner.exe")
        );
    }

    #[test]
    fn test_runner_npm_dir() {
        let args = make_test_args(false, false);
        let config = SupervisorConfig::from_args(args);
        let npm_dir = config.runner_npm_dir();
        // src-tauri's parent is qontinui-runner
        assert!(
            npm_dir.ends_with("qontinui-runner")
                || npm_dir.to_string_lossy().contains("qontinui-runner")
        );
    }

    #[test]
    fn test_dev_logs_dir_is_computed() {
        let args = make_test_args(false, false);
        let config = SupervisorConfig::from_args(args);
        // project_dir = /tmp/qontinui-runner/src-tauri
        // dev_logs_dir = project_dir.parent().parent().join(".dev-logs") = /tmp/.dev-logs
        assert!(config.dev_logs_dir.ends_with(".dev-logs"));
    }

    #[test]
    fn test_from_args_with_expo_dir() {
        let mut args = make_test_args(false, false);
        args.expo_dir = Some(PathBuf::from("/tmp/qontinui-mobile"));
        let config = SupervisorConfig::from_args(args);
        assert_eq!(config.expo_dir, Some(PathBuf::from("/tmp/qontinui-mobile")));
        assert_eq!(config.expo_port, EXPO_PORT);
    }
}

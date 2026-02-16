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
}

#[allow(dead_code)]
pub struct SupervisorConfig {
    pub project_dir: PathBuf,
    pub dev_mode: bool,
    pub watchdog_enabled_at_start: bool,
    pub auto_start: bool,
    pub log_file: Option<PathBuf>,
    pub port: u16,
    pub dev_logs_dir: PathBuf,
    pub cli_args: Vec<String>,
}

// Port constants
pub const DEFAULT_SUPERVISOR_PORT: u16 = 9875;
pub const RUNNER_API_PORT: u16 = 9876;
pub const RUNNER_VITE_PORT: u16 = 1420;

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

impl SupervisorConfig {
    pub fn from_args(args: CliArgs) -> Self {
        let auto_start = args.auto_start || args.watchdog;
        let dev_logs_dir = args.project_dir.parent()
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
            log_file: args.log_file,
            port: args.port,
            dev_logs_dir,
            cli_args,
        }
    }

    /// Path to the runner executable (for exe mode)
    pub fn runner_exe_path(&self) -> PathBuf {
        self.project_dir.join("target").join("debug").join("qontinui-runner.exe")
    }

    /// Path to the runner npm project root (parent of src-tauri)
    pub fn runner_npm_dir(&self) -> PathBuf {
        self.project_dir.parent()
            .unwrap_or(&self.project_dir)
            .to_path_buf()
    }
}

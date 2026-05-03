use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Qontinui Supervisor — manages the qontinui-runner process lifecycle.
#[derive(Parser, Debug, Clone)]
#[command(name = "qontinui-supervisor")]
pub struct CliArgs {
    /// Path to qontinui-runner/src-tauri directory
    #[arg(short = 'p', long = "project-dir")]
    pub project_dir: PathBuf,

    /// Enable watchdog (implies auto-start)
    #[arg(short = 'w', long = "watchdog")]
    pub watchdog: bool,

    /// Start runner on supervisor launch
    #[arg(short = 'a', long = "auto-start")]
    pub auto_start: bool,

    /// Persistent log file for the supervisor's in-memory log buffer (append mode).
    /// Every log entry that currently lives in the ring buffer (default 500, override
    /// via `QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE`) is also
    /// written here so a crash-loop can be diagnosed from historical logs.
    /// If unset but `--log-dir` is set, defaults to `<log-dir>/supervisor.log`.
    /// No rotation — the file grows unbounded; rotate it externally if needed.
    #[arg(short = 'l', long = "log-file")]
    pub log_file: Option<PathBuf>,

    /// Directory for persistent log files. When set, the supervisor writes
    /// `<log-dir>/supervisor.log` (unless `--log-file` overrides) plus one
    /// `<log-dir>/<runner-id>.log` per managed runner containing its tee'd
    /// stdout/stderr. Directory is created if it does not exist. No rotation.
    #[arg(long = "log-dir")]
    pub log_dir: Option<PathBuf>,

    /// Supervisor HTTP port
    #[arg(long = "port", default_value_t = DEFAULT_SUPERVISOR_PORT)]
    pub port: u16,

    /// Enable AI auto-debug on startup
    #[arg(long = "auto-debug")]
    pub auto_debug: bool,

    /// Path to Expo/React Native project directory
    #[arg(long = "expo-dir")]
    pub expo_dir: Option<PathBuf>,

    /// Disable post-startup build slot pre-warming (`cargo check` per slot).
    /// Also honored via env var `QONTINUI_SUPERVISOR_NO_PREWARM=1`.
    #[arg(long = "no-prewarm")]
    pub no_prewarm: bool,

    /// Disable the ambient dashboard WebView2 window that auto-registers with
    /// `supervisor-bridge/*` for UI automation (item B of the post-3J UI Bridge
    /// improvements plan).
    ///
    /// By default the supervisor spawns a small minimized WebView2 window on
    /// startup pointing at its own dashboard at `http://127.0.0.1:{port}/`.
    /// The React SPA's `CommandRelayListener` then keeps the supervisor-bridge
    /// heartbeat alive without requiring a human-opened browser tab, so
    /// `responsive: true` is reachable in headless dev loops.
    ///
    /// Use this flag (or the env var `QONTINUI_SUPERVISOR_NO_WEBVIEW=1`) to
    /// skip the window — e.g. on a CI box with no desktop, or when you prefer
    /// to drive the dashboard from your own browser tab.
    #[arg(long = "no-webview")]
    pub no_webview: bool,
}

#[allow(dead_code)]
pub struct SupervisorConfig {
    pub project_dir: PathBuf,
    pub watchdog_enabled_at_start: bool,
    pub auto_start: bool,
    pub auto_debug: bool,
    pub log_file: Option<PathBuf>,
    /// Directory for persistent log files (supervisor.log + per-runner logs).
    /// None disables persistent file logging.
    pub log_dir: Option<PathBuf>,
    pub port: u16,
    pub dev_logs_dir: PathBuf,
    pub cli_args: Vec<String>,
    pub expo_dir: Option<PathBuf>,
    pub expo_port: u16,
    /// Runner configurations. If empty at startup, a default primary runner is created.
    pub runners: Vec<RunnerConfig>,
    /// Parallel cargo build pool configuration.
    pub build_pool: BuildPoolConfig,
    /// When true, skip the post-startup `cargo check` pre-warm of build slots.
    pub no_prewarm: bool,
    /// When true, skip the ambient dashboard WebView2 window (item B of the
    /// post-3J UI Bridge improvements plan). See [`CliArgs::no_webview`].
    pub no_webview: bool,
}

/// Configuration for the parallel cargo build pool.
///
/// Each slot gets its own `CARGO_TARGET_DIR` so concurrent `cargo build`s do not
/// contend on a shared `target/`. Source tree is shared (live working tree);
/// callers accept the same source-mutation race that single-build today already has.
#[derive(Debug, Clone)]
pub struct BuildPoolConfig {
    /// Number of concurrent cargo builds allowed. Default: 3.
    /// Override via env var `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`.
    pub pool_size: usize,
}

impl Default for BuildPoolConfig {
    fn default() -> Self {
        let pool_size = std::env::var("QONTINUI_SUPERVISOR_BUILD_POOL_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(3);
        Self { pool_size }
    }
}

/// Configuration for a single managed runner instance.
///
/// Note: `is_primary: bool` is retained for serde compat with the on-disk
/// `settings.json` file format. Switching this to a `RunnerKind` field would
/// break older settings files; the cleaner migration is a one-shot
/// settings.json rewrite (per the plan's guiding priorities) tracked as a
/// follow-up to Item 2. The [`RunnerConfig::kind`] method provides the
/// `RunnerKind` view without changing the on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub is_primary: bool,
    /// When true, this runner cannot be stopped or restarted by smart rebuild,
    /// watchdog, AI sessions, or workflow loop between-iterations. Only manual
    /// API calls with `force: true` can override protection.
    #[serde(default = "default_true")]
    pub protected: bool,
    #[serde(default)]
    pub server_mode: bool,
    #[serde(default)]
    pub restate_ingress_port: Option<u16>,
    #[serde(default)]
    pub restate_admin_port: Option<u16>,
    #[serde(default)]
    pub restate_service_port: Option<u16>,
    #[serde(default)]
    pub external_restate_admin_url: Option<String>,
    #[serde(default)]
    pub external_restate_ingress_url: Option<String>,
    /// Additional environment variables forwarded to the runner child process
    /// on spawn (both exe and dev-mode paths). Useful for test runners that
    /// need a feature flag like `QONTINUI_SCRIPTED_OUTPUT=1` without requiring
    /// a supervisor restart.
    ///
    /// Applied after all hardcoded envs, so callers can override e.g.
    /// `QONTINUI_API_URL` if they need to point a temp runner at a different
    /// backend. Not persisted across supervisor restarts for temp runners
    /// (they're ephemeral); for named runners it IS persisted via the
    /// settings file.
    #[serde(default)]
    pub extra_env: std::collections::HashMap<String, String>,
}

impl RunnerConfig {
    /// Classify this runner.
    ///
    /// Combines the legacy `is_primary` boolean (which wins if true) with
    /// the id-prefix scheme handled by [`RunnerKind::from_id`]. For
    /// `RunnerKind::Named`, the user-friendly display name from
    /// [`RunnerConfig::name`] is mirrored into the variant rather than the
    /// raw `named-{port}-{uuid}` id, since that's what callers actually want
    /// for UI/logs.
    #[allow(dead_code)] // Item 2: helper exposed for follow-up migration of `is_primary` checks.
    pub fn kind(&self) -> qontinui_types::wire::runner_kind::RunnerKind {
        use qontinui_types::wire::runner_kind::RunnerKind;
        if self.is_primary {
            return RunnerKind::Primary;
        }
        match RunnerKind::from_id(&self.id) {
            // Override Named's name with the friendly display name.
            RunnerKind::Named { .. } => RunnerKind::Named {
                name: self.name.clone(),
            },
            other => other,
        }
    }

    /// Create the default primary runner config.
    pub fn default_primary() -> Self {
        Self {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            port: DEFAULT_RUNNER_API_PORT,
            is_primary: true,
            protected: true,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: std::collections::HashMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

// Port constants
pub const DEFAULT_SUPERVISOR_PORT: u16 = 9875;
pub const DEFAULT_RUNNER_API_PORT: u16 = 9876;
/// Backward compat alias
pub const RUNNER_API_PORT: u16 = DEFAULT_RUNNER_API_PORT;
pub const EXPO_PORT: u16 = 8081;

// Process constants
/// How long to wait for a runner to exit on its own after we POST the
/// graceful close-request endpoint, before falling through to child.kill().
/// Gives the runner's WindowEvent::CloseRequested handler time to run
/// teardown hooks (e.g. UsbTransport::release_all releasing adb forwards).
pub const RUNNER_GRACEFUL_STOP_TIMEOUT_MS: u64 = 3000;
/// Per-request timeout for the graceful close POST itself. Short because
/// the endpoint returns as soon as the event is queued — if it hangs, the
/// runner is already unhealthy and we want to fall through to kill quickly.
pub const RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS: u64 = 500;
const DEFAULT_BUILD_TIMEOUT_SECS: u64 = 1800; // 30 minutes — cold Tauri builds on Windows can run 11+ min

/// Resolved cargo build timeout in seconds, read from
/// `QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS` env var at first access.
/// Clamped to [60, 7200], defaults to 1800 (30 minutes).
pub fn build_timeout_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        let raw = std::env::var("QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS").ok();
        match raw {
            None => DEFAULT_BUILD_TIMEOUT_SECS,
            Some(ref s) => match s.parse::<u64>() {
                Ok(n) => n.clamp(60, 7200),
                Err(_) => {
                    tracing::warn!(
                        env_var = "QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS",
                        value = s.as_str(),
                        default = DEFAULT_BUILD_TIMEOUT_SECS,
                        "invalid value for env var, using default"
                    );
                    DEFAULT_BUILD_TIMEOUT_SECS
                }
            },
        }
    })
}
#[allow(dead_code)]
pub const PORT_WAIT_TIMEOUT_SECS: u64 = 120;
pub const PORT_CHECK_INTERVAL_MS: u64 = 500;

// Log constants
const DEFAULT_LOG_BUFFER_SIZE: usize = 500;
/// Default cap for the build-only log buffer. Cargo output is dense
/// (thousands of lines per rebuild), so this is intentionally much larger
/// than the supervisor-events buffer to keep the prior build's output
/// available alongside the current one. Override via
/// `QONTINUI_SUPERVISOR_BUILD_LOG_BUFFER_SIZE`.
const DEFAULT_BUILD_LOG_BUFFER_SIZE: usize = 5000;

/// Resolved log buffer size, read from `QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE`
/// env var at first access. Clamped to [100, 10000], defaults to 500.
pub fn log_buffer_size() -> usize {
    use std::sync::OnceLock;
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.clamp(100, 10000))
            .unwrap_or(DEFAULT_LOG_BUFFER_SIZE)
    })
}

/// Resolved build-only log buffer size, read from
/// `QONTINUI_SUPERVISOR_BUILD_LOG_BUFFER_SIZE` env var at first access.
/// Clamped to [500, 50000], defaults to 5000.
///
/// Build output (`LogSource::Build`) is segregated into its own buffer so a
/// dense cargo rebuild (thousands of lines) does not evict supervisor-side
/// events (placement preview HTTP traces, spawn lifecycle records, expo
/// status, etc.) from the main 500-cap buffer. See `LogState` in
/// `log_capture.rs`.
pub fn build_log_buffer_size() -> usize {
    use std::sync::OnceLock;
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("QONTINUI_SUPERVISOR_BUILD_LOG_BUFFER_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.clamp(500, 50000))
            .unwrap_or(DEFAULT_BUILD_LOG_BUFFER_SIZE)
    })
}

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

/// Resolve the full model ID string from a (provider, key) pair.
/// Returns `None` if the combination is not found in `AI_MODELS`.
pub fn resolve_model_id(provider: &str, model_key: &str) -> Option<String> {
    AI_MODELS
        .iter()
        .find(|(p, k, _, _)| *p == provider && *k == model_key)
        .map(|(_, _, model_id, _)| model_id.to_string())
}

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

        let no_prewarm = args.no_prewarm
            || std::env::var("QONTINUI_SUPERVISOR_NO_PREWARM")
                .ok()
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

        // Honor the env var for headless CI boxes.
        let no_webview = args.no_webview
            || std::env::var("QONTINUI_SUPERVISOR_NO_WEBVIEW")
                .ok()
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

        // Resolve effective supervisor log file:
        //   1. explicit --log-file
        //   2. --log-dir/supervisor.log
        //   3. None (no persistent logging)
        let log_file = args
            .log_file
            .clone()
            .or_else(|| args.log_dir.as_ref().map(|d| d.join("supervisor.log")));

        SupervisorConfig {
            project_dir: args.project_dir,
            watchdog_enabled_at_start: args.watchdog,
            auto_start,
            auto_debug: args.auto_debug,
            log_file,
            log_dir: args.log_dir,
            port: args.port,
            dev_logs_dir,
            cli_args,
            expo_dir: args.expo_dir,
            expo_port: EXPO_PORT,
            // Default: single primary runner; settings may override later
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig::default(),
            no_prewarm,
            no_webview,
        }
    }

    /// Target directory for a given build slot.
    ///
    /// Each slot gets its own `target-pool/slot-{k}/` under the runner npm dir
    /// (workspace root). Cargo respects `CARGO_TARGET_DIR` and writes all
    /// artifacts here, so slots never contend on the same `target/`.
    pub fn runner_slot_target_dir(&self, slot_id: usize) -> PathBuf {
        self.runner_npm_dir()
            .join("target-pool")
            .join(format!("slot-{}", slot_id))
    }

    /// Path to the runner executable inside a specific slot's target dir.
    /// Used by the binary copy step after a per-slot cargo build completes.
    pub fn runner_exe_path_for_slot(&self, slot_id: usize) -> PathBuf {
        self.runner_slot_target_dir(slot_id)
            .join("debug")
            .join("qontinui-runner.exe")
    }

    /// Last-known-good directory under the build pool. Holds a copy of the
    /// most recent successfully built runner exe plus a `lkg.json` sidecar
    /// describing when it was built and which slot it came from. Survives
    /// any subsequent failed build that overwrites or deletes a slot's exe.
    pub fn lkg_dir(&self) -> PathBuf {
        self.runner_npm_dir().join("target-pool").join("lkg")
    }

    /// Path to the LKG runner exe. The file is replaced atomically on every
    /// successful build via copy-to-temp + rename.
    pub fn lkg_exe_path(&self) -> PathBuf {
        self.lkg_dir().join("qontinui-runner.exe")
    }

    /// Path to the LKG metadata sidecar (`built_at`, `source_slot`, `exe_size`).
    /// Loaded at supervisor startup so the in-memory `last_known_good` field
    /// survives restarts.
    pub fn lkg_metadata_path(&self) -> PathBuf {
        self.lkg_dir().join("lkg.json")
    }

    /// Path to the runner executable (for exe mode).
    ///
    /// Cargo builds into the workspace root's target directory (parent of
    /// src-tauri), not the package directory's target. `build_monitor::run_cargo_build`
    /// runs `cargo build --bin qontinui-runner` (no `--release` flag), so the
    /// fresh artifact lives under `target/debug/`, not `target/release/`.
    /// Pointing this at release caused `spawn-test {rebuild:true}` to rebuild
    /// debug and then silently launch a stale release binary.
    pub fn runner_exe_path(&self) -> PathBuf {
        self.runner_npm_dir()
            .join("target")
            .join("debug")
            .join("qontinui-runner.exe")
    }

    /// Path to a copied runner executable for non-primary runners.
    /// This avoids locking the main build artifact so dev-mode rebuilds succeed.
    /// Lives alongside the source exe under `target/debug/` so it picks up the
    /// same incremental build outputs (DLLs, PDBs, etc.) as the original.
    pub fn runner_exe_copy_path(&self, runner_id: &str) -> PathBuf {
        self.runner_npm_dir()
            .join("target")
            .join("debug")
            .join(format!("qontinui-runner-{}.exe", runner_id))
    }

    /// Path to the runner npm project root (parent of src-tauri).
    ///
    /// Always returns an absolute path. When the supervisor was
    /// launched with a relative `--project-dir` (e.g. `../qontinui-runner/src-tauri`),
    /// the parent resolves to `../qontinui-runner` — still relative. If that
    /// relative path is later passed to cargo via `CARGO_TARGET_DIR`, cargo
    /// resolves it from its own CWD (`src-tauri`), producing a double-nested
    /// path like `qontinui-runner/qontinui-runner/target-pool/slot-0/`. The
    /// `canonicalize()` call prevents this by expanding to an absolute path
    /// at the first call site.
    ///
    /// On Windows, `std::fs::canonicalize` returns verbatim paths with the
    /// `\\?\` UNC prefix. Third-party build scripts (notably `libusb1-sys`)
    /// panic when that prefix appears in `CARGO_TARGET_DIR`. Strip it so
    /// the returned path is a plain absolute Windows path.
    pub fn runner_npm_dir(&self) -> PathBuf {
        let npm = self
            .project_dir
            .parent()
            .unwrap_or(&self.project_dir)
            .to_path_buf();
        let canonical = npm.canonicalize().unwrap_or(npm);
        strip_verbatim_prefix(canonical)
    }
}

/// Strip Windows' `\\?\` verbatim prefix from a path when it represents a
/// simple absolute path (drive-letter root, no reserved characters). Returns
/// the input unchanged on non-Windows platforms or when the prefix is
/// genuinely needed (UNC paths, long paths where short form would collide).
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    match path.to_str() {
        // UNC (`\\?\UNC\server\share\...`) MUST keep the prefix — stripping
        // it yields `UNC\...` which isn't a valid path.
        Some(s) if s.starts_with(VERBATIM_UNC) => path,
        Some(s) => match s.strip_prefix(VERBATIM) {
            Some(stripped) => PathBuf::from(stripped),
            None => path,
        },
        None => path,
    }
}

#[cfg(not(windows))]
#[inline]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
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
    fn test_expo_port() {
        assert_eq!(EXPO_PORT, 8081);
    }

    // --- Process constant tests ---

    #[test]
    fn test_build_timeout_default_is_30_minutes() {
        assert_eq!(DEFAULT_BUILD_TIMEOUT_SECS, 1800);
    }

    #[test]
    fn test_build_timeout_resolves() {
        // No env override in unit-test env → returns the default.
        // (Resolved value is memoized; this test still exercises the parse path.)
        let resolved = build_timeout_secs();
        assert!((60..=7200).contains(&resolved));
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

    #[cfg(windows)]
    #[test]
    fn test_strip_verbatim_prefix_plain_drive_path() {
        let p = PathBuf::from(r"\\?\D:\qontinui-root\qontinui-runner");
        assert_eq!(
            strip_verbatim_prefix(p),
            PathBuf::from(r"D:\qontinui-root\qontinui-runner"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_strip_verbatim_prefix_preserves_unc() {
        let p = PathBuf::from(r"\\?\UNC\server\share\dir");
        assert_eq!(strip_verbatim_prefix(p.clone()), p);
    }

    #[cfg(windows)]
    #[test]
    fn test_strip_verbatim_prefix_no_prefix_unchanged() {
        let p = PathBuf::from(r"D:\some\path");
        assert_eq!(strip_verbatim_prefix(p.clone()), p);
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
            watchdog,
            auto_start,
            log_file: None,
            log_dir: None,
            port: DEFAULT_SUPERVISOR_PORT,
            auto_debug: false,
            expo_dir: None,
            no_prewarm: false,
            no_webview: false,
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
        assert!(!config.watchdog_enabled_at_start);
        assert!(!config.auto_start);
        assert!(!config.auto_debug);
        assert_eq!(config.port, DEFAULT_SUPERVISOR_PORT);
        assert_eq!(config.expo_port, EXPO_PORT);
        assert!(config.expo_dir.is_none());
        // Default single primary runner
        assert_eq!(config.runners.len(), 1);
        assert_eq!(config.runners[0].id, "primary");
        assert!(config.runners[0].is_primary);
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

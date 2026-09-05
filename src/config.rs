use clap::Parser;
use qontinui_types::wire::runner_kind::RunnerKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Runner binary filename for the current platform: `qontinui-runner.exe` on
/// Windows, `qontinui-runner` on Unix. Build/spawn/LKG/footprint paths must use
/// this (or [`std::env::consts::EXE_SUFFIX`] for derived names) instead of a
/// hardcoded `.exe`. The runner artifact has no extension on macOS/Linux, so the
/// old literal made the supervisor fail to locate a freshly-built runner there
/// (LKG promotion + `resolve_source_exe` both came up empty).
#[cfg(windows)]
pub const RUNNER_BIN_NAME: &str = "qontinui-runner.exe";
#[cfg(not(windows))]
pub const RUNNER_BIN_NAME: &str = "qontinui-runner";

/// Cargo's own target-dir override env var. Read (never written) by the
/// supervisor so exe resolution lands on the directory cargo actually wrote to.
pub const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// Which level of cargo's target-dir precedence produced a resolved path.
///
/// Cargo picks its target directory in this order (highest first), and so must
/// anything that wants to *find* what cargo built:
///
/// 1. `CARGO_TARGET_DIR` in the environment,
/// 2. `build.target-dir` from the `.cargo/config.toml` hierarchy that applies
///    at cargo's working directory,
/// 3. the workspace root's `target/`.
///
/// **Why this exists.** `runner_exe_path()` used to hardcode level 3. Every
/// build on this fleet exports
/// `CARGO_TARGET_DIR=<runner>/src-tauri/target` (cargo-guard.sh, the dev docs,
/// and every agent instructed to reuse the warm shared target dir), so cargo
/// wrote level 1 while the supervisor read level 3 — and level 3 still held a
/// months-old artifact from before the convention. Measured 2026-08-06 on the
/// fleet box:
///
/// ```text
/// qontinui-runner/target/debug/qontinui-runner.exe            2026-06-12 17:29  259,355,136 B
/// qontinui-runner/src-tauri/target/debug/qontinui-runner.exe  2026-08-06 02:54  340,434,432 B
/// ```
///
/// A `spawn-test {rebuild:false}` therefore launched a 54-day-old binary that
/// came up healthy and served the UI Bridge — a false green in the verification
/// path. Repointing the constant at `src-tauri/target` would have inverted the
/// bug onto every environment that sets no override, which is why resolution
/// follows cargo's precedence instead of picking a different fixed level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetDirSource {
    /// Level 1 — `CARGO_TARGET_DIR` was set in the environment.
    CargoTargetDirEnv,
    /// Level 2 — `build.target-dir` from an applicable `.cargo/config.toml`.
    CargoConfigBuildTargetDir,
    /// Level 3 — cargo's default: the workspace root's `target/`.
    WorkspaceDefault,
}

impl TargetDirSource {
    /// Stable machine-readable label, mirrored in API responses and logs.
    pub fn label(self) -> &'static str {
        match self {
            TargetDirSource::CargoTargetDirEnv => "cargo_target_dir_env",
            TargetDirSource::CargoConfigBuildTargetDir => "cargo_config_build_target_dir",
            TargetDirSource::WorkspaceDefault => "workspace_default",
        }
    }
}

/// Resolve `p` against `base` when it is relative. Cargo resolves a relative
/// `CARGO_TARGET_DIR` against its own working directory and a relative
/// `build.target-dir` against the directory *containing* the `.cargo` dir the
/// value was read from — hence the caller-supplied base.
fn absolutize(p: &Path, base: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Build the ordered target-dir candidate list for a cargo invocation, in
/// cargo's own precedence order. Pure — every input is injected, so the
/// precedence rule is unit-testable without touching the environment or the
/// filesystem.
///
/// - `cargo_cwd` — the directory cargo is invoked from (the runner's
///   `src-tauri`). Base for a relative `CARGO_TARGET_DIR`.
/// - `workspace_root` — the cargo workspace root (the runner npm dir), whose
///   `target/` is cargo's default.
/// - `env_target_dir` — the raw `CARGO_TARGET_DIR` value, if set. Empty /
///   whitespace-only is treated as unset (that is how cargo reads it too).
/// - `config_target_dir` — `(value, base)` from the applicable
///   `.cargo/config.toml`, if any.
///
/// The workspace default is ALWAYS the last candidate, so the list is never
/// empty and an environment with no override keeps today's behavior exactly.
pub fn target_dir_candidates(
    cargo_cwd: &Path,
    workspace_root: &Path,
    env_target_dir: Option<&str>,
    config_target_dir: Option<(&Path, &Path)>,
) -> Vec<(TargetDirSource, PathBuf)> {
    let mut out: Vec<(TargetDirSource, PathBuf)> = Vec::with_capacity(3);
    if let Some(v) = env_target_dir.map(str::trim).filter(|v| !v.is_empty()) {
        out.push((
            TargetDirSource::CargoTargetDirEnv,
            absolutize(Path::new(v), cargo_cwd),
        ));
    }
    if let Some((value, base)) = config_target_dir {
        out.push((
            TargetDirSource::CargoConfigBuildTargetDir,
            absolutize(value, base),
        ));
    }
    out.push((
        TargetDirSource::WorkspaceDefault,
        workspace_root.join("target"),
    ));
    out
}

/// Parse `build.target-dir` out of a `.cargo/config.toml` body. Returns `None`
/// when the file does not set it (or is not parseable — an unreadable config is
/// "no opinion", never an error, because cargo itself would still build).
pub fn parse_build_target_dir(toml_text: &str) -> Option<PathBuf> {
    let value: toml::Value = toml::from_str(toml_text).ok()?;
    let s = value.get("build")?.get("target-dir")?.as_str()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// Walk the `.cargo/config.toml` hierarchy at and above `start_dir` looking for
/// `build.target-dir`, exactly as cargo does — the deepest file wins, and both
/// the modern `config.toml` and the legacy extensionless `config` name are
/// honoured (cargo still reads the latter).
///
/// Returns `(value, base_dir)` where `base_dir` is the directory that CONTAINS
/// the `.cargo` dir — the anchor cargo resolves a relative value against.
pub fn read_cargo_config_target_dir(start_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    for dir in start_dir.ancestors() {
        for name in ["config.toml", "config"] {
            let path = dir.join(".cargo").join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(value) = parse_build_target_dir(&text) {
                return Some((value, dir.to_path_buf()));
            }
        }
    }
    None
}

/// Pick the winning exe candidate: the first one that exists, in precedence
/// order. When none exist, the highest-precedence candidate is returned so
/// error messages name the path cargo would have written to rather than a
/// lower-precedence path nobody uses.
///
/// Existence is injected so the choice is unit-testable with no filesystem.
///
/// **Why first-EXISTING rather than first-declared:** the supervisor's own
/// process may carry a `CARGO_TARGET_DIR` meant for something else entirely
/// (an agent's shell exports it for the supervisor's own builds). Requiring the
/// runner exe to actually be there makes a mis-aimed override degrade to the
/// next candidate instead of resolving to a path that will never hold a runner.
pub fn pick_exe_candidate<F: Fn(&Path) -> bool>(
    candidates: &[(TargetDirSource, PathBuf)],
    exists: F,
) -> (TargetDirSource, PathBuf) {
    if let Some((src, path)) = candidates.iter().find(|(_, p)| exists(p)) {
        return (*src, path.clone());
    }
    candidates
        .first()
        .cloned()
        .expect("target_dir_candidates always yields the workspace default")
}

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

/// Default minimum free disk (GB) below which the pre-permit disk guard
/// refuses a build. See [`min_free_disk_gb`].
pub const DEFAULT_MIN_FREE_DISK_GB: u64 = 30;

/// Env var overriding the pre-permit disk guard threshold, in GB.
pub const MIN_FREE_DISK_GB_ENV: &str = "QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB";

/// Minimum free disk (in GB) required before a build may acquire a build-pool
/// permit. Resolved from [`MIN_FREE_DISK_GB_ENV`], falling back to
/// [`DEFAULT_MIN_FREE_DISK_GB`]. Parse-with-default exactly like
/// [`BuildPoolConfig::default`]: a malformed value yields the default. A `0`
/// disables the guard entirely (treat every disk state as sufficient).
pub fn min_free_disk_gb() -> u64 {
    std::env::var(MIN_FREE_DISK_GB_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MIN_FREE_DISK_GB)
}

/// Default minimum free COMMIT (GB) below which the pre-permit memory guard
/// DEFERS a build. Mirrors `cargo-guard.sh`'s `MIN_FREE_GB=5` and `ci_node`'s
/// `MIN_FREE_RAM_GB`, so the supervisor lane now applies the same floor the
/// agent lane and the CI lane already did — it was the one build path without
/// one.
///
/// Why this exists: the runner's bin crate needs several GiB in a SINGLE rustc.
/// Starting it into a starved box does not fail cleanly — rustc aborts with
/// `memory allocation of N bytes failed` / `STATUS_STACK_BUFFER_OVERRUN`
/// (`0xc0000409`, Rust's `__fastfail` abort path, NOT a real buffer overrun),
/// **and that abort corrupts the slot's incremental cache**, so the next build
/// restarts from scratch and needs even MORE memory than the one that just
/// died. The failure is self-perpetuating, which is why a too-eager start is
/// worse than waiting. Observed 8x on the MSI box between 2026-07-23 and
/// 2026-07-30; the 07-30 instance is what took the runner build offline.
pub const DEFAULT_MIN_FREE_RAM_GB: u64 = 5;

/// Env var overriding the pre-permit memory guard threshold, in GB.
pub const MIN_FREE_RAM_GB_ENV: &str = "QONTINUI_SUPERVISOR_MIN_FREE_RAM_GB";

/// Minimum free commit (in GB) required before a build may acquire a build-pool
/// permit. Resolved from [`MIN_FREE_RAM_GB_ENV`], falling back to
/// [`DEFAULT_MIN_FREE_RAM_GB`]. Parse-with-default exactly like
/// [`min_free_disk_gb`]. A `0` disables the guard entirely.
pub fn min_free_ram_gb() -> u64 {
    std::env::var(MIN_FREE_RAM_GB_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MIN_FREE_RAM_GB)
}

/// Default minimum free **PHYSICAL** memory (GB) below which the pre-permit
/// memory guard DEFERS a build. A SECOND, independent floor beside
/// [`DEFAULT_MIN_FREE_RAM_GB`] — not a replacement for it — because on this
/// fleet the two pools are exhausted independently and a commit-only gate is
/// blind to half of it.
///
/// Measured on the MSI box 2026-08-29: free commit **30 GB** (six times the
/// commit floor, so that arm passed instantly) while free physical was
/// **629 MB**; one `rustc` held 7.4 GB resident and `vmmemWSL` 9.6 GB of a
/// 32.5 GB box. Two builds died — `os error 1455` (`ERROR_COMMITMENT_LIMIT`),
/// which cascaded into ~170 **bogus** compile errors in unrelated crates, then
/// `memory allocation of 1835008 bytes failed` → `rust_oom` → `0xc0000409`.
///
/// **3 GiB, not something larger, on purpose: the floor must be REACHABLE.**
/// The guard sleeps up to [`DEFAULT_MEM_WAIT_MAX_SECS`] and then builds anyway,
/// so an unreachable floor buys no protection and only delays every build by
/// the full window. Separately measured on that box: it **idles at 4.9-8.8 GB**
/// free physical and **died at 0.36-0.63 GB**. 3 clears the death band with
/// margin and still sits below the idle band, so it is reachable at rest. This
/// is a "do not ADD load to a thrashing box" gate, not a promise that a 7 GB
/// `rustc` will fit.
///
/// Mirrors `cargo-guard.sh`'s `MIN_FREE_PHYS_GB` (qontinui-claude-config PR
/// #428, 2026-08-29), which chose the same value for the same measurements —
/// but nothing in this repo can observe that lane, so this comment states the
/// shared derivation rather than claiming a pinned invariant.
pub const DEFAULT_MIN_FREE_PHYS_GB: u64 = 3;

/// Env var overriding the pre-permit memory guard's PHYSICAL threshold, in GB.
pub const MIN_FREE_PHYS_GB_ENV: &str = "QONTINUI_SUPERVISOR_MIN_FREE_PHYS_GB";

/// Minimum free physical memory (in GB) required before a build may acquire a
/// build-pool permit. Resolved from [`MIN_FREE_PHYS_GB_ENV`], falling back to
/// [`DEFAULT_MIN_FREE_PHYS_GB`]. Parse-with-default exactly like
/// [`min_free_ram_gb`].
///
/// A `0` disables the **physical arm only**, leaving the commit floor intact —
/// the two arms are separately disableable because they answer separately
/// (`cargo-guard.sh` documents the same split for
/// `CARGO_GUARD_MIN_FREE_PHYS_GB`).
pub fn min_free_phys_gb() -> u64 {
    std::env::var(MIN_FREE_PHYS_GB_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MIN_FREE_PHYS_GB)
}

/// Default cap (seconds) on how long the memory guard defers before building
/// anyway. Mirrors `cargo-guard.sh`'s `MEM_WAIT_MAX=900`.
pub const DEFAULT_MEM_WAIT_MAX_SECS: u64 = 900;

/// Env var overriding the memory guard's maximum defer window, in seconds.
pub const MEM_WAIT_MAX_SECS_ENV: &str = "QONTINUI_SUPERVISOR_MEM_WAIT_MAX_SECS";

/// Maximum seconds the memory guard will defer a build waiting for headroom
/// before proceeding anyway. Bounded so a mis-measuring box can never deadlock
/// the build lane.
pub fn mem_wait_max_secs() -> u64 {
    std::env::var(MEM_WAIT_MAX_SECS_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MEM_WAIT_MAX_SECS)
}

/// Configuration for a single managed runner instance.
///
/// The canonical discriminator is the `kind` field (a [`RunnerKind`]).
/// Pre-migration on-disk shapes (with `is_primary: bool` and no `kind`) are
/// rewritten in place at startup by `settings::migrate_settings`, so this
/// struct only ever sees the post-migration shape. The
/// `#[serde(default = "default_kind")]` attribute is retained so test fixtures
/// constructed from JSON literals without an explicit `kind` still deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub id: String,
    pub name: String,
    pub port: u16,
    /// Canonical runner classifier.
    #[serde(default = "default_kind")]
    pub kind: RunnerKind,
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
    /// Mirrors the friendly display name from [`RunnerConfig::name`] onto the
    /// `Named { name }` variant so callers don't have to reach back into
    /// `RunnerConfig` for the name (the raw id carries the
    /// `named-{port}-{uuid}` form, not what UIs/logs want).
    pub fn kind(&self) -> RunnerKind {
        match &self.kind {
            RunnerKind::Named { .. } => RunnerKind::Named {
                name: self.name.clone(),
            },
            other => other.clone(),
        }
    }

    /// Create the default primary runner config.
    pub fn default_primary() -> Self {
        Self {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            port: DEFAULT_RUNNER_API_PORT,
            kind: RunnerKind::Primary,
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

fn default_kind() -> RunnerKind {
    RunnerKind::Primary
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
// --- Cargo build watchdogs ---
//
// A cargo build is bounded by TWO budgets, not one wall-clock cap:
//
//   * [`build_no_progress_timeout_secs`] — the real watchdog. It fires only
//     after the build has produced NOTHING (no new cargo output, no new
//     artifact under the slot's `CARGO_TARGET_DIR`) for that long. A build
//     that is compiling is never killed by it, however slow the box is.
//   * [`build_absolute_timeout_secs`] — a pure backstop for a build that is
//     somehow producing churn forever. Deliberately far above any real build.
//
// Why the single wall-clock cap had to go. It was raised 1800 → 5400 on
// 2026-07-31 because measured cold `spawn-test {rebuild:true}` builds on this
// box are 2382s / 2974s. 5400 then killed a build at 2650 of ~2800 compile
// units with rustc actively working (2026-08-03) — the box was carrying 6-7
// concurrent peer cargo builds, so the cap was measuring LOAD, not a stuck
// build. And the kill is not merely a lost build: the failure was classified
// environmental, the slot's target dir was wiped, and the retry therefore
// started COLD and took even longer. A wall-clock cap on a shared box is a
// self-reinforcing loop; "has it made progress recently" is the question that
// actually distinguishes a wedged build from a slow one.
const DEFAULT_BUILD_NO_PROGRESS_SECS: u64 = 1200;

/// Backstop ceiling. 6h — an order of magnitude above the slowest observed
/// real build (2974s), so it can only catch a pathological build that keeps
/// emitting output forever. It must NEVER be the budget that ends a normal
/// build; that is the no-progress watchdog's job.
const DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS: u64 = 21600;

/// Env var for the no-progress watchdog budget.
pub const BUILD_NO_PROGRESS_SECS_ENV: &str = "QONTINUI_SUPERVISOR_BUILD_NO_PROGRESS_SECS";
/// Env var for the absolute backstop. Keeps its historical name so an existing
/// operator override keeps working — but it now bounds TOTAL wall-clock as a
/// backstop, and is no longer what ends a slow-but-progressing build.
pub const BUILD_ABSOLUTE_TIMEOUT_SECS_ENV: &str = "QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS";

/// Resolve `env_var` as a `u64`, clamped to `[lo, hi]`, falling back to
/// `default` when unset or unparseable (warning loudly on the latter).
fn env_secs_or(env_var: &str, default: u64, lo: u64, hi: u64) -> u64 {
    match std::env::var(env_var).ok() {
        None => default,
        Some(ref s) => match s.parse::<u64>() {
            Ok(n) => n.clamp(lo, hi),
            Err(_) => {
                tracing::warn!(
                    env_var,
                    value = s.as_str(),
                    default,
                    "invalid value for env var, using default"
                );
                default
            }
        },
    }
}

/// Resolved NO-PROGRESS watchdog budget in seconds, read from
/// [`BUILD_NO_PROGRESS_SECS_ENV`] at first access. Clamped to [60, 7200],
/// defaults to 1200 (20 minutes of *complete silence* — no cargo output and no
/// new artifact in the slot's target dir).
pub fn build_no_progress_timeout_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        env_secs_or(
            BUILD_NO_PROGRESS_SECS_ENV,
            DEFAULT_BUILD_NO_PROGRESS_SECS,
            60,
            7200,
        )
    })
}

/// Resolved ABSOLUTE backstop in seconds, read from
/// [`BUILD_ABSOLUTE_TIMEOUT_SECS_ENV`] at first access. Clamped to
/// [300, 86400], defaults to 21600 (6 hours).
pub fn build_absolute_timeout_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        env_secs_or(
            BUILD_ABSOLUTE_TIMEOUT_SECS_ENV,
            DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS,
            300,
            86400,
        )
    })
}
const DEFAULT_PNPM_TIMEOUT_SECS: u64 = 1200; // 20 minutes — cold pnpm install + frontend build
const DEFAULT_GIT_TIMEOUT_SECS: u64 = 30; // git rev-parse / diff are fast; a hang means trouble

/// Resolved frontend (pnpm) build timeout in seconds, read from
/// `QONTINUI_SUPERVISOR_PNPM_TIMEOUT_SECS` at first access. Clamped to
/// [30, 3600], defaults to 1200 (20 minutes). Distinct from
/// the cargo budgets ([`build_no_progress_timeout_secs`] /
/// [`build_absolute_timeout_secs`]) because pnpm install+build has a very
/// different runtime profile; the frontend build had NO timeout before the
/// build-pipeline consolidation, which is exactly the hang this guards.
pub fn pnpm_timeout_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        let raw = std::env::var("QONTINUI_SUPERVISOR_PNPM_TIMEOUT_SECS").ok();
        match raw {
            None => DEFAULT_PNPM_TIMEOUT_SECS,
            Some(ref s) => match s.parse::<u64>() {
                Ok(n) => n.clamp(30, 3600),
                Err(_) => {
                    tracing::warn!(
                        env_var = "QONTINUI_SUPERVISOR_PNPM_TIMEOUT_SECS",
                        value = s.as_str(),
                        default = DEFAULT_PNPM_TIMEOUT_SECS,
                        "invalid value for env var, using default"
                    );
                    DEFAULT_PNPM_TIMEOUT_SECS
                }
            },
        }
    })
}

/// Resolved git-subprocess timeout in seconds, read from
/// `QONTINUI_SUPERVISOR_GIT_TIMEOUT_SECS` at first access. Clamped to
/// [5, 300], defaults to 30. Used for the best-effort `git rev-parse` calls
/// in `build_monitor` so a wedged git (network filesystem, lock contention)
/// can't hang a build forever.
pub fn git_timeout_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        let raw = std::env::var("QONTINUI_SUPERVISOR_GIT_TIMEOUT_SECS").ok();
        match raw {
            None => DEFAULT_GIT_TIMEOUT_SECS,
            Some(ref s) => match s.parse::<u64>() {
                Ok(n) => n.clamp(5, 300),
                Err(_) => {
                    tracing::warn!(
                        env_var = "QONTINUI_SUPERVISOR_GIT_TIMEOUT_SECS",
                        value = s.as_str(),
                        default = DEFAULT_GIT_TIMEOUT_SECS,
                        "invalid value for env var, using default"
                    );
                    DEFAULT_GIT_TIMEOUT_SECS
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

/// Default maximum entries retained in the post-mortem `stopped_runners`
/// cache (see `process::stopped_cache`). Bumped from the original 100 to
/// give agent-driven post-mortem queries (bulk crash-summary fetches hours
/// after a stale-spawn sweep) more headroom. Override via
/// `QONTINUI_SUPERVISOR_STOPPED_CACHE_CAP`.
const DEFAULT_STOPPED_CACHE_MAX_ENTRIES: usize = 1000;

/// Default TTL in seconds for entries in the post-mortem `stopped_runners`
/// cache. Bumped from the original 600s (10 min) so post-mortem queries
/// landing an hour after a crash still find the snapshot. Override via
/// `QONTINUI_SUPERVISOR_STOPPED_CACHE_TTL_SECS`. `i64` because chrono's
/// `Duration::num_seconds()` returns `i64` at the comparison call site
/// (`process::stopped_cache::insert_and_evict`).
const DEFAULT_STOPPED_CACHE_TTL_SECS: i64 = 3600;

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

/// Pure helper: read `env_var`, parse as `usize`, clamp to `[min, max]`,
/// fall back to `default` on missing/unparseable. Factored out so unit
/// tests can exercise the parse/clamp logic directly without hitting the
/// `OnceLock`-cached accessors above (which memoize their first read for
/// the lifetime of the process).
pub(crate) fn parse_clamped_usize(env_var: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(env_var)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default)
}

/// Pure helper: read `env_var`, parse as `i64`, clamp to `[min, max]`,
/// fall back to `default` on missing/unparseable. Sibling of
/// [`parse_clamped_usize`] for the i64-typed bounds (chrono::Duration's
/// `num_seconds()` returns `i64`).
pub(crate) fn parse_clamped_i64(env_var: &str, default: i64, min: i64, max: i64) -> i64 {
    std::env::var(env_var)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default)
}

/// Default upper bound on a **temp** runner's lifetime, in seconds: 24 hours.
///
/// Deliberately generous. The bound exists to catch the abandoned-orphan case
/// the 2026-08-08 incident evidenced (an unowned `requester_id: None` temp
/// still alive with 31 live PTYs **two days** after it was spawned), not to
/// police normal use — a temp runner outliving a working session is the
/// anomaly, and reaping one an operator is actively driving is the worse
/// failure. Override via `QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS`.
const DEFAULT_TEMP_RUNNER_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Resolved max-age bound for temp runners, read from
/// `QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS` at first access.
///
/// - unset / unparseable → the 24h default;
/// - `0` → **disabled**, `None`; no temp runner is ever reaped for age. The
///   explicit off-switch matters because this is the one sweep rule that kills
///   a *healthy* process, so an operator running a long-lived temp needs a way
///   to turn it off without a rebuild (same posture as
///   `QONTINUI_SUPERVISOR_MIN_FREE_RAM_GB=0` and
///   `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1`);
/// - anything else → clamped to `[3600, 604_800]` (1 hour – 7 days). The floor
///   is set ABOVE the observed cold-build ceiling: a cold
///   `spawn-test {rebuild:true}` runs 40–50 min (2382s and 2974s measured — the
///   same evidence that raised [`DEFAULT_BUILD_TIMEOUT_SECS`] to 5400s), so
///   3600s is the smallest floor at which a legal setting cannot reap a runner
///   that has only just finished building. The age itself is measured from
///   `RunnerState::started_at` (see
///   [`crate::process::manager::resolve_temp_runner_age`]), so build time is
///   not charged to the runner's lifetime either — floor and clock are two
///   independent guards against the same mistake.
///
/// Applies to `RunnerKind::Temp` only — see
/// `process::manager::reap_stale_test_runners`.
///
/// Memoized via `OnceLock`, so later env changes are ignored after the first
/// read. Tests drive [`parse_temp_runner_max_age`] directly to avoid the cache.
pub fn temp_runner_max_age() -> Option<std::time::Duration> {
    use std::sync::OnceLock;
    static AGE: OnceLock<Option<std::time::Duration>> = OnceLock::new();
    *AGE.get_or_init(|| {
        parse_temp_runner_max_age(
            std::env::var("QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS").ok(),
        )
    })
}

/// Default serving-restart threshold: 300 s of a HELD PORT WITH A SILENT API
/// before the serving watchdog will consider a restart.
///
/// Five minutes is chosen against the measured costs on both sides. A real
/// wedge has run 12 h to 5 days with zero recoveries, so ≤5 min plus one
/// restart is a rounding error against that. A 4-minute GC pause or a `/health`
/// tail (sampled to 10 120 ms on a loaded box) costs nothing — the 30 s wedge
/// escalation still logs, the restart simply does not fire.
const DEFAULT_SERVING_RESTART_AFTER_SECS: u64 = 300;

/// How long a runner's API must have been silent (with its port still held)
/// before the serving watchdog will restart it.
///
/// Env: `QONTINUI_SUPERVISOR_SERVING_RESTART_AFTER_SECS`. Default
/// [`DEFAULT_SERVING_RESTART_AFTER_SECS`], clamped to `[60, 86_400]`.
///
/// **`0` is NOT "disabled" here.** The off-switch is a separate variable
/// (`QONTINUI_SUPERVISOR_NO_SERVING_RESTART=1`), because a threshold that
/// doubles as a kill-switch means a fat-fingered `0` silently removes the
/// protection instead of setting a fast one.
///
/// The 60 s floor is not cosmetic: `UNRESPONSIVE_ESCALATION_TICKS × 2 s = 30 s`
/// must have elapsed (so the wedge has been escalated at least once) PLUS
/// `READINESS_TIMEOUT = 15 s` (so one full readiness probe has had time to
/// fail) before a restart can be correct. 60 s is the smallest round number
/// above that sum.
///
/// Memoized via `OnceLock`; tests drive [`parse_serving_restart_after`].
pub fn serving_restart_after_secs() -> u64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        parse_serving_restart_after(
            std::env::var("QONTINUI_SUPERVISOR_SERVING_RESTART_AFTER_SECS").ok(),
        )
    })
}

/// Pure parse/clamp core behind [`serving_restart_after_secs`].
pub(crate) fn parse_serving_restart_after(raw: Option<String>) -> u64 {
    const MIN_SECS: u64 = 60;
    const MAX_SECS: u64 = 24 * 60 * 60;
    match raw.as_deref().map(str::trim).map(str::parse::<u64>) {
        Some(Ok(n)) => n.clamp(MIN_SECS, MAX_SECS),
        // Missing OR unparseable → the default. An unreadable knob must never
        // silently disarm the watchdog, and `0` clamps UP to the floor rather
        // than meaning "immediately" or "never".
        _ => DEFAULT_SERVING_RESTART_AFTER_SECS,
    }
}

/// Pure decision core behind [`temp_runner_max_age`]; see it for the contract.
/// Takes the raw env value so the parse/clamp/disable logic is unit-testable
/// without the `OnceLock` (which memoizes its first read process-wide) and
/// without mutating the environment (which races every other test).
pub(crate) fn parse_temp_runner_max_age(raw: Option<String>) -> Option<std::time::Duration> {
    // Above the measured cold-build ceiling (2974s) — see the doc on
    // `temp_runner_max_age`. A lower floor would advertise a legal setting that
    // reaps a runner right after a cold `spawn-test {rebuild:true}` finishes.
    const MIN_SECS: u64 = 3600;
    const MAX_SECS: u64 = 7 * 24 * 60 * 60;
    let secs = match raw.as_deref().map(str::trim).map(str::parse::<u64>) {
        Some(Ok(0)) => return None,
        Some(Ok(n)) => n.clamp(MIN_SECS, MAX_SECS),
        // Missing OR unparseable → the default. An unreadable knob must not
        // silently disable the bound.
        _ => DEFAULT_TEMP_RUNNER_MAX_AGE_SECS,
    };
    Some(std::time::Duration::from_secs(secs))
}

/// Resolved maximum entries for the post-mortem `stopped_runners` cache
/// (see `process::stopped_cache`), read from
/// `QONTINUI_SUPERVISOR_STOPPED_CACHE_CAP` env var at first access.
/// Clamped to `[100, 100_000]`, defaults to 1000.
///
/// Memoized via `OnceLock`, so subsequent env-var changes are ignored
/// after the first read. Tests should drive [`parse_clamped_usize`]
/// directly to avoid the cache.
pub fn stopped_cache_max_entries() -> usize {
    use std::sync::OnceLock;
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        parse_clamped_usize(
            "QONTINUI_SUPERVISOR_STOPPED_CACHE_CAP",
            DEFAULT_STOPPED_CACHE_MAX_ENTRIES,
            100,
            100_000,
        )
    })
}

/// Resolved TTL (seconds) for entries in the post-mortem
/// `stopped_runners` cache (see `process::stopped_cache`), read from
/// `QONTINUI_SUPERVISOR_STOPPED_CACHE_TTL_SECS` env var at first access.
/// Clamped to `[60, 86_400]`, defaults to 3600 (60 min). Returns `i64`
/// because the consumer compares against `chrono::Duration::num_seconds()`.
///
/// Memoized via `OnceLock`, so subsequent env-var changes are ignored
/// after the first read. Tests should drive [`parse_clamped_i64`]
/// directly to avoid the cache.
pub fn stopped_cache_ttl_secs() -> i64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<i64> = OnceLock::new();
    *SECS.get_or_init(|| {
        parse_clamped_i64(
            "QONTINUI_SUPERVISOR_STOPPED_CACHE_TTL_SECS",
            DEFAULT_STOPPED_CACHE_TTL_SECS,
            60,
            86_400,
        )
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
            .join(RUNNER_BIN_NAME)
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
        self.lkg_dir().join(RUNNER_BIN_NAME)
    }

    /// Path to the LKG metadata sidecar (`built_at`, `source_slot`, `exe_size`,
    /// `sha`, `source`). Loaded at supervisor startup so the in-memory
    /// `last_known_good` field survives restarts.
    pub fn lkg_metadata_path(&self) -> PathBuf {
        self.lkg_dir().join("lkg.json")
    }

    /// The directory cargo is invoked from for runner builds (`src-tauri`),
    /// absolute. Base for a relative `CARGO_TARGET_DIR` and the starting point
    /// for the `.cargo/config.toml` hierarchy walk.
    pub fn runner_cargo_cwd(&self) -> PathBuf {
        let dir = self.project_dir.clone();
        let canonical = dir.canonicalize().unwrap_or(dir);
        strip_verbatim_prefix(canonical)
    }

    /// The ordered target-dir candidates for a runner build, in cargo's own
    /// precedence order (`CARGO_TARGET_DIR` → `build.target-dir` → workspace
    /// default). See [`TargetDirSource`] for why this is a ladder and not a
    /// constant.
    ///
    /// Reads the environment and the `.cargo/config.toml` hierarchy on every
    /// call — deliberately, so an operator who exports a different
    /// `CARGO_TARGET_DIR` and rebuilds does not need to restart the supervisor
    /// for resolution to follow. Both reads are cheap and off any hot path
    /// (resolution runs once per runner start).
    pub fn runner_target_dir_candidates(&self) -> Vec<(TargetDirSource, PathBuf)> {
        let cargo_cwd = self.runner_cargo_cwd();
        let env_value = std::env::var(CARGO_TARGET_DIR_ENV).ok();
        let config_value = read_cargo_config_target_dir(&cargo_cwd);
        target_dir_candidates(
            &cargo_cwd,
            &self.runner_npm_dir(),
            env_value.as_deref(),
            config_value
                .as_ref()
                .map(|(v, base)| (v.as_path(), base.as_path())),
        )
    }

    /// The same ladder as [`Self::runner_target_dir_candidates`], expressed as
    /// runner-exe paths (`<target-dir>/debug/<RUNNER_BIN_NAME>`).
    ///
    /// `build_monitor::run_cargo_build` runs `cargo build --bin qontinui-runner`
    /// with no `--release`, so the fresh artifact is always under `debug/`.
    /// Pointing this at release once caused `spawn-test {rebuild:true}` to
    /// rebuild debug and then silently launch a stale release binary.
    pub fn runner_exe_candidates(&self) -> Vec<(TargetDirSource, PathBuf)> {
        self.runner_target_dir_candidates()
            .into_iter()
            .map(|(src, dir)| (src, dir.join("debug").join(RUNNER_BIN_NAME)))
            .collect()
    }

    /// Resolve the non-pool runner exe, reporting WHICH precedence level won.
    ///
    /// Callers that surface the path to an operator (logs, spawn responses,
    /// refusal messages) should use this rather than [`Self::runner_exe_path`]:
    /// the whole reason the stale-binary incident went unnoticed is that
    /// nothing ever reported where the binary came from.
    pub fn runner_exe_path_resolved(&self) -> (TargetDirSource, PathBuf) {
        let candidates = self.runner_exe_candidates();
        pick_exe_candidate(&candidates, |p| p.exists())
    }

    /// Path to the runner executable (for exe mode), resolved through cargo's
    /// target-dir precedence. Convenience wrapper over
    /// [`Self::runner_exe_path_resolved`] for call sites that only need the path.
    pub fn runner_exe_path(&self) -> PathBuf {
        self.runner_exe_path_resolved().1
    }

    /// Path to a copied runner executable for non-primary runners.
    /// This avoids locking the main build artifact so dev-mode rebuilds succeed.
    /// Lives alongside the source exe under `target/debug/` so it picks up the
    /// same incremental build outputs (DLLs, PDBs, etc.) as the original.
    ///
    /// **Pool naming.** For `Temp` and `Named` runners the filename is keyed
    /// off the runner's port (`qontinui-runner-test-{port}.exe`,
    /// `qontinui-runner-named-{port}.exe`) rather than its unique id. This
    /// gives a stable, bounded set of filenames (23 test slots × the named
    /// port set) so Windows Firewall rules registered against those paths
    /// keep matching across spawn-test invocations. Without the pool naming,
    /// each spawn produced a new binary path (`qontinui-runner-test-{uuid}.exe`),
    /// every cold spawn triggered a "Allow this app through firewall?" prompt,
    /// and the install-firewall-rules.ps1 helper had to be re-run after
    /// every cargo build.
    ///
    /// `Primary` and `External` runners keep id-based names — primary's id is
    /// already a stable `"primary"`, and external runners are user-managed
    /// (the supervisor only observes them).
    pub fn runner_exe_copy_path(&self, config: &RunnerConfig) -> PathBuf {
        let sfx = std::env::consts::EXE_SUFFIX;
        let filename = match &config.kind {
            RunnerKind::Temp { .. } => format!("qontinui-runner-test-{}{}", config.port, sfx),
            RunnerKind::Named { .. } => format!("qontinui-runner-named-{}{}", config.port, sfx),
            RunnerKind::Primary => format!("qontinui-runner-{}{}", config.id, sfx),
            // `External` and any future `RunnerKind` variants (the enum is
            // `#[non_exhaustive]`) — fall back to the id-based name so
            // user-managed runners that the supervisor only observes keep
            // working without supervisor changes.
            _ => format!("qontinui-runner-{}{}", config.id, sfx),
        };
        self.runner_npm_dir()
            .join("target")
            .join("debug")
            .join(filename)
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

    // --- Serving-restart threshold ---

    #[test]
    fn serving_restart_after_defaults_and_never_silently_disarms() {
        assert_eq!(
            parse_serving_restart_after(None),
            DEFAULT_SERVING_RESTART_AFTER_SECS
        );
        // Unreadable knob → default, never "off" and never "immediately".
        for junk in ["", "  ", "abc", "-1", "12.5", "1e6"] {
            assert_eq!(
                parse_serving_restart_after(Some(junk.to_string())),
                DEFAULT_SERVING_RESTART_AFTER_SECS,
                "{junk:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn serving_restart_after_clamps_to_the_escalation_plus_probe_floor() {
        // `0` is NOT a disable — the off-switch is a separate env var. It
        // clamps UP to the floor, which sits above
        // UNRESPONSIVE_ESCALATION_TICKS*2s + READINESS_TIMEOUT = 45s.
        assert_eq!(parse_serving_restart_after(Some("0".to_string())), 60);
        assert_eq!(parse_serving_restart_after(Some("1".to_string())), 60);
        assert_eq!(parse_serving_restart_after(Some("45".to_string())), 60);
        assert_eq!(parse_serving_restart_after(Some("60".to_string())), 60);
        assert_eq!(parse_serving_restart_after(Some(" 900 ".to_string())), 900);
        assert_eq!(
            parse_serving_restart_after(Some("999999".to_string())),
            24 * 60 * 60
        );
    }

    // --- Temp-runner max-age bound ---

    /// Missing or unreadable knob → the default bound, never "disabled".
    /// An unparseable value silently turning the safety bound off is the
    /// failure mode worth pinning (`silent-empty-is-unknown`).
    #[test]
    fn temp_runner_max_age_defaults_to_24h_and_never_silently_disables() {
        let want = std::time::Duration::from_secs(DEFAULT_TEMP_RUNNER_MAX_AGE_SECS);
        assert_eq!(parse_temp_runner_max_age(None), Some(want));
        for junk in ["", "  ", "abc", "-1", "12.5", "1e6"] {
            assert_eq!(
                parse_temp_runner_max_age(Some(junk.to_string())),
                Some(want),
                "unparseable {junk:?} must fall back to the default bound, not disable it"
            );
        }
        assert_eq!(DEFAULT_TEMP_RUNNER_MAX_AGE_SECS, 86_400);
    }

    /// `0` is the explicit off-switch — the one way to get `None`.
    #[test]
    fn temp_runner_max_age_zero_disables_the_bound() {
        assert_eq!(parse_temp_runner_max_age(Some("0".to_string())), None);
        assert_eq!(parse_temp_runner_max_age(Some(" 0 ".to_string())), None);
    }

    /// Clamped to [1 hour, 7 days]. The floor sits ABOVE the measured cold
    /// `spawn-test {rebuild:true}` ceiling (2382s and 2974s observed — the same
    /// evidence behind `DEFAULT_BUILD_TIMEOUT_SECS = 5400`), so no legal
    /// setting can reap a runner that has only just finished building.
    #[test]
    fn temp_runner_max_age_clamps_to_a_safe_range() {
        assert_eq!(
            parse_temp_runner_max_age(Some("1".to_string())),
            Some(std::time::Duration::from_secs(3600))
        );
        assert_eq!(
            parse_temp_runner_max_age(Some("3599".to_string())),
            Some(std::time::Duration::from_secs(3600))
        );
        assert_eq!(
            parse_temp_runner_max_age(Some("7200".to_string())),
            Some(std::time::Duration::from_secs(7200))
        );
        assert_eq!(
            parse_temp_runner_max_age(Some(u64::MAX.to_string())),
            Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
        );
    }

    /// The floor must clear the slowest cold build the repo has measured,
    /// otherwise a legal setting reaps a runner the moment it finishes
    /// building. Tied to `DEFAULT_BUILD_TIMEOUT_SECS` so raising the build
    /// timeout without revisiting this floor fails here.
    #[test]
    fn temp_runner_max_age_floor_clears_the_observed_cold_build() {
        const OBSERVED_COLD_BUILD_SECS: u64 = 2974;
        let floor = parse_temp_runner_max_age(Some("1".to_string()))
            .expect("a non-zero setting must yield a bound");
        assert!(
            floor.as_secs() > OBSERVED_COLD_BUILD_SECS,
            "the max-age floor ({}s) must exceed the slowest observed cold \
             `spawn-test {{rebuild:true}}` ({OBSERVED_COLD_BUILD_SECS}s), or an operator can \
             legally configure a bound that reaps a runner right after it builds",
            floor.as_secs()
        );
    }

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
    fn test_build_watchdog_defaults() {
        // The no-progress budget is what actually ends a wedged build: 20
        // minutes of NO cargo output and NO new artifact. It must stay well
        // under the absolute backstop, or the backstop becomes the de-facto
        // wall-clock cap this design exists to remove.
        assert_eq!(DEFAULT_BUILD_NO_PROGRESS_SECS, 1200);
        assert_eq!(DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS, 21600);
        // Ordering + headroom expressed with `min`/`max` rather than a bare
        // `assert!(A < B)`: the latter is a constant expression, which clippy
        // (correctly) flags as an assertion that can never fail at runtime.
        assert_eq!(
            DEFAULT_BUILD_NO_PROGRESS_SECS.min(DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS),
            DEFAULT_BUILD_NO_PROGRESS_SECS,
            "the no-progress budget must be the tighter of the two budgets"
        );
        // The absolute backstop must clear the slowest observed real build
        // (2974s on this box) by a wide margin — it is a backstop, not a cap.
        assert_eq!(
            DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS.max(4 * 2974),
            DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS,
            "the absolute backstop must sit far above the slowest real build"
        );
    }

    #[test]
    fn test_build_watchdog_defaults_are_within_their_clamps() {
        // Each default must survive its own clamp, or the resolver would
        // silently return something other than the declared default.
        assert!((60..=7200).contains(&DEFAULT_BUILD_NO_PROGRESS_SECS));
        assert!((300..=86400).contains(&DEFAULT_BUILD_ABSOLUTE_TIMEOUT_SECS));
    }

    #[test]
    fn test_memory_guard_defaults() {
        // These are literals, and a literal is all this test can honestly
        // assert: it cannot see `cargo-guard.sh` or the runner's `ci_node`, so
        // it must not claim to pin them. (The comment here used to say it
        // mirrored both — and `ci_node`'s floor is 4, not 5, which nothing
        // detected because nothing could. Converging that lane is the runner's
        // half of §A3 of plan
        // `2026-08-02-fleet-resource-telemetry-and-ci-allocation`.)
        //
        // What IS pinned in-repo is the QUANTITY, by
        // `memory_guard_floor_and_published_sample_read_one_field` below.
        assert_eq!(DEFAULT_MIN_FREE_RAM_GB, 5);
        assert_eq!(DEFAULT_MEM_WAIT_MAX_SECS, 900);
        // Same honesty applies to the physical floor: this is a literal, and a
        // literal is all it asserts. It does NOT pin `cargo-guard.sh`'s
        // `MIN_FREE_PHYS_GB`, which happens to be 3 for the same measurements
        // and which nothing here can read.
        assert_eq!(DEFAULT_MIN_FREE_PHYS_GB, 3);
    }

    #[test]
    fn the_physical_floor_is_reachable_at_this_boxs_measured_idle() {
        // A REAL invariant, not a restated literal: an unreachable floor
        // protects nothing. `check_ram_guard` sleeps up to
        // `DEFAULT_MEM_WAIT_MAX_SECS` and then builds ANYWAY, so a floor above
        // the band the box idles in converts every build into a full-window
        // delay followed by the same build. Measured on the MSI box
        // 2026-08-29: idles at 4.9-8.8 GB free physical, died at 0.36-0.63 GB.
        // The floor must sit strictly inside that gap, at both ends.
        //
        // Both are `const` assertions on purpose: they are decidable at compile
        // time, so a future edit to the floor fails the BUILD rather than
        // waiting for someone to run the suite.
        const MEASURED_DEATH_BAND_CEILING_GB: u64 = 1; // 0.36-0.63 rounds under 1
        const MEASURED_IDLE_FLOOR_GB: u64 = 4; // 4.9 at the low end
        const {
            assert!(
                DEFAULT_MIN_FREE_PHYS_GB > MEASURED_DEATH_BAND_CEILING_GB,
                "the physical floor must clear the band where builds actually died"
            );
        }
        const {
            assert!(
                DEFAULT_MIN_FREE_PHYS_GB <= MEASURED_IDLE_FLOOR_GB,
                "the physical floor must be reachable at this box's measured idle, or \
                 every build eats the whole mem_wait_max window and then builds anyway"
            );
        }
    }

    #[test]
    fn the_physical_probe_is_windows_only_and_paired_with_its_total() {
        // The observable in-repo invariant for the new arm. Two halves:
        //
        // 1. Availability and total agree about whether this platform answers
        //    at all — a reading with no denominator cannot be rendered as
        //    "0.61 of 31.71 GB", which is the whole point of the pair.
        // 2. Off Windows the arm is INERT BY DESIGN, not broken: there
        //    `available_commit_bytes` already reads MemAvailable (physical
        //    available), so a second probe would apply two floors to one
        //    quantity. `None` there is the correct answer, and the guard's
        //    fail-open contract turns it into "this arm contributes nothing".
        let avail = crate::build_monitor::available_phys_bytes();
        let total = crate::build_monitor::total_phys_bytes();
        assert_eq!(
            avail.is_some(),
            total.is_some(),
            "the physical reading and its ceiling must come from the same probe"
        );
        if cfg!(windows) {
            assert!(
                avail.is_some(),
                "on Windows the physical probe must answer — a silently-None probe \
                 disarms this arm on exactly the box it exists to protect"
            );
        } else {
            assert!(
                avail.is_none(),
                "off Windows this arm must stay inert; available_commit_bytes already \
                 reads physical-available there"
            );
        }
    }

    #[test]
    fn memory_guard_floor_and_published_sample_read_one_field() {
        // The observable invariant this repo CAN assert: the floor
        // `check_ram_guard` enforces and the `commit_available_bytes` field
        // published in every resource sample are the same probe, not two
        // measurements that happen to agree today. A lane that drifted onto
        // physical-available memory (which on Windows is a different number
        // entirely) would fail here instead of silently rendering a headroom
        // figure no guard enforces.
        let guard_probe = crate::build_monitor::available_commit_bytes();
        let published = crate::footprint::memory_snapshot();
        assert_eq!(
            guard_probe.is_some(),
            published.commit_available_bytes.is_some(),
            "the guard's probe and the published sample field must be the same probe"
        );
        // Windows commit-available and physical-available are DIFFERENT
        // numbers, so the published pair must not collapse into one field:
        // that collapse is precisely how a lane drifts onto the wrong quantity
        // without anything noticing. (Values are not compared across the two
        // probes — a live counter moves between reads and that would flake;
        // what is pinned is that both fields exist and are sourced separately.)
        //
        // Physical-available is published too, but as its OWN named field — the
        // point of A3 is that a divergence is visible, not that it is erased.
        assert!(
            published.mem_available_bytes.is_some(),
            "mem_available_bytes must be published alongside, under its own name"
        );
    }

    #[test]
    fn test_memory_guard_resolvers_fall_back_to_defaults() {
        // Only assert the no-override path when the env is genuinely unset —
        // asserting unconditionally would fail for an operator who has set a
        // per-machine override, and reading an absent var as "default" is the
        // silent-empty-is-unknown trap.
        if std::env::var(MIN_FREE_RAM_GB_ENV).is_err() {
            assert_eq!(min_free_ram_gb(), DEFAULT_MIN_FREE_RAM_GB);
        }
        if std::env::var(MEM_WAIT_MAX_SECS_ENV).is_err() {
            assert_eq!(mem_wait_max_secs(), DEFAULT_MEM_WAIT_MAX_SECS);
        }
        if std::env::var(MIN_FREE_PHYS_GB_ENV).is_err() {
            assert_eq!(min_free_phys_gb(), DEFAULT_MIN_FREE_PHYS_GB);
        }
    }

    #[test]
    fn test_build_watchdog_budgets_resolve() {
        // No env override in unit-test env → returns the default.
        // (Resolved values are memoized; this still exercises the parse path.)
        assert!((60..=7200).contains(&build_no_progress_timeout_secs()));
        assert!((300..=86400).contains(&build_absolute_timeout_secs()));
    }

    #[test]
    fn test_env_secs_or_clamps_and_falls_back() {
        // Unset → default.
        assert_eq!(
            env_secs_or("QONTINUI_SUPERVISOR_NO_SUCH_VAR_FOR_TESTS", 42, 1, 100),
            42
        );
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
        assert!(config.runners[0].kind().is_primary());
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
        assert!(exe_path.ends_with(format!("target/debug/{}", RUNNER_BIN_NAME)));
    }

    // --- Cargo target-dir precedence (the stale-exe defect) ---
    //
    // Every case drives the pure core with injected inputs: no env mutation
    // (which races other tests in the same process) and no filesystem.

    fn cwd() -> PathBuf {
        PathBuf::from("/ws/qontinui-runner/src-tauri")
    }
    fn ws_root() -> PathBuf {
        PathBuf::from("/ws/qontinui-runner")
    }

    #[test]
    fn cargo_target_dir_env_wins_over_the_workspace_default() {
        // The measured defect: the fleet exports CARGO_TARGET_DIR at
        // `<runner>/src-tauri/target` and cargo writes there, while the old
        // resolution read `<runner>/target` and found a 54-day-old exe.
        let env = "/ws/qontinui-runner/src-tauri/target";
        let candidates = target_dir_candidates(&cwd(), &ws_root(), Some(env), None);
        assert_eq!(candidates[0].0, TargetDirSource::CargoTargetDirEnv);
        assert_eq!(candidates[0].1, PathBuf::from(env));
        // The default is still present as the last-resort candidate.
        assert_eq!(
            candidates.last().unwrap(),
            &(
                TargetDirSource::WorkspaceDefault,
                PathBuf::from("/ws/qontinui-runner/target")
            )
        );
    }

    #[test]
    fn unset_cargo_target_dir_resolves_to_the_workspace_default() {
        // The inverse of the bug: an environment with no override must keep
        // resolving exactly where it does today. This is what repointing the
        // constant at `src-tauri/target` would have broken.
        let candidates = target_dir_candidates(&cwd(), &ws_root(), None, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0],
            (
                TargetDirSource::WorkspaceDefault,
                PathBuf::from("/ws/qontinui-runner/target")
            )
        );
    }

    #[test]
    fn empty_cargo_target_dir_is_treated_as_unset() {
        let candidates = target_dir_candidates(&cwd(), &ws_root(), Some("   "), None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, TargetDirSource::WorkspaceDefault);
    }

    #[test]
    fn relative_cargo_target_dir_resolves_against_the_cargo_cwd() {
        // Cargo resolves a relative CARGO_TARGET_DIR against its own working
        // directory, which for the runner is `src-tauri` — NOT the workspace
        // root. Getting this backwards is how a "../target" override lands one
        // directory off.
        let candidates = target_dir_candidates(&cwd(), &ws_root(), Some("target"), None);
        assert_eq!(
            candidates[0].1,
            PathBuf::from("/ws/qontinui-runner/src-tauri/target")
        );
    }

    #[test]
    fn build_target_dir_from_cargo_config_is_honoured_below_the_env() {
        let value = PathBuf::from("cargo-out");
        let base = PathBuf::from("/ws/qontinui-runner/src-tauri");
        let candidates = target_dir_candidates(
            &cwd(),
            &ws_root(),
            None,
            Some((value.as_path(), base.as_path())),
        );
        assert_eq!(candidates[0].0, TargetDirSource::CargoConfigBuildTargetDir);
        // Config-relative paths anchor at the dir CONTAINING `.cargo`.
        assert_eq!(
            candidates[0].1,
            PathBuf::from("/ws/qontinui-runner/src-tauri/cargo-out")
        );
        assert_eq!(candidates[1].0, TargetDirSource::WorkspaceDefault);

        // With BOTH set, the env still wins — cargo's own order.
        let both = target_dir_candidates(
            &cwd(),
            &ws_root(),
            Some("/env/target"),
            Some((value.as_path(), base.as_path())),
        );
        assert_eq!(
            both.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![
                TargetDirSource::CargoTargetDirEnv,
                TargetDirSource::CargoConfigBuildTargetDir,
                TargetDirSource::WorkspaceDefault,
            ]
        );
    }

    #[test]
    fn parse_build_target_dir_reads_both_spellings_and_ignores_the_rest() {
        assert_eq!(
            parse_build_target_dir("[build]\ntarget-dir = \"out\"\n"),
            Some(PathBuf::from("out"))
        );
        assert_eq!(
            parse_build_target_dir("build.target-dir = \"out\"\n"),
            Some(PathBuf::from("out"))
        );
        // The runner's real config sets [env] + rustflags but no target-dir —
        // it must read as "no opinion", not as an empty override.
        assert_eq!(
            parse_build_target_dir("[env]\nFOO = \"bar\"\n[build]\nrustflags = []\n"),
            None
        );
        assert_eq!(parse_build_target_dir("not = valid = toml"), None);
        assert_eq!(parse_build_target_dir("[build]\ntarget-dir = \"\"\n"), None);
    }

    #[test]
    fn pick_exe_candidate_takes_the_first_that_exists() {
        let candidates = target_dir_candidates(
            &cwd(),
            &ws_root(),
            Some("/ws/qontinui-runner/src-tauri/target"),
            None,
        )
        .into_iter()
        .map(|(s, d)| (s, d.join("debug").join(RUNNER_BIN_NAME)))
        .collect::<Vec<_>>();

        // Both present → the env override wins (the fresh artifact).
        let (src, _) = pick_exe_candidate(&candidates, |_| true);
        assert_eq!(src, TargetDirSource::CargoTargetDirEnv);

        // Override aimed somewhere that holds no runner (e.g. a shell that
        // exported CARGO_TARGET_DIR for a DIFFERENT crate) → degrade to the
        // default rather than resolve to a path that will never hold a runner.
        let default_exe = candidates.last().unwrap().1.clone();
        let (src, path) = pick_exe_candidate(&candidates, |p| p == default_exe);
        assert_eq!(src, TargetDirSource::WorkspaceDefault);
        assert_eq!(path, default_exe);

        // Nothing exists → report the highest-precedence candidate, i.e. where
        // cargo would have written, so the error names a useful path.
        let (src, _) = pick_exe_candidate(&candidates, |_| false);
        assert_eq!(src, TargetDirSource::CargoTargetDirEnv);
    }

    #[test]
    fn read_cargo_config_target_dir_walks_up_and_anchors_relative_values() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let src_tauri = root.join("qontinui-runner").join("src-tauri");
        std::fs::create_dir_all(src_tauri.join(".cargo")).unwrap();
        // No target-dir here — the deepest config must not shadow an ancestor
        // that DOES set one just by existing.
        std::fs::write(
            src_tauri.join(".cargo").join("config.toml"),
            "[env]\nFOO = \"bar\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo").join("config.toml"),
            "[build]\ntarget-dir = \"shared-target\"\n",
        )
        .unwrap();

        let (value, base) = read_cargo_config_target_dir(&src_tauri).expect("ancestor config");
        assert_eq!(value, PathBuf::from("shared-target"));
        assert_eq!(base, root);
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

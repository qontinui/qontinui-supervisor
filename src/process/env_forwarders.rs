//! Env-var forwarders for spawned runners (Item 4 of the runner-supervisor
//! modularity plan).
//!
//! Each forwarder is a unit struct implementing [`EnvForwarder`]. The set is
//! constructed once in [`crate::process::manager::start_exe_mode_for_runner`]
//! and applied in a single loop, replacing what used to be five hand-written
//! `forward_*_env` functions called from two `cfg(windows)` / `cfg(not(windows))`
//! branches.
//!
//! Adding a new forwarder is now: write one struct + one registration line,
//! not a five-place edit. The decorations-forwarding bug we hit (function
//! existed but wasn't called from one of the branches) becomes structurally
//! impossible.
//!
//! ## Order is significant
//!
//! Forwarders run in registration order. [`ExtraEnv`] MUST be last so callers
//! of `POST /runners/spawn-test` with `extra_env: {...}` can override anything
//! the supervisor set (documented at `config.rs:148-153`). [`PanicLogEnv`]
//! must run before the spawn so the runner sees `QONTINUI_RUNNER_LOG_DIR`.

use std::future::Future;
use std::pin::Pin;
use tokio::process::Command;

use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::is_temp_runner;
use crate::state::{ManagedRunner, SharedState};

/// Forwarder of one logical group of environment variables onto a child
/// runner's spawn [`Command`].
///
/// Implementors are unit structs registered into the slice returned by
/// [`default_env_forwarders`]. Each forwarder's `apply` is invoked once per
/// runner start; side-effects are limited to mutating the [`Command`]
/// (`cmd.env(...)` calls) and, for [`PanicLogEnv`], writing to
/// `runner.panic_log_dir`.
pub trait EnvForwarder: Send + Sync {
    /// Short identifier surfaced via tracing for forwarder-level diagnostics.
    fn name(&self) -> &'static str;

    /// Apply this forwarder's env vars to `cmd`.
    ///
    /// Returns a boxed future to keep the trait object-safe without dragging
    /// in `async-trait`. None of the forwarders need to be polled in
    /// parallel — the manager awaits each one in registration order.
    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Build the canonical forwarder list applied at every supervisor-managed
/// runner spawn.
///
/// Order is load-bearing — see the module docs.
pub fn default_env_forwarders() -> Vec<Box<dyn EnvForwarder>> {
    vec![
        Box::new(TestAutoLoginEnv),
        Box::new(DevBootstrapEnv),
        Box::new(WorktreeModeEnv),
        Box::new(WindowPositionEnv),
        Box::new(RestateEnv),
        Box::new(RunnerTierEnv),
        Box::new(PlanAdapterEnv),
        Box::new(PanicLogEnv),
        Box::new(ExtraEnv),
    ]
}

// =============================================================================
// TestAutoLoginEnv
// =============================================================================

/// Forwards test-auto-login credentials so spawned runners can self-login as
/// the dev user. Resolves in priority order:
///
/// 1. Runtime credentials set via `POST /test-login` (stored on `SharedState`).
/// 2. The runner project's `.env` file (`VITE_DEV_EMAIL` / `VITE_DEV_PASSWORD`).
/// 3. Supervisor process env vars (`QONTINUI_TEST_LOGIN_EMAIL`/`PASSWORD`)
///    as a CI fallback.
///
/// SECURITY: dev-account only. The supervisor never targets a production
/// binary, so forwarding to every spawned runner — primary included — is
/// safe; the runner's `AuthProvider` only consumes these creds when no
/// other auth state exists.
pub struct TestAutoLoginEnv;

impl EnvForwarder for TestAutoLoginEnv {
    fn name(&self) -> &'static str {
        "test_auto_login"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        _runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Priority 1: runtime-configured credentials.
            if let Ok(guard) = state.test_auto_login.try_read() {
                if let Some((ref email, ref password)) = *guard {
                    cmd.env("QONTINUI_TEST_AUTO_LOGIN_EMAIL", email);
                    cmd.env("QONTINUI_TEST_AUTO_LOGIN_PASSWORD", password);
                    return;
                }
            }
            // Priority 2: runner's `.env` file.
            if let Some((email, password)) = read_runner_env_creds(&state.config.project_dir) {
                cmd.env("QONTINUI_TEST_AUTO_LOGIN_EMAIL", email);
                cmd.env("QONTINUI_TEST_AUTO_LOGIN_PASSWORD", password);
                return;
            }
            // Priority 3: supervisor process env.
            if let (Ok(email), Ok(password)) = (
                std::env::var("QONTINUI_TEST_LOGIN_EMAIL"),
                std::env::var("QONTINUI_TEST_LOGIN_PASSWORD"),
            ) {
                if !email.is_empty() && !password.is_empty() {
                    cmd.env("QONTINUI_TEST_AUTO_LOGIN_EMAIL", email);
                    cmd.env("QONTINUI_TEST_AUTO_LOGIN_PASSWORD", password);
                }
            }
        })
    }
}

/// Read `VITE_DEV_EMAIL` / `VITE_DEV_PASSWORD` from the runner's `.env` file.
/// `project_dir` points at `<runner>/src-tauri`; `.env` lives at its parent.
/// Returns `None` if the file is missing, unreadable, or either key is absent.
fn read_runner_env_creds(project_dir: &std::path::Path) -> Option<(String, String)> {
    let env_path = project_dir.parent()?.join(".env");
    let content = std::fs::read_to_string(env_path).ok()?;
    let mut email: Option<String> = None;
    let mut password: Option<String> = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim().trim_matches(|c| c == '"' || c == '\'')),
            None => continue,
        };
        match key {
            "VITE_DEV_EMAIL" if !value.is_empty() => email = Some(value.to_string()),
            "VITE_DEV_PASSWORD" if !value.is_empty() => password = Some(value.to_string()),
            _ => {}
        }
    }
    Some((email?, password?))
}

// =============================================================================
// DevBootstrapEnv
// =============================================================================

/// Forwards `QONTINUI_DEV_BOOTSTRAP` from the supervisor's own environment
/// onto every spawned runner.
///
/// When set (typically `=1` in dev), the runner's bootstrap flow registers
/// the dev app set (runner + web + supervisor) automatically into
/// `project.apps` so the spec-multi-app routes (`/apps/<app_id>/spec/*`) have
/// a populated registry on first boot. Without this forwarder, temp runners
/// boot with an empty `project.apps` registry even though the supervisor
/// process has the bootstrap flag set, and callers have to `POST /apps` by
/// hand to register every dev app.
///
/// Forwarded unconditionally when the supervisor has the var set — there is
/// no scenario where a runner spawned by this supervisor benefits from a
/// different bootstrap mode than the supervisor itself. Callers that need
/// to disable bootstrap on a specific temp runner can still do so via
/// `extra_env: {"QONTINUI_DEV_BOOTSTRAP": "0"}` because [`ExtraEnv`] runs
/// after this forwarder (see module docs on order).
pub struct DevBootstrapEnv;

impl EnvForwarder for DevBootstrapEnv {
    fn name(&self) -> &'static str {
        "dev_bootstrap"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        _runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if let Ok(val) = std::env::var("QONTINUI_DEV_BOOTSTRAP") {
                if !val.is_empty() {
                    cmd.env("QONTINUI_DEV_BOOTSTRAP", val);
                }
            }
        })
    }
}

// =============================================================================
// WorktreeModeEnv
// =============================================================================

/// Environment variable the runner reads to enable per-session git-worktree
/// isolation for terminal edit sessions. When `=1`, each terminal edit session
/// gets its own git worktree + coord Worktree claim instead of every session
/// sharing one physical checkout (which causes concurrent `git switch`
/// clobbers). The runner's compiled default is OFF; this forwarder makes
/// worktree-isolation the supervisor's default-on posture (Layer 4 of
/// `plans/2026-06-03-shared-checkout-coordination-gap-fix.md`).
const ENV_AGENT_WORKTREE_MODE: &str = "QONTINUI_AGENT_WORKTREE_MODE";

/// Supervisor-side opt-out. When set to one of `0`/`off`/`false`/`no`
/// (case-insensitive), the supervisor injects NOTHING, leaving every runner on
/// the runner's own compiled default (worktree-isolation off).
const ENV_SUPERVISOR_WORKTREE_DEFAULT: &str = "QONTINUI_SUPERVISOR_WORKTREE_DEFAULT";

/// Pure precedence resolver for the worktree-mode posture. Returns the value to
/// inject as `QONTINUI_AGENT_WORKTREE_MODE` on the child, or `None` to inject
/// nothing.
///
/// Precedence (highest first):
/// 1. **Explicit opt-out.** If `supervisor_default` (the value of
///    `QONTINUI_SUPERVISOR_WORKTREE_DEFAULT` in the supervisor's env) is one of
///    `0`/`off`/`false`/`no` (case-insensitive, trimmed) → return `None`. The
///    runner keeps its own compiled default (off).
/// 2. **Respect operator override.** Else if `agent_mode` (the value of
///    `QONTINUI_AGENT_WORKTREE_MODE` already set in the supervisor's env) is
///    `Some` → propagate that EXACT value (including `0`/`off`) to the child.
///    This makes the posture reversible without code changes.
/// 3. **Default-on.** Else → return `Some("1")`, turning the dormant
///    worktree-isolation mechanism on for every spawned runner.
///
/// Kept as a free function over its two inputs (not reading `std::env`
/// directly) so it is exhaustively unit-testable without process-global env
/// mutation. [`WorktreeModeEnv::apply`] is the thin env-reading wrapper.
pub(crate) fn resolve_worktree_mode(
    supervisor_default: Option<&str>,
    agent_mode: Option<&str>,
) -> Option<String> {
    if let Some(raw) = supervisor_default {
        let v = raw.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "off" | "false" | "no") {
            return None;
        }
    }
    if let Some(explicit) = agent_mode {
        return Some(explicit.to_string());
    }
    Some("1".to_string())
}

/// Inject the worktree-isolation posture (`QONTINUI_AGENT_WORKTREE_MODE`) onto
/// every supervisor-spawned runner per [`resolve_worktree_mode`].
///
/// Registered before [`ExtraEnv`] so a specific `POST /runners/spawn-test`
/// caller can still override it via
/// `extra_env: {"QONTINUI_AGENT_WORKTREE_MODE": "0"}` (see module docs on
/// order).
pub struct WorktreeModeEnv;

impl EnvForwarder for WorktreeModeEnv {
    fn name(&self) -> &'static str {
        "worktree_mode"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let supervisor_default = std::env::var(ENV_SUPERVISOR_WORKTREE_DEFAULT).ok();
            let agent_mode = std::env::var(ENV_AGENT_WORKTREE_MODE).ok();
            match resolve_worktree_mode(supervisor_default.as_deref(), agent_mode.as_deref()) {
                Some(val) => {
                    cmd.env(ENV_AGENT_WORKTREE_MODE, &val);
                    let msg = format!(
                        "Worktree mode: runner '{}' spawned with {}={}",
                        runner.config.name, ENV_AGENT_WORKTREE_MODE, val
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                }
                None => {
                    let msg = format!(
                        "Worktree mode: runner '{}' left on runner default ({}={} opt-out)",
                        runner.config.name,
                        ENV_SUPERVISOR_WORKTREE_DEFAULT,
                        supervisor_default.as_deref().unwrap_or("")
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                }
            }
        })
    }
}

// =============================================================================
// WindowPositionEnv
// =============================================================================

/// Fetch placement for a TEMP runner from the primary runner's
/// `/spawn-placement/temp` endpoint, then push its rect as
/// `QONTINUI_WINDOW_X/Y/WIDTH/HEIGHT` (+ optional `_DECORATIONS`). The
/// runner reads these at window-build time.
///
/// **Temp-only.** Only applies when `runner.config` identifies a `test-*`
/// runner AND it's not the primary. Named runners have their own
/// per-instance `spawn_placement` in settings.json. Primary runners never
/// get placement-forwarded. The forwarder is a no-op outside that gate, so
/// it's safe to leave registered for every runner.
///
/// On any failure (404, 502, network error, timeout, parse error) we log
/// and skip — the runner falls back to its built-in default behavior.
/// Never fails the spawn.
pub struct WindowPositionEnv;

impl EnvForwarder for WindowPositionEnv {
    fn name(&self) -> &'static str {
        "window_position"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Gate: temp runners only, never primary.
            if runner.config.kind().is_primary() {
                return;
            }
            if !is_temp_runner(&runner.config.id) {
                return;
            }

            let runner_id = runner.config.id.as_str();
            let runner_name = runner.config.name.as_str();

            // Count existing test-* runners EXCLUDING the one we're about to spawn.
            // `start_managed_runner` pre-inserts the new runner into `state.runners`
            // (defensive re-insertion against the spawn-test 404 race), so a naive
            // count would include the runner-to-be and shift every spawn one index
            // off — the first temp would land on index 1 instead of 0.
            let runners = state.runners.read().await;
            let temp_count = runners
                .keys()
                .filter(|id| id.starts_with("test-") && id.as_str() != runner_id)
                .count();
            drop(runners);
            let index = temp_count;

            // The URL building, GET, envelope-vs-bare unwrap, and response
            // typing all live behind `qontinui_runner_client::SpawnPlacementClient`
            // (Item 8 of the modularity plan), and the typed payload comes
            // from `qontinui_types::wire::placement::SpawnPlacementResponse`
            // (Item 1) so the runner and supervisor agree on the shape at
            // compile time. We keep the per-call 3s budget by wrapping the
            // call in a short `tokio::time::timeout` — the shared
            // `state.http_client` has a 10s default that we want to tighten
            // for this latency-sensitive path (it's blocking spawn). All
            // errors are mapped to a logged "skip, fall back to runner
            // default" branch so a slow runner never blocks a spawn.
            use qontinui_runner_client::{
                Overflow, SpawnPlacementClient, SpawnPlacementClientError,
            };

            let base = match reqwest::Url::parse("http://localhost:9876") {
                Ok(u) => u,
                Err(e) => {
                    // Static URL — this should never fail, but log defensively.
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — runner base URL parse failed: {}",
                        runner_name, e
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
            };
            let client = SpawnPlacementClient::new(base, state.http_client.clone());
            let fetch = client.temp(index, Overflow::Wrap);
            let placement = match tokio::time::timeout(std::time::Duration::from_secs(3), fetch)
                .await
            {
                Ok(Ok(p)) => p,
                Ok(Err(SpawnPlacementClientError::Status { status, body })) => {
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — index {} returned {}: {}",
                        runner_name,
                        index,
                        status,
                        body.chars().take(200).collect::<String>()
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
                Ok(Err(SpawnPlacementClientError::EnvelopeError { error })) => {
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — endpoint returned envelope without success/data (error={:?})",
                        runner_name, error
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
                Ok(Err(SpawnPlacementClientError::Parse(e))) => {
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — failed to parse response: {}",
                        runner_name, e
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
                Ok(Err(e)) => {
                    // Http(reqwest::Error) and Url(url::ParseError) both flow here.
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — request failed: {}",
                        runner_name, e
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
                Err(_) => {
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — request to runner timed out after 3s",
                        runner_name
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
            };

            let msg = format!(
                "Spawn temp placement: runner '{}' index={} (label={:?}, monitor={:?}, source={:?}) at ({},{}) {}x{}",
                runner_name,
                index,
                placement.slot_label,
                placement.monitor_label,
                placement.source,
                placement.global_x,
                placement.global_y,
                placement.width,
                placement.height
            );
            tracing::info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;

            cmd.env("QONTINUI_WINDOW_X", placement.global_x.to_string());
            cmd.env("QONTINUI_WINDOW_Y", placement.global_y.to_string());
            cmd.env("QONTINUI_WINDOW_WIDTH", placement.width.to_string());
            cmd.env("QONTINUI_WINDOW_HEIGHT", placement.height.to_string());
            if let Some(d) = placement.decorations {
                cmd.env("QONTINUI_WINDOW_DECORATIONS", if d { "1" } else { "0" });
            }
        })
    }
}

// =============================================================================
// RestateEnv
// =============================================================================

/// Forward Restate ports + external URLs when the runner is in
/// `server_mode`. Sets `QONTINUI_SERVER_MODE=1` plus the four
/// `QONTINUI_RESTATE_*` vars on top.
pub struct RestateEnv;

impl EnvForwarder for RestateEnv {
    fn name(&self) -> &'static str {
        "restate"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let config = &runner.config;
            if !config.server_mode {
                return;
            }
            if let Some(p) = config.restate_ingress_port {
                cmd.env("QONTINUI_RESTATE_INGRESS_PORT", p.to_string());
            }
            if let Some(p) = config.restate_admin_port {
                cmd.env("QONTINUI_RESTATE_ADMIN_PORT", p.to_string());
            }
            if let Some(p) = config.restate_service_port {
                cmd.env("QONTINUI_RESTATE_SERVICE_PORT", p.to_string());
            }
            if let Some(ref u) = config.external_restate_admin_url {
                cmd.env("QONTINUI_RESTATE_EXTERNAL_ADMIN_URL", u);
            }
            if let Some(ref u) = config.external_restate_ingress_url {
                cmd.env("QONTINUI_RESTATE_EXTERNAL_INGRESS_URL", u);
            }
            cmd.env("QONTINUI_SERVER_MODE", "1");
        })
    }
}

// =============================================================================
// RunnerTierEnv
// =============================================================================

/// Force a spawned temp runner to boot at Tier 0 (`Local`) regardless of what
/// the shared runner settings.json says.
///
/// **Why this is necessary.** The runner's `settings::get_settings_path()`
/// resolves to `dirs::config_dir() / "com.qontinui.runner" / "settings.json"`
/// — a single shared file across the primary runner, every named runner, and
/// every temp runner. When the primary is signed into Qontinui (Tier 2,
/// `runner_token` populated), every fresh temp runner reads the same shared
/// settings, sees `tier == qontinui_account`, has no JWT in its (also-shared)
/// keychain slot, and parks on `LoginScreen` waiting for credentials. The
/// supervisor cannot fix this by `POST`ing `set_runner_tier` to the temp
/// runner because that handler persists, and the persist would clobber the
/// primary's Tier 2 state on disk.
///
/// The runner now honors `QONTINUI_RUNNER_TIER` as an in-memory overlay
/// inside `load_settings()` (parallel to the existing `QONTINUI_RESTATE_*`
/// and `QONTINUI_WEB_*` overlays — never persisted). This forwarder sets
/// it to `local` for temp runners only.
///
/// **Temp-only.** Named runners are user-managed and can be promoted to
/// Tier 2 by the operator; we must not silently demote them. Primary
/// runners are user-managed too. The temp-runner gate matches
/// [`WindowPositionEnv`]'s gating.
///
/// **Order.** Registered before [`ExtraEnv`] so callers can still override
/// via `extra_env: {"QONTINUI_RUNNER_TIER": "qontinui_account"}` when a
/// specific test needs Tier 2 (e.g., the calibration matrix's auto-login
/// fixture). See the module docs.
pub struct RunnerTierEnv;

impl EnvForwarder for RunnerTierEnv {
    fn name(&self) -> &'static str {
        "runner_tier"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if !is_temp_runner(&runner.config.id) {
                return;
            }
            cmd.env("QONTINUI_RUNNER_TIER", "local");
        })
    }
}

// =============================================================================
// PlanAdapterEnv
// =============================================================================

/// Forwards the plan→work-unit adapter configuration
/// (`QONTINUI_PLAN_ADAPTER_DIR`, `QONTINUI_PLAN_ADAPTER_INTERVAL_SECS`) to the
/// **primary** runner, reading the persisted user environment fresh at each
/// spawn.
///
/// The runner's adapter (`plan_workunit_adapter::trigger::spawn_if_configured`)
/// is gated at boot on `QONTINUI_PLAN_ADAPTER_DIR` in the runner's process
/// env. A spawned runner inherits the SUPERVISOR's env snapshot, and the
/// supervisor is long-lived — an operator `setx` after the supervisor started
/// is invisible to every later runner respawn, so the adapter silently stays
/// dormant across primary rebuilds (observed 2026-07-03). Reading
/// `HKCU\Environment` at spawn time makes `setx` + runner restart sufficient;
/// no supervisor restart required.
///
/// Resolution per variable (first non-blank wins):
/// 1. `HKCU\Environment` (Windows) — the persisted store `setx` writes;
///    always current, unlike this process's env snapshot.
/// 2. The supervisor's process env — non-Windows platforms, and supervisors
///    deliberately launched with the variable set.
///
/// **Primary-only.** The adapter mirrors the operator's plans dir into coord;
/// one reconcile writer is the intent — temp/named runners must not each start
/// a competing scan of the same dir. A test that wants the adapter on a temp
/// runner can still inject via `extra_env` ([`ExtraEnv`] runs last and
/// overrides anything set here).
pub struct PlanAdapterEnv;

const PLAN_ADAPTER_VARS: [&str; 2] = [
    "QONTINUI_PLAN_ADAPTER_DIR",
    "QONTINUI_PLAN_ADAPTER_INTERVAL_SECS",
];

impl EnvForwarder for PlanAdapterEnv {
    fn name(&self) -> &'static str {
        "plan_adapter"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if !runner.config.kind().is_primary() {
                return;
            }
            for var in PLAN_ADAPTER_VARS {
                if let Some(value) = resolve_plan_adapter_value(
                    read_persisted_user_env(var),
                    std::env::var(var).ok(),
                ) {
                    cmd.env(var, value);
                }
            }
        })
    }
}

/// Pick the value to forward: the persisted user env (read fresh) beats the
/// supervisor's process-env snapshot (stale by definition on a long-lived
/// supervisor); blank/whitespace values count as unset at both levels, so a
/// blanked-out registry entry cannot mask a real process-env value.
fn resolve_plan_adapter_value(
    persisted: Option<String>,
    process: Option<String>,
) -> Option<String> {
    persisted
        .filter(|v| !v.trim().is_empty())
        .or_else(|| process.filter(|v| !v.trim().is_empty()))
}

/// Read `name` from `HKCU\Environment` — the store `setx` writes — so the
/// value reflects the operator's latest persist rather than the env snapshot
/// this process was started with.
#[cfg(windows)]
fn read_persisted_user_env(name: &str) -> Option<String> {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Environment")
        .ok()?
        .get_value::<String, _>(name)
        .ok()
}

/// Non-Windows: there is no persisted user-env store; process env is the
/// only source.
#[cfg(not(windows))]
fn read_persisted_user_env(_name: &str) -> Option<String> {
    None
}

// =============================================================================
// PanicLogEnv
// =============================================================================

/// Decide and set `QONTINUI_RUNNER_LOG_DIR` for the spawned runner, plus
/// `QONTINUI_RUNNER_ID` so the runner's panic hook can tag its output.
///
/// Resolution:
/// * If `state.config.log_dir` is set on the supervisor (`--log-dir <dir>`),
///   use `<log-dir>/runner-<id>/`. The per-runner subdir avoids clobbering
///   a sibling runner's `runner-panic.log` when several test runners panic
///   in quick succession.
/// * Otherwise the runner's default path is fine — we still set
///   `QONTINUI_RUNNER_ID` so any diagnostic tooling can correlate.
///
/// Stores the resolved per-runner panic-log path on `runner.panic_log_dir`
/// so `monitor_runner_process_exit` can find the file after a non-zero
/// exit. (Pre-trait code returned a `PathBuf` from the helper and the
/// caller wrote it onto `ManagedRunner` after spawn; storing it here
/// directly removes the special-case return path from the trait.)
pub struct PanicLogEnv;

impl EnvForwarder for PanicLogEnv {
    fn name(&self) -> &'static str {
        "panic_log"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let runner_id = runner.config.id.as_str();
            cmd.env("QONTINUI_RUNNER_ID", runner_id);

            let new_dir = if let Some(log_dir) = state.config.log_dir.as_ref() {
                let per_runner = log_dir.join(format!("runner-{}", runner_id));
                let _ = std::fs::create_dir_all(&per_runner);
                cmd.env("QONTINUI_RUNNER_LOG_DIR", &per_runner);
                Some(per_runner)
            } else {
                None
            };
            // Stash on the ManagedRunner so `monitor_runner_process_exit`
            // can find `runner-panic.log` after a non-zero exit. The
            // pre-trait code returned this from the helper and the caller
            // wrote it onto `ManagedRunner` after spawn; doing it here
            // directly keeps the panic-log lifecycle in one place and
            // removes the special-case return path from the trait.
            *runner.panic_log_dir.write().await = new_dir;
        })
    }
}

// =============================================================================
// ExtraEnv
// =============================================================================

/// Apply the caller-supplied `extra_env` map to the spawn command.
///
/// MUST run last. Callers (e.g. `POST /runners/spawn-test` with
/// `{"extra_env": {"QONTINUI_SCRIPTED_OUTPUT": "1"}}`) override anything
/// the supervisor set, including `QONTINUI_SERVER_MODE`, `QONTINUI_API_URL`,
/// etc. See `config.rs:148-153` for the documented contract.
pub struct ExtraEnv;

impl EnvForwarder for ExtraEnv {
    fn name(&self) -> &'static str {
        "extra_env"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            for (k, v) in &runner.config.extra_env {
                cmd.env(k, v);
            }
        })
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::{resolve_plan_adapter_value, resolve_worktree_mode};

    #[test]
    fn default_on_when_nothing_set() {
        // No supervisor opt-out, no operator override → inject =1.
        assert_eq!(resolve_worktree_mode(None, None), Some("1".to_string()));
    }

    #[test]
    fn opt_out_values_inject_nothing() {
        // Each documented opt-out value (case-insensitive, trimmed) suppresses
        // injection regardless of any operator override.
        for v in ["0", "off", "false", "no", "OFF", "False", "No", " off "] {
            assert_eq!(
                resolve_worktree_mode(Some(v), None),
                None,
                "supervisor default {v:?} should opt out"
            );
            // Opt-out wins even when an operator override is also present.
            assert_eq!(
                resolve_worktree_mode(Some(v), Some("1")),
                None,
                "supervisor default {v:?} should opt out even with agent_mode=1"
            );
        }
    }

    #[test]
    fn non_opt_out_supervisor_default_does_not_suppress() {
        // A non-opt-out value of QONTINUI_SUPERVISOR_WORKTREE_DEFAULT (e.g. "1"
        // or junk) is NOT an opt-out — falls through to the default-on path.
        assert_eq!(
            resolve_worktree_mode(Some("1"), None),
            Some("1".to_string())
        );
        assert_eq!(
            resolve_worktree_mode(Some("yes"), None),
            Some("1".to_string())
        );
        assert_eq!(resolve_worktree_mode(Some(""), None), Some("1".to_string()));
    }

    #[test]
    fn operator_override_propagated_verbatim() {
        // An explicit QONTINUI_AGENT_WORKTREE_MODE in the supervisor env is
        // propagated EXACTLY (reversibility) when there's no opt-out.
        assert_eq!(
            resolve_worktree_mode(None, Some("1")),
            Some("1".to_string())
        );
        assert_eq!(
            resolve_worktree_mode(None, Some("0")),
            Some("0".to_string())
        );
        assert_eq!(
            resolve_worktree_mode(None, Some("off")),
            Some("off".to_string())
        );
        // Verbatim — no normalization of an unusual value.
        assert_eq!(
            resolve_worktree_mode(None, Some("verbose")),
            Some("verbose".to_string())
        );
    }

    #[test]
    fn plan_adapter_persisted_beats_process_snapshot() {
        // The freshly-read persisted value wins over the (possibly stale)
        // process-env snapshot — the whole point of reading at spawn time.
        assert_eq!(
            resolve_plan_adapter_value(
                Some("D:/plans-new".to_string()),
                Some("D:/plans-old".to_string())
            ),
            Some("D:/plans-new".to_string())
        );
    }

    #[test]
    fn plan_adapter_process_env_is_fallback() {
        // No persisted value (non-Windows, or never setx'd) → the supervisor's
        // process env still works, so explicit launches keep functioning.
        assert_eq!(
            resolve_plan_adapter_value(None, Some("D:/plans".to_string())),
            Some("D:/plans".to_string())
        );
        assert_eq!(resolve_plan_adapter_value(None, None), None);
    }

    #[test]
    fn plan_adapter_blank_values_count_as_unset() {
        // A blank registry entry must not mask a real process-env value, and
        // blank-everywhere resolves to no injection at all.
        assert_eq!(
            resolve_plan_adapter_value(Some("  ".to_string()), Some("D:/plans".to_string())),
            Some("D:/plans".to_string())
        );
        assert_eq!(
            resolve_plan_adapter_value(Some(String::new()), Some(String::new())),
            None
        );
    }
}

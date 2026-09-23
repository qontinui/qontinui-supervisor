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
//! [`DisplayEnv`] and [`SetupWizardBypassEnv`] run before [`ExtraEnv`] for the
//! same override reason; the `no_display` refusal is checked after the whole
//! list has run ([`display_refusal_for_command`]), so it judges final values.

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
        Box::new(SetupWizardBypassEnv),
        Box::new(DisplayEnv),
        Box::new(ExtraEnv),
    ]
}

// =============================================================================
// SetupWizardBypassEnv
// =============================================================================

/// Env var the runner's `check_setup_completed` is to honor to skip the
/// first-run SetupWizard. This is the SUPERVISOR half of UI-5; until the
/// runner half lands (`commands/setup_wizard.rs`), the runner ignores it and a
/// temp runner still opens the wizard.
pub(crate) const SETUP_WIZARD_BYPASS_ENV: &str = "QONTINUI_SETUP_WIZARD_BYPASS";

/// Sets `QONTINUI_SETUP_WIZARD_BYPASS=1` on every **temp** runner spawn.
///
/// A temp runner boots with a fresh per-instance config dir, so the runner
/// treats it as a first run and opens the SetupWizard — a full-viewport cover
/// that hides every page an agent spawned the runner to drive. The runner used
/// to infer "test runner" from `QONTINUI_TEST_AUTO_LOGIN_EMAIL`, which the
/// paired-profile path never sets; this is the explicit flag instead (plan
/// `2026-09-23-conductor-e2e-phase1-defects`, UI-5). Inert until the runner
/// reads it — see [`SETUP_WIZARD_BYPASS_ENV`]. Primary and named runners
/// are an operator's own and keep the wizard. Registered before [`ExtraEnv`]
/// so `extra_env: {"QONTINUI_SETUP_WIZARD_BYPASS": "0"}` can still exercise
/// the wizard on a temp runner.
pub struct SetupWizardBypassEnv;

/// Pure gate for [`SetupWizardBypassEnv`]: the value to set, or `None`.
pub(crate) fn setup_wizard_bypass_value(is_temp: bool) -> Option<&'static str> {
    is_temp.then_some("1")
}

impl EnvForwarder for SetupWizardBypassEnv {
    fn name(&self) -> &'static str {
        "setup_wizard_bypass"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        _state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if let Some(val) = setup_wizard_bypass_value(runner.config.kind().is_temp()) {
                cmd.env(SETUP_WIZARD_BYPASS_ENV, val);
            }
        })
    }
}

// =============================================================================
// DisplayEnv
// =============================================================================

/// The display-selection variables a GTK/WebKitGTK runner reads on Linux.
pub(crate) const DISPLAY_ENV_KEYS: [&str; 4] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "GDK_BACKEND",
    "BROADWAY_DISPLAY",
];

/// Resolve the display variables to set on a temp runner.
///
/// Every key in [`DISPLAY_ENV_KEYS`] the supervisor's own environment carries
/// (non-blank) is forwarded verbatim. When the supervisor has no `DISPLAY`,
/// the configured knob (`--temp-runner-display` /
/// `QONTINUI_SUPERVISOR_TEMP_DISPLAY`) supplies it. Pure over its inputs so
/// the precedence is testable without mutating the process environment.
pub(crate) fn resolve_display_env(
    supervisor_env: impl Fn(&str) -> Option<String>,
    knob: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = DISPLAY_ENV_KEYS
        .iter()
        .filter_map(|k| {
            supervisor_env(k)
                .filter(|v| !v.trim().is_empty())
                .map(|v| (*k, v))
        })
        .collect();
    if !out.iter().any(|(k, _)| *k == "DISPLAY") {
        if let Some(display) = knob.map(str::trim).filter(|v| !v.is_empty()) {
            out.push(("DISPLAY", display.to_string()));
        }
    }
    out
}

/// Forwards the display variables to **temp** runners on Linux (plan
/// `2026-09-23-conductor-e2e-phase1-defects`, S-3).
///
/// A supervisor started from a non-GUI shell has no `DISPLAY`, so the temp
/// runner it spawned used to die in GTK init with a panic that named nothing
/// the operator could act on. This forwarder carries the supervisor's own
/// display variables across explicitly, and supplies `DISPLAY` from the
/// `--temp-runner-display` / `QONTINUI_SUPERVISOR_TEMP_DISPLAY` knob when the
/// supervisor has none. Registered before [`ExtraEnv`] so `extra_env` still
/// overrides it. Whether a display then resolves at all is checked after
/// every forwarder has run, by [`display_refusal_for_command`].
pub struct DisplayEnv;

impl EnvForwarder for DisplayEnv {
    fn name(&self) -> &'static str {
        "display"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if !cfg!(target_os = "linux") || !runner.config.kind().is_temp() {
                return;
            }
            let resolved = resolve_display_env(
                |k| std::env::var(k).ok(),
                state.config.temp_runner_display.as_deref(),
            );
            for (key, value) in resolved {
                cmd.env(key, value);
            }
        })
    }
}

/// The refusal a temp spawn gets on Linux when neither `DISPLAY` nor
/// `WAYLAND_DISPLAY` resolves for the child, or `None` when it may proceed.
///
/// `explicit(key)` is what the spawn [`Command`] itself says about `key`:
/// `Some(Some(v))` set, `Some(None)` removed, `None` untouched — in which case
/// the child inherits `inherited(key)` from the supervisor. This is evaluated
/// AFTER every forwarder has run, [`ExtraEnv`] included, so it sees the final
/// values. The message starts `no_display:` and names the knob.
pub(crate) fn display_refusal(
    is_linux: bool,
    is_temp: bool,
    runner_name: &str,
    explicit: impl Fn(&str) -> Option<Option<String>>,
    inherited: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if !is_linux || !is_temp {
        return None;
    }
    let resolves = |key: &str| {
        let value = match explicit(key) {
            Some(set_or_removed) => set_or_removed,
            None => inherited(key),
        };
        value.is_some_and(|v| !v.trim().is_empty())
    };
    if resolves("DISPLAY") || resolves("WAYLAND_DISPLAY") {
        return None;
    }
    Some(format!(
        "no_display: temp runner '{runner_name}' would start with neither DISPLAY nor \
         WAYLAND_DISPLAY, and GTK cannot open a window without one. The supervisor's \
         environment has no display (it was likely started from a non-GUI shell). Start \
         the supervisor with --temp-runner-display <display> (or env \
         {knob}=<display>, e.g. :0), start it from a graphical session, or pass \
         extra_env {{\"DISPLAY\": \"<display>\"}} on the spawn.",
        knob = crate::config::TEMP_RUNNER_DISPLAY_ENV,
    ))
}

/// [`display_refusal`] over a real spawn [`Command`] and the supervisor's
/// live environment.
pub(crate) fn display_refusal_for_command(
    cmd: &Command,
    runner: &crate::config::RunnerConfig,
) -> Option<String> {
    let envs: Vec<(String, Option<String>)> = cmd
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    display_refusal(
        cfg!(target_os = "linux"),
        runner.kind().is_temp(),
        &runner.name,
        |key| envs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()),
        |key| std::env::var(key).ok(),
    )
}

/// [`display_refusal`] evaluated BEFORE a temp spawn's build, from what the
/// spawn request already determines: [`DisplayEnv`]'s resolution over the
/// supervisor's env and knob, overlaid by the request's `extra_env` (which
/// [`ExtraEnv`] applies last). A spawn-test build takes minutes, so a request
/// that can only end in `no_display` is refused up front instead of after the
/// compile. The post-forwarder check in the spawn path
/// ([`display_refusal_for_command`]) stays the final authority.
pub(crate) fn display_preflight_refusal(
    is_linux: bool,
    runner_name: &str,
    extra_env: &std::collections::HashMap<String, String>,
    supervisor_env: impl Fn(&str) -> Option<String>,
    knob: Option<&str>,
) -> Option<String> {
    let forwarded = resolve_display_env(&supervisor_env, knob);
    display_refusal(
        is_linux,
        true,
        runner_name,
        |key| {
            extra_env.get(key).map(|v| Some(v.clone())).or_else(|| {
                forwarded
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| Some(v.clone()))
            })
        },
        &supervisor_env,
    )
}

// =============================================================================
// TestAutoLoginEnv
// =============================================================================

/// Which of the three credential sources actually resolved, in the priority
/// order [`resolve_test_auto_login`] walks.
///
/// Exists so the spawn-test response (`auth_state.auto_login_source`) and
/// `GET /test-login` can report the **real** resolved source instead of
/// guessing. Before this, both endpoints hard-coded a
/// `state.test_auto_login.is_some()` check — i.e. they only ever saw
/// priority 1 — so a supervisor that was in fact forwarding credentials from
/// the runner's `.env` (priority 2, the common dev setup) reported
/// `auto_login_configured: false`. One resolver, two readers: the drift
/// cannot come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestAutoLoginSource {
    /// Runtime credentials set via `POST /test-login` (stored on `SharedState`).
    RuntimeApi,
    /// The runner project's `.env` file (`VITE_DEV_EMAIL` / `VITE_DEV_PASSWORD`).
    RunnerDotEnv,
    /// Supervisor process env (`QONTINUI_TEST_LOGIN_EMAIL` / `_PASSWORD`).
    SupervisorEnv,
}

impl TestAutoLoginSource {
    /// Stable wire string for the JSON APIs.
    pub fn as_str(self) -> &'static str {
        match self {
            TestAutoLoginSource::RuntimeApi => "runtime_api",
            TestAutoLoginSource::RunnerDotEnv => "runner_dotenv",
            TestAutoLoginSource::SupervisorEnv => "supervisor_env",
        }
    }
}

/// Credentials that actually resolved, plus which source they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTestAutoLogin {
    pub source: TestAutoLoginSource,
    pub email: String,
    pub password: String,
}

/// THE credential resolver. Pure over its inputs (aside from reading the
/// runner's `.env` off `project_dir`) so the precedence is unit-testable
/// without process-global env mutation.
///
/// Priority order (first hit wins):
/// 1. `runtime` — credentials set via `POST /test-login`.
/// 2. `<project_dir>/../.env` → `VITE_DEV_EMAIL` / `VITE_DEV_PASSWORD`.
/// 3. `env_email` / `env_password` — the supervisor's own
///    `QONTINUI_TEST_LOGIN_EMAIL` / `QONTINUI_TEST_LOGIN_PASSWORD` (CI fallback).
///    Blank values at this level count as unset (matching the pre-existing
///    non-empty guard).
pub(crate) fn resolve_test_auto_login(
    runtime: Option<(String, String)>,
    project_dir: &std::path::Path,
    env_email: Option<String>,
    env_password: Option<String>,
) -> Option<ResolvedTestAutoLogin> {
    if let Some((email, password)) = runtime {
        return Some(ResolvedTestAutoLogin {
            source: TestAutoLoginSource::RuntimeApi,
            email,
            password,
        });
    }
    if let Some((email, password)) = read_runner_env_creds(project_dir) {
        return Some(ResolvedTestAutoLogin {
            source: TestAutoLoginSource::RunnerDotEnv,
            email,
            password,
        });
    }
    match (env_email, env_password) {
        (Some(email), Some(password)) if !email.is_empty() && !password.is_empty() => {
            Some(ResolvedTestAutoLogin {
                source: TestAutoLoginSource::SupervisorEnv,
                email,
                password,
            })
        }
        _ => None,
    }
}

/// Async wrapper: read the two live inputs off `SharedState` / the process env
/// and run [`resolve_test_auto_login`]. Shared by [`TestAutoLoginEnv`] (which
/// forwards the creds) and by the routes that REPORT on them, so the report
/// can never disagree with what was actually forwarded.
pub async fn resolve_test_auto_login_for_state(
    state: &SharedState,
) -> Option<ResolvedTestAutoLogin> {
    let runtime = state.test_auto_login.read().await.clone();
    resolve_test_auto_login(
        runtime,
        &state.config.project_dir,
        std::env::var("QONTINUI_TEST_LOGIN_EMAIL").ok(),
        std::env::var("QONTINUI_TEST_LOGIN_PASSWORD").ok(),
    )
}

/// Forwards test-auto-login credentials to spawned runners, resolving them via
/// [`resolve_test_auto_login_for_state`] (runtime API → runner `.env` →
/// supervisor process env).
///
/// SECURITY: dev-account only. The supervisor never targets a production
/// binary, so forwarding to every spawned runner — primary included — is safe.
///
/// NOTE (2026-07): the runner side of this is **inert for login**. The runner's
/// dev email/password auto-login was removed product-wide (`AuthProvider.tsx`
/// now hard-codes `devAutoLoginPending: false`, the `get_test_auto_login` Tauri
/// command is gone, and the backend's `/jwt/login` endpoint no longer exists), so
/// a spawned runner will NOT sign itself in from these vars — it boots Tier 0/1
/// Local. The vars are still forwarded because the runner reads
/// `QONTINUI_TEST_AUTO_LOGIN_EMAIL` as a "this is an automated test runner"
/// marker (e.g. `commands/setup_wizard.rs` skips the wizard on it) and
/// `scripts/headless-launcher.js` consumes them. Read `auto_login_configured` as
/// "the supervisor resolved and forwarded credentials", NOT as "the runner will
/// be authenticated".
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
            if let Some(resolved) = resolve_test_auto_login_for_state(state).await {
                cmd.env("QONTINUI_TEST_AUTO_LOGIN_EMAIL", &resolved.email);
                cmd.env("QONTINUI_TEST_AUTO_LOGIN_PASSWORD", &resolved.password);
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

/// Forwards the plan→work-unit adapter's env knob
/// (`QONTINUI_PLAN_ADAPTER_INTERVAL_SECS`) to the **primary** runner, reading
/// the persisted user environment fresh at each spawn.
///
/// The plans dir itself is NOT forwarded. It is a runner setting —
/// `paths.plans_dir` in the runner's `settings.json`, editable in the runner's
/// Settings → Paths section and read every scan cycle — and the runner reads
/// the old `QONTINUI_PLAN_ADAPTER_DIR` env var only in its one-time migration
/// that persists an env value into settings at first boot. Sequencing note:
/// this forwarder stopped carrying that variable only after the runner release
/// with the migration had booted once on every fleet primary. A Windows primary
/// saw the env var solely through this forwarder, so dropping the entry before
/// the migration ran would have left `paths.plans_dir` unset and the plan tier
/// silently OFF on that machine.
///
/// The interval knob is still read from the runner's process env by the
/// reconcile loop. A spawned runner inherits the SUPERVISOR's env snapshot, and
/// the supervisor is long-lived — an operator `setx` after the supervisor
/// started is invisible to every later runner respawn (observed 2026-07-03).
/// Reading `HKCU\Environment` at spawn time makes `setx` + runner restart
/// sufficient; no supervisor restart required.
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

const PLAN_ADAPTER_VARS: [&str; 1] = ["QONTINUI_PLAN_ADAPTER_INTERVAL_SECS"];

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

/// Env vars whose silent override re-opens the unpaired-spawn bug.
///
/// `process::manager::apply_instance_dir_env` points the child at its
/// per-instance dir with these two, and `routes::runners` writes the
/// `paired_profile_id` snapshot into that same dir. A caller that passes BOTH
/// `paired_profile_id` and an `extra_env` override for one of these gets the
/// profile written to the instance dir while the child reads somewhere else —
/// a spawn that reports `paired_profile_applied` and comes up unpaired. The
/// override is still honored (that is `extra_env`'s contract), but it is never
/// silent.
const INSTANCE_DIR_ENV_KEYS: [&str; 2] = ["QONTINUI_CONFIG_DIR", "QONTINUI_SECURE_STORAGE_DIR"];

/// Apply the caller-supplied `extra_env` map to the spawn command.
///
/// MUST run last. Callers (e.g. `POST /runners/spawn-test` with
/// `{"extra_env": {"QONTINUI_SCRIPTED_OUTPUT": "1"}}`) override anything
/// the supervisor set, including `QONTINUI_SERVER_MODE`, `QONTINUI_API_URL`,
/// etc. See `config.rs:148-153` for the documented contract.
///
/// Overriding one of [`INSTANCE_DIR_ENV_KEYS`] is warned about — see there.
pub struct ExtraEnv;

impl EnvForwarder for ExtraEnv {
    fn name(&self) -> &'static str {
        "extra_env"
    }

    fn apply<'a>(
        &'a self,
        cmd: &'a mut Command,
        state: &'a SharedState,
        runner: &'a ManagedRunner,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            for (k, v) in &runner.config.extra_env {
                if let Some(msg) = instance_dir_override_warning(cmd, &runner.config.name, k, v) {
                    tracing::warn!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                        .await;
                }
                cmd.env(k, v);
            }
        })
    }
}

/// Build the warning for an `extra_env` entry that would redirect one of the
/// [`INSTANCE_DIR_ENV_KEYS`] away from the per-instance dir the supervisor
/// already set. Returns `None` when the key is unrelated, was never set by the
/// supervisor (nothing is being overridden), or the value is identical.
///
/// Names BOTH values so the reader can see the split at a glance.
fn instance_dir_override_warning(
    cmd: &Command,
    runner_name: &str,
    key: &str,
    new_value: &str,
) -> Option<String> {
    if !INSTANCE_DIR_ENV_KEYS.contains(&key) {
        return None;
    }
    let key_os = std::ffi::OsStr::new(key);
    let current = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == key_os)
        .and_then(|(_, v)| v)?
        .to_string_lossy()
        .into_owned();
    if current == new_value {
        return None;
    }
    Some(format!(
        "extra_env override: runner '{runner_name}' redirects {key} from the supervisor's \
         per-instance dir '{current}' to '{new_value}'. A `paired_profile_id` snapshot is \
         copied into '{current}', so the child will read a directory nothing was written to \
         and come up UNPAIRED. Honoring the override as documented — drop it if you wanted \
         the paired profile."
    ))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::{
        display_refusal, display_refusal_for_command, resolve_display_env,
        resolve_plan_adapter_value, resolve_test_auto_login, resolve_worktree_mode,
        setup_wizard_bypass_value, TestAutoLoginSource, PLAN_ADAPTER_VARS,
    };
    use std::path::{Path, PathBuf};

    /// Build a `<root>/qontinui-runner/.env` with the given body and return the
    /// `project_dir` (`.../src-tauri`) the resolver expects — `.env` lives at
    /// `project_dir.parent()`.
    fn project_dir_with_env(tmp: &Path, body: &str) -> PathBuf {
        let runner = tmp.join("qontinui-runner");
        let project_dir = runner.join("src-tauri");
        std::fs::create_dir_all(&project_dir).expect("mkdir project_dir");
        std::fs::write(runner.join(".env"), body).expect("write .env");
        project_dir
    }

    #[test]
    fn runtime_creds_win_over_dotenv_and_process_env() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let project_dir = project_dir_with_env(
            tmp.path(),
            "VITE_DEV_EMAIL=dotenv@x.io\nVITE_DEV_PASSWORD=dotenvpw\n",
        );
        let r = resolve_test_auto_login(
            Some(("rt@x.io".into(), "rtpw".into())),
            &project_dir,
            Some("env@x.io".into()),
            Some("envpw".into()),
        )
        .expect("must resolve");
        assert_eq!(r.source, TestAutoLoginSource::RuntimeApi);
        assert_eq!(r.email, "rt@x.io");
    }

    #[test]
    fn dotenv_resolves_when_no_runtime_creds() {
        // THE regression this item is about: with no `POST /test-login` creds
        // but a runner `.env` present, the supervisor DOES forward credentials
        // — so the report must be `Some`, not the old hard-coded `false`.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let project_dir = project_dir_with_env(
            tmp.path(),
            "# comment\nVITE_DEV_EMAIL=\"dotenv@x.io\"\nVITE_DEV_PASSWORD='dotenvpw'\n",
        );
        let r = resolve_test_auto_login(None, &project_dir, None, None).expect("must resolve");
        assert_eq!(r.source, TestAutoLoginSource::RunnerDotEnv);
        assert_eq!(r.email, "dotenv@x.io");
        assert_eq!(r.password, "dotenvpw");
    }

    #[test]
    fn process_env_is_last_resort() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // `.env` exists but carries no dev creds → fall through to process env.
        let project_dir = project_dir_with_env(tmp.path(), "VITE_API_URL=http://localhost\n");
        let r = resolve_test_auto_login(
            None,
            &project_dir,
            Some("env@x.io".into()),
            Some("envpw".into()),
        )
        .expect("must resolve");
        assert_eq!(r.source, TestAutoLoginSource::SupervisorEnv);
        assert_eq!(r.email, "env@x.io");
    }

    #[test]
    fn nothing_configured_resolves_to_none() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let project_dir = project_dir_with_env(tmp.path(), "");
        // Fully unset, and blank process-env values count as unset (a half-set
        // pair must not resolve either).
        assert!(resolve_test_auto_login(None, &project_dir, None, None).is_none());
        assert!(resolve_test_auto_login(
            None,
            &project_dir,
            Some(String::new()),
            Some(String::new())
        )
        .is_none());
        assert!(
            resolve_test_auto_login(None, &project_dir, Some("env@x.io".into()), None).is_none()
        );
    }

    #[test]
    fn wire_strings_are_stable() {
        // These strings are API surface (`auth_state.auto_login_source`,
        // `GET /test-login`.source) — pin them.
        assert_eq!(TestAutoLoginSource::RuntimeApi.as_str(), "runtime_api");
        assert_eq!(TestAutoLoginSource::RunnerDotEnv.as_str(), "runner_dotenv");
        assert_eq!(
            TestAutoLoginSource::SupervisorEnv.as_str(),
            "supervisor_env"
        );
    }

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
    fn plan_adapter_forwards_only_the_interval_knob() {
        // The plans dir is a runner setting (`paths.plans_dir`), not an env
        // var this forwarder carries; the reconcile interval is the one
        // remaining env knob the runner's loop still reads.
        assert_eq!(PLAN_ADAPTER_VARS, ["QONTINUI_PLAN_ADAPTER_INTERVAL_SECS"]);
    }

    #[test]
    fn plan_adapter_persisted_beats_process_snapshot() {
        // The freshly-read persisted value wins over the (possibly stale)
        // process-env snapshot — the whole point of reading at spawn time.
        assert_eq!(
            resolve_plan_adapter_value(Some("120".to_string()), Some("60".to_string())),
            Some("120".to_string())
        );
    }

    #[test]
    fn plan_adapter_process_env_is_fallback() {
        // No persisted value (non-Windows, or never setx'd) → the supervisor's
        // process env still works, so explicit launches keep functioning.
        assert_eq!(
            resolve_plan_adapter_value(None, Some("60".to_string())),
            Some("60".to_string())
        );
        assert_eq!(resolve_plan_adapter_value(None, None), None);
    }

    #[test]
    fn plan_adapter_blank_values_count_as_unset() {
        // A blank registry entry must not mask a real process-env value, and
        // blank-everywhere resolves to no injection at all.
        assert_eq!(
            resolve_plan_adapter_value(Some("  ".to_string()), Some("60".to_string())),
            Some("60".to_string())
        );
        assert_eq!(
            resolve_plan_adapter_value(Some(String::new()), Some(String::new())),
            None
        );
    }

    // --- DisplayEnv (S-3) ---------------------------------------------------

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn display_forwards_every_present_supervisor_key() {
        let sup = [
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("GDK_BACKEND", "x11"),
            ("BROADWAY_DISPLAY", ":5"),
        ];
        let got = resolve_display_env(env_of(&sup), Some(":9"));
        assert_eq!(
            got,
            vec![
                ("DISPLAY", ":0".to_string()),
                ("WAYLAND_DISPLAY", "wayland-0".to_string()),
                ("GDK_BACKEND", "x11".to_string()),
                ("BROADWAY_DISPLAY", ":5".to_string()),
            ],
            "the knob must not override a DISPLAY the supervisor already has"
        );
    }

    #[test]
    fn display_knob_supplies_display_only_when_supervisor_has_none() {
        let sup = [("GDK_BACKEND", "x11"), ("DISPLAY", "  ")];
        let got = resolve_display_env(env_of(&sup), Some(":1"));
        assert_eq!(
            got,
            vec![
                ("GDK_BACKEND", "x11".to_string()),
                ("DISPLAY", ":1".to_string())
            ]
        );
        // Wayland-only supervisor: DISPLAY still comes from the knob.
        let sup = [("WAYLAND_DISPLAY", "wayland-1")];
        let got = resolve_display_env(env_of(&sup), Some(":0"));
        assert!(got.contains(&("DISPLAY", ":0".to_string())));
        assert!(got.contains(&("WAYLAND_DISPLAY", "wayland-1".to_string())));
    }

    #[test]
    fn display_nothing_anywhere_resolves_to_nothing() {
        assert!(resolve_display_env(env_of(&[]), None).is_empty());
        assert!(resolve_display_env(env_of(&[]), Some("  ")).is_empty());
    }

    #[test]
    fn no_display_refusal_names_the_knob() {
        let msg = display_refusal(true, true, "test-1", |_| None, |_| None)
            .expect("no display anywhere must refuse on Linux");
        assert!(msg.starts_with("no_display:"), "{msg}");
        assert!(msg.contains("--temp-runner-display"), "{msg}");
        assert!(msg.contains("QONTINUI_SUPERVISOR_TEMP_DISPLAY"), "{msg}");
    }

    #[test]
    fn no_display_refusal_is_linux_temp_only() {
        assert_eq!(display_refusal(false, true, "t", |_| None, |_| None), None);
        assert_eq!(display_refusal(true, false, "n", |_| None, |_| None), None);
    }

    #[test]
    fn display_resolution_sees_command_inherited_and_removed_values() {
        // Explicit on the command (knob or extra_env) resolves.
        let explicit = |k: &str| (k == "DISPLAY").then(|| Some(":7".to_string()));
        assert_eq!(display_refusal(true, true, "t", explicit, |_| None), None);
        // Inherited from the supervisor resolves when the command is silent.
        let inherited = |k: &str| (k == "WAYLAND_DISPLAY").then(|| "wayland-0".to_string());
        assert_eq!(display_refusal(true, true, "t", |_| None, inherited), None);
        // An extra_env that REMOVES / blanks the inherited value refuses.
        let removed = |k: &str| (k == "DISPLAY").then_some(None);
        let inherited_display = |k: &str| (k == "DISPLAY").then(|| ":0".to_string());
        assert!(display_refusal(true, true, "t", removed, inherited_display).is_some());
        let blanked = |k: &str| (k == "DISPLAY").then(|| Some(String::new()));
        assert!(display_refusal(true, true, "t", blanked, |_| None).is_some());
    }

    /// The command-reading wrapper judges the FINAL command env: a value set
    /// on the `Command` (as `extra_env` or the knob would) satisfies it.
    #[test]
    fn display_refusal_for_command_reads_the_final_command_env() {
        let mut temp = crate::config::RunnerConfig::default_primary();
        temp.id = "test-x".to_string();
        temp.name = "test-x".to_string();
        temp.kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
            id: "test-x".to_string(),
        };
        let mut cmd = tokio::process::Command::new("true");
        cmd.env("DISPLAY", ":3");
        assert_eq!(display_refusal_for_command(&cmd, &temp), None);

        // Removing both on the command refuses on Linux, whatever the
        // supervisor's own env holds.
        let mut cmd = tokio::process::Command::new("true");
        cmd.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
        let refusal = display_refusal_for_command(&cmd, &temp);
        if cfg!(target_os = "linux") {
            assert!(refusal.expect("must refuse").starts_with("no_display:"));
        } else {
            assert_eq!(refusal, None);
        }
    }

    /// The pre-build check refuses exactly what the post-forwarder check
    /// would: nothing anywhere refuses; the knob, an inherited value, or an
    /// `extra_env` value each satisfy it; an `extra_env` blank still refuses.
    #[test]
    fn display_preflight_matches_the_final_resolution() {
        use super::display_preflight_refusal;
        use std::collections::HashMap;
        let none: HashMap<String, String> = HashMap::new();

        let msg = display_preflight_refusal(true, "t", &none, |_| None, None)
            .expect("no display anywhere must refuse before the build");
        assert!(msg.starts_with("no_display:"), "{msg}");

        assert_eq!(
            display_preflight_refusal(true, "t", &none, |_| None, Some(":0")),
            None,
            "the knob supplies DISPLAY"
        );
        assert_eq!(
            display_preflight_refusal(
                true,
                "t",
                &none,
                env_of(&[("WAYLAND_DISPLAY", "w-0")]),
                None
            ),
            None,
            "an inherited WAYLAND_DISPLAY resolves"
        );
        let extra: HashMap<String, String> = [("DISPLAY".to_string(), ":5".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            display_preflight_refusal(true, "t", &extra, |_| None, None),
            None,
            "extra_env DISPLAY resolves"
        );
        let blank: HashMap<String, String> = [("DISPLAY".to_string(), String::new())]
            .into_iter()
            .collect();
        assert!(
            display_preflight_refusal(true, "t", &blank, env_of(&[("DISPLAY", ":0")]), None)
                .is_some(),
            "an extra_env blank overrides the inherited DISPLAY and refuses"
        );
        assert_eq!(
            display_preflight_refusal(false, "t", &none, |_| None, None),
            None,
            "never refuses off Linux"
        );
    }

    // --- SetupWizardBypassEnv (UI-5) -----------------------------------------

    #[test]
    fn setup_wizard_bypass_is_set_for_temp_runners_only() {
        assert_eq!(setup_wizard_bypass_value(true), Some("1"));
        assert_eq!(setup_wizard_bypass_value(false), None);
        assert_eq!(
            super::SETUP_WIZARD_BYPASS_ENV,
            "QONTINUI_SETUP_WIZARD_BYPASS"
        );
    }

    /// Registration order: both new forwarders run before `ExtraEnv`, which
    /// stays last so `extra_env` can override them.
    #[test]
    fn display_and_bypass_forwarders_are_registered_before_extra_env() {
        let names: Vec<&str> = super::default_env_forwarders()
            .iter()
            .map(|f| f.name())
            .collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).expect(n);
        assert_eq!(names.last(), Some(&"extra_env"));
        assert!(pos("display") < pos("extra_env"));
        assert!(pos("setup_wizard_bypass") < pos("extra_env"));
    }
}

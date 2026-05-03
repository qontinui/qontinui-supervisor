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
        Box::new(WindowPositionEnv),
        Box::new(RestateEnv),
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
            if runner.config.is_primary {
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

            let url =
                format!("http://localhost:9876/spawn-placement/temp?index={index}&overflow=wrap");
            let resp = match state
                .http_client
                .get(&url)
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!(
                        "Spawn temp placement: skipping runner '{}' — request to {} failed: {}",
                        runner_name, url, e
                    );
                    tracing::info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
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

            #[derive(serde::Deserialize)]
            struct PlacementPreview {
                global_x: i32,
                global_y: i32,
                width: u32,
                height: u32,
                #[serde(default)]
                monitor_label: Option<String>,
                #[serde(default)]
                slot_label: Option<String>,
                #[serde(default)]
                source: Option<String>,
                /// Per-placement decorations override. `None` = use runner default
                /// (chrome on); `Some(true)` = chrome on; `Some(false)` = borderless.
                /// Forwarded to the runner as `QONTINUI_WINDOW_DECORATIONS=0|1`.
                #[serde(default)]
                decorations: Option<bool>,
            }

            // Runner endpoints wrap responses in `{success, data, error?}`.
            // Deserialize the envelope and pull `data` out; fall back to the
            // bare-payload only if there's no envelope (forward-compat).
            #[derive(serde::Deserialize)]
            #[serde(untagged)]
            enum PlacementResponse {
                Wrapped {
                    #[serde(default)]
                    success: bool,
                    #[serde(default)]
                    data: Option<PlacementPreview>,
                    #[serde(default)]
                    error: Option<String>,
                },
                Bare(PlacementPreview),
            }

            let placement: PlacementPreview = match resp.json::<PlacementResponse>().await {
                Ok(PlacementResponse::Wrapped {
                    success,
                    data: Some(p),
                    ..
                }) if success => p,
                Ok(PlacementResponse::Wrapped { error, .. }) => {
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
                Ok(PlacementResponse::Bare(p)) => p,
                Err(e) => {
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

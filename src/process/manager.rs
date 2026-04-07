use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::{GRACEFUL_KILL_TIMEOUT_SECS, RUNNER_VITE_PORT};
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::port::wait_for_port_free;
use crate::process::windows::{
    clear_webview2_cache, kill_by_pid, kill_by_port, kill_by_port_tree,
    kill_webview2_processes, remove_webview2_user_data_folder, webview2_user_data_folder,
};
use crate::state::{ManagedRunner, SharedState};

// =============================================================================
// Runner Category Helpers
// =============================================================================

/// Returns true if this runner is a temp/test runner managed by the supervisor.
/// Only temp runners can be started, stopped, or restarted by the supervisor.
/// All other runners (primary, user-opened) are observe-only.
pub fn is_temp_runner(runner_id: &str) -> bool {
    runner_id.starts_with("test-")
}

// =============================================================================
// Startup Cleanup
// =============================================================================

/// Kill any orphaned temp runner processes from previous supervisor sessions.
/// Only cleans up temp runner ports — user runners are never touched.
pub async fn cleanup_orphaned_runners(state: &SharedState) {
    let runners = state.get_all_runners().await;

    // Only clean up temp runner ports
    let mut ports: Vec<u16> = Vec::new();
    for r in &runners {
        if is_temp_runner(&r.config.id) {
            ports.push(r.config.port);
        } else {
            // Mark non-temp runners as running if something is on their port,
            // so the supervisor tracks their health without managing them.
            if crate::process::port::is_port_in_use(r.config.port) {
                info!(
                    "Runner '{}' (port {}) already running — tracking health only",
                    r.config.name, r.config.port
                );
                let mut runner = r.runner.write().await;
                runner.running = true;
            }
        }
    }

    let mut killed_any = false;
    for &port in &ports {
        if let Ok(true) = kill_by_port(port).await {
            info!("Killed orphaned temp runner on port {}", port);
            killed_any = true;
        }
    }

    if killed_any {
        tokio::time::sleep(Duration::from_secs(1)).await;
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                "Cleaned up orphaned temp runner processes",
            )
            .await;
    }
}

// =============================================================================
// Per-Runner Process Management (multi-runner)
// =============================================================================

/// Start a specific runner by ID.
pub async fn start_runner_by_id(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

    // Check if build is in progress
    {
        let build = state.build.read().await;
        if build.build_in_progress {
            return Err(SupervisorError::BuildInProgress);
        }
    }

    {
        let runner = managed.runner.read().await;
        if runner.running {
            return Err(SupervisorError::RunnerAlreadyRunning);
        }
    }

    let is_primary = managed.config.is_primary;
    let port = managed.config.port;
    let runner_name = managed.config.name.clone();

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Starting runner '{}' (port {}) in {} mode",
                runner_name,
                port,
                if is_primary && state.config.dev_mode {
                    "dev"
                } else {
                    "exe"
                }
            ),
        )
        .await;

    // Primary in dev mode uses `npm run tauri dev` (Vite + Tauri).
    // Non-primary runners always use exe mode — only one Vite instance can run at a time,
    // and discovered runners share the compiled binary from the primary's build.
    let mut child = if is_primary && state.config.dev_mode {
        start_dev_mode_for_runner(state, &managed).await?
    } else {
        start_exe_mode_for_runner(state, &managed).await?
    };

    let pid = child.id();
    info!(
        "Runner '{}' started with PID {:?} on port {}",
        runner_name, pid, port
    );

    // Capture stdout/stderr to the managed runner's logs
    if let Some(stdout) = child.stdout.take() {
        crate::log_capture::spawn_stdout_reader(stdout, &managed.logs);
    }
    if let Some(stderr) = child.stderr.take() {
        crate::log_capture::spawn_stderr_reader(stderr, &managed.logs);
    }

    // Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.process = Some(child);
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
    }

    // If this is the primary runner, also update legacy state for backward compat
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
        // process stays None in legacy — managed runner owns it
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Runner '{}' process started (PID: {:?}, port: {})",
                runner_name, pid, port
            ),
        )
        .await;

    state.notify_health_change();
    managed.health_cache_notify.notify_one();

    // Spawn a task to monitor the process exit
    let state_clone = state.clone();
    let managed_clone = managed.clone();
    let runner_id_owned = runner_id.to_string();
    tokio::spawn(async move {
        monitor_runner_process_exit(state_clone, managed_clone, runner_id_owned).await;
    });

    Ok(())
}

/// Start exe mode for a specific runner with port/name env vars.
async fn start_exe_mode_for_runner(
    state: &SharedState,
    managed: &ManagedRunner,
) -> Result<tokio::process::Child, SupervisorError> {
    let source_exe = state.config.runner_exe_path();

    if !source_exe.exists() {
        return Err(SupervisorError::Process(format!(
            "Runner exe not found at {:?}. Run a build first.",
            source_exe
        )));
    }

    // All runners use a copy of the exe to avoid locking the build artifact.
    // This allows cargo build to succeed while any runner is running.
    let exe_path = {
        let copy_path = state.config.runner_exe_copy_path(&managed.config.id);
        if let Err(e) = std::fs::copy(&source_exe, &copy_path) {
            warn!(
                "Failed to copy runner exe for '{}': {} — falling back to original",
                managed.config.name, e
            );
            source_exe
        } else {
            info!(
                "Copied runner exe for '{}' to {:?}",
                managed.config.name, copy_path
            );
            copy_path
        }
    };

    info!(
        "Starting runner '{}' in exe mode from {:?} on port {}",
        managed.config.name, exe_path, managed.config.port
    );

    #[cfg(windows)]
    let child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut cmd = Command::new(&exe_path);
        cmd.current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .env_remove("CLAUDECODE")
            .env("QONTINUI_PORT", managed.config.port.to_string())
            .env("QONTINUI_API_URL", "http://127.0.0.1:8000");

        // Non-primary runners get QONTINUI_INSTANCE_NAME to skip scheduler
        // and QONTINUI_PRIMARY_PORT so they can proxy process commands to the primary
        if !managed.config.is_primary {
            cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
            // Find the primary runner's port for process log proxying
            if let Some(primary) = state.get_primary().await {
                cmd.env("QONTINUI_PRIMARY_PORT", primary.config.port.to_string());
            }
        }

        // Per-runner WebView2 data dir — non-primary runners get isolated
        // localStorage, IndexedDB, cookies, and caches. Primary keeps the
        // default path so its existing state (auth, terminal layouts, etc.)
        // is preserved. This prevents state bleed-over when spawning temp
        // test runners and eliminates the "216 restored terminals" problem
        // where one runner's persisted UI state floods every other runner.
        if let Some(webview_dir) =
            webview2_user_data_folder(&managed.config.id, managed.config.is_primary)
        {
            if !managed.config.is_primary {
                // Ensure the folder exists so WebView2 doesn't race to create
                // it against the parent dir's permissions.
                if let Err(e) = std::fs::create_dir_all(&webview_dir) {
                    warn!(
                        "Failed to pre-create WebView2 data dir {:?} for runner '{}': {}",
                        webview_dir, managed.config.name, e
                    );
                }
                info!(
                    "Runner '{}' using isolated WebView2 data dir: {:?}",
                    managed.config.name, webview_dir
                );
                cmd.env("WEBVIEW2_USER_DATA_FOLDER", webview_dir);
            }
        }

        cmd.spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?
    };

    #[cfg(not(windows))]
    let child = {
        let mut cmd = Command::new(&exe_path);
        cmd.current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .env("QONTINUI_PORT", managed.config.port.to_string())
            .env("QONTINUI_API_URL", "http://127.0.0.1:8000");

        if !managed.config.is_primary {
            cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
            if let Some(primary) = state.get_primary().await {
                cmd.env("QONTINUI_PRIMARY_PORT", primary.config.port.to_string());
            }
            // Per-runner WebView2 data dir (see Windows branch for rationale).
            // On non-Windows the variable is ignored by other webview backends,
            // so this is harmless but keeps behavior consistent.
            if let Some(webview_dir) =
                webview2_user_data_folder(&managed.config.id, false)
            {
                let _ = std::fs::create_dir_all(&webview_dir);
                cmd.env("WEBVIEW2_USER_DATA_FOLDER", webview_dir);
            }
        }

        cmd.spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?
    };

    Ok(child)
}

/// Monitor a specific runner's process for exit.
async fn monitor_runner_process_exit(
    state: SharedState,
    managed: Arc<ManagedRunner>,
    _runner_id: String,
) {
    let is_primary = managed.config.is_primary;
    let runner_name = managed.config.name.clone();

    // Take the child out of state so we can await without holding the lock.
    let child = {
        let mut runner = managed.runner.write().await;
        runner.process.take()
    };

    let exit_status = if let Some(mut child) = child {
        match child.wait().await {
            Ok(status) => Some(status),
            Err(e) => {
                error!("Error waiting for runner '{}' process: {}", runner_name, e);
                None
            }
        }
    } else {
        None
    };

    // Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
    }

    // Update legacy state for primary
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
    }

    state.notify_health_change();

    if let Some(status) = exit_status {
        let msg = if status.success() {
            format!("Runner '{}' process exited normally", runner_name)
        } else {
            format!(
                "Runner '{}' process exited with status: {}",
                runner_name, status
            )
        };

        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, &msg)
            .await;
        info!("{}", msg);
    } else {
        let msg = format!("Runner '{}' process terminated unexpectedly", runner_name);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, &msg)
            .await;
        warn!("{}", msg);
    }
}

/// Stop a specific runner by ID. Kills by PID (not by process name).
pub async fn stop_runner_by_id(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

    let runner_name = managed.config.name.clone();
    let port = managed.config.port;
    let is_primary = managed.config.is_primary;

    {
        let mut runner = managed.runner.write().await;
        runner.stop_requested = true;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Stopping runner '{}'...", runner_name),
        )
        .await;

    // 1. Kill the child process by PID
    let child = {
        let mut runner = managed.runner.write().await;
        runner.process.take()
    };

    let pid_to_kill = {
        let runner = managed.runner.read().await;
        runner.pid
    };

    if let Some(mut child) = child {
        info!("Killing runner '{}' child process", runner_name);
        let _ = child.kill().await;

        let wait_result = tokio::time::timeout(
            Duration::from_secs(GRACEFUL_KILL_TIMEOUT_SECS),
            child.wait(),
        )
        .await;

        match wait_result {
            Ok(Ok(_)) => info!("Runner '{}' process exited gracefully", runner_name),
            Ok(Err(e)) => warn!("Error waiting for runner '{}': {}", runner_name, e),
            Err(_) => warn!("Runner '{}' did not exit within timeout", runner_name),
        }
    }

    // 2. If we have a PID, try to kill it directly (may have been a grandchild)
    if let Some(pid) = pid_to_kill {
        let _ = kill_by_pid(pid).await;
    }

    // 3. Clean up the runner's port
    let port_free = wait_for_port_free(port, 5).await;
    if !port_free {
        warn!(
            "Port {} still in use after stopping runner '{}', force-killing",
            port, runner_name
        );
        let _ = kill_by_port(port).await;
    }

    // For primary in dev mode, always force-kill the Vite process tree.
    // Use tree-kill (/T) to kill the entire cmd→npm→tauri→node chain, since
    // the grandchild node.exe (Vite) often survives parent kills despite
    // CREATE_NEW_PROCESS_GROUP. Without tree-kill, the old Vite process keeps
    // running with stale in-memory module transforms after code changes.
    if is_primary && state.config.dev_mode {
        let _ = kill_by_port_tree(RUNNER_VITE_PORT).await;
        let vite_free = wait_for_port_free(RUNNER_VITE_PORT, 5).await;
        if !vite_free {
            warn!(
                "Vite port {} still in use after tree-kill, retrying with port kill",
                RUNNER_VITE_PORT
            );
            let _ = kill_by_port(RUNNER_VITE_PORT).await;
            let vite_free_retry = wait_for_port_free(RUNNER_VITE_PORT, 10).await;
            if !vite_free_retry {
                error!(
                    "Vite port {} still in use after two kill attempts",
                    RUNNER_VITE_PORT
                );
            }
        }

        // Kill WebView2 processes and clear their cache so the webview
        // fetches fresh JavaScript modules on next startup. WebView2 on
        // Windows aggressively caches ES modules (ignoring no-store) and
        // the cached bytecode survives process restarts via the EBWebView
        // profile directory.
        let _ = kill_webview2_processes().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = clear_webview2_cache().await;
    }

    // 4. Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
        runner.stop_requested = false;
    }

    // Update legacy state for primary
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
        runner.stop_requested = false;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Runner '{}' stopped", runner_name),
        )
        .await;
    info!("Runner '{}' stopped", runner_name);

    // Auto-remove ephemeral test runners (spawned via /runners/spawn-test)
    // from the runners map so they don't accumulate over time. These have IDs
    // prefixed with "test-" and are not persisted to settings.
    let runner_id = managed.config.id.clone();
    if runner_id.starts_with("test-") {
        let mut runners = state.runners.write().await;
        if runners.remove(&runner_id).is_some() {
            info!(
                "Removed ephemeral test runner '{}' (id: {}) from state",
                runner_name, runner_id
            );
        }
        drop(runners);
        // Also remove the test runner's isolated WebView2 data folder so its
        // localStorage, cookies, and caches don't accumulate on disk.
        #[cfg(windows)]
        {
            if let Err(e) = remove_webview2_user_data_folder(&runner_id, false).await {
                warn!(
                    "Failed to remove WebView2 data folder for test runner '{}': {}",
                    runner_id, e
                );
            }
        }
    }

    state.notify_health_change();
    managed.health_cache_notify.notify_one();

    Ok(())
}

/// Restart a specific runner by ID.
/// Automated sources (watchdog, workflow loop, smart rebuild) are rejected for
/// non-temp runners — only manual API calls can restart user runners.
pub async fn restart_runner_by_id(
    state: &SharedState,
    runner_id: &str,
    rebuild: bool,
    source: RestartSource,
    _force: bool,
) -> Result<(), SupervisorError> {
    if !is_temp_runner(runner_id) && !source.is_manual() {
        let msg = format!(
            "Automated restart of non-temp runner '{}' blocked (source: {}). \
             Only manual restarts are allowed for user runners.",
            runner_id, source
        );
        warn!("{}", msg);
        return Err(SupervisorError::Validation(msg));
    }

    let restart_start = std::time::Instant::now();

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::RestartStarted {
            source: source.clone(),
            rebuild,
        });

    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

    {
        let mut runner = managed.runner.write().await;
        runner.restart_requested = true;
    }

    // Stop if running
    {
        let runner = managed.runner.read().await;
        if runner.running {
            drop(runner);
            if let Err(e) = stop_runner_by_id(state, runner_id).await {
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::RestartFailed {
                        source,
                        error: e.to_string(),
                    });
                return Err(e);
            }
        }
    }

    // Rebuild if requested (global — single binary)
    let build_duration = if rebuild {
        let build_start = std::time::Instant::now();
        if let Err(e) = crate::build_monitor::run_cargo_build(state).await {
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartFailed {
                    source,
                    error: e.to_string(),
                });
            return Err(e);
        }
        Some(build_start.elapsed().as_secs_f64())
    } else {
        None
    };

    // Start
    if let Err(e) = start_runner_by_id(state, runner_id).await {
        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::RestartFailed {
                source,
                error: e.to_string(),
            });
        return Err(e);
    }

    {
        let mut runner = managed.runner.write().await;
        runner.restart_requested = false;
    }

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::RestartCompleted {
            source,
            rebuild,
            duration_secs: restart_start.elapsed().as_secs_f64(),
            build_duration_secs: build_duration,
        });

    Ok(())
}

/// Stop all runners. Primary is stopped last.
/// Stop all temp runners. User runners (primary and secondary) are never touched.
pub async fn stop_all_temp_runners(state: &SharedState) -> Result<(), SupervisorError> {
    let runners = state.get_all_runners().await;
    let mut errors = Vec::new();

    for managed in &runners {
        if !is_temp_runner(&managed.config.id) {
            continue;
        }
        let running = managed.runner.read().await.running;
        if running {
            if let Err(e) = stop_runner_by_id(state, &managed.config.id).await {
                errors.push(format!("'{}': {}", managed.config.name, e));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(SupervisorError::Other(format!(
            "Errors stopping temp runners: {}",
            errors.join("; ")
        )))
    }
}

/// Restart all runners. Stop all, optionally rebuild, start all (primary first).
#[allow(dead_code)]
pub async fn restart_all(
    state: &SharedState,
    rebuild: bool,
    _source: RestartSource,
) -> Result<(), SupervisorError> {
    // Collect which runners were running before stop
    let runners = state.get_all_runners().await;
    let mut was_running = Vec::new();
    for managed in &runners {
        let running = managed.runner.read().await.running;
        if running {
            was_running.push(managed.config.id.clone());
        }
    }

    stop_all_temp_runners(state).await?;

    if rebuild {
        crate::build_monitor::run_cargo_build(state).await?;
    }

    // Start primary first
    for managed in &runners {
        if managed.config.is_primary && was_running.contains(&managed.config.id) {
            start_runner_by_id(state, &managed.config.id).await?;
        }
    }

    // Then start non-primary with 2s delay
    for managed in &runners {
        if !managed.config.is_primary && was_running.contains(&managed.config.id) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            start_runner_by_id(state, &managed.config.id).await?;
        }
    }

    Ok(())
}

// =============================================================================
// Legacy single-runner functions (backward compat — delegate to primary runner)
// =============================================================================

/// Start a runner in dev mode (`npm run tauri dev`), which launches both
/// the Vite dev server and the Tauri/Rust backend.
/// Works for both primary and discovered runners.
async fn start_dev_mode_for_runner(
    state: &SharedState,
    managed: &ManagedRunner,
) -> Result<tokio::process::Child, SupervisorError> {
    let npm_dir = state.config.runner_npm_dir();
    let port = managed.config.port;

    info!(
        "Starting runner '{}' in dev mode from {:?} (port {})",
        managed.config.name, npm_dir, port
    );

    #[cfg(windows)]
    let child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "npm.cmd run tauri dev -- --no-watch"])
            .current_dir(&npm_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .env_remove("CLAUDECODE")
            .env("QONTINUI_PORT", port.to_string());

        // Non-primary runners get instance env vars so they skip scheduler
        // and can proxy process commands to the primary
        if !managed.config.is_primary {
            cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
            if let Some(primary) = state.get_primary().await {
                cmd.env("QONTINUI_PRIMARY_PORT", primary.config.port.to_string());
            }
        }

        cmd.spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn dev mode: {}", e)))?
    };

    #[cfg(not(windows))]
    let child = {
        let mut cmd = Command::new("npx");
        cmd.args(["tauri", "dev", "--no-watch"])
            .current_dir(&npm_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .env("QONTINUI_PORT", port.to_string());

        if !managed.config.is_primary {
            cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
            if let Some(primary) = state.get_primary().await {
                cmd.env("QONTINUI_PRIMARY_PORT", primary.config.port.to_string());
            }
        }

        cmd.spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn dev mode: {}", e)))?
    };

    Ok(child)
}

/// Stop the runner process (primary). Attempts graceful shutdown, then force kill.
/// Legacy stop — targets the primary runner. Allowed for manual use.
pub async fn stop_runner(state: &SharedState, _force: bool) -> Result<(), SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::Other("No primary runner configured".to_string()))?;

    stop_runner_by_id(state, &primary.config.id).await
}

/// Legacy restart wrapper — targets the primary runner.
/// Only manual restarts are allowed; automated sources are rejected.
pub async fn restart_runner(
    state: &SharedState,
    rebuild: bool,
    source: RestartSource,
    force: bool,
) -> Result<(), SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::Other("No primary runner configured".to_string()))?;

    restart_runner_by_id(state, &primary.config.id, rebuild, source, force).await
}

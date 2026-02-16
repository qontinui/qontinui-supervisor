use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::{GRACEFUL_KILL_TIMEOUT_SECS, RUNNER_API_PORT, RUNNER_VITE_PORT};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::port::wait_for_port_free;
use crate::process::windows::{kill_by_port, kill_runner_comprehensive};
use crate::state::SharedState;

/// Start the runner process. Returns error if already running.
pub async fn start_runner(state: &SharedState) -> Result<(), SupervisorError> {
    {
        let runner = state.runner.read().await;
        if runner.running {
            return Err(SupervisorError::RunnerAlreadyRunning);
        }
    }

    // Check if build is in progress
    {
        let build = state.build.read().await;
        if build.build_in_progress {
            return Err(SupervisorError::BuildInProgress);
        }
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Starting runner in {} mode",
                if state.config.dev_mode { "dev" } else { "exe" }
            ),
        )
        .await;

    let mut child = if state.config.dev_mode {
        start_dev_mode(state).await?
    } else {
        start_exe_mode(state).await?
    };

    let pid = child.id();
    info!("Runner started with PID {:?}", pid);

    // Capture stdout/stderr
    if let Some(stdout) = child.stdout.take() {
        crate::log_capture::spawn_stdout_reader(stdout, &state.logs);
    }
    if let Some(stderr) = child.stderr.take() {
        crate::log_capture::spawn_stderr_reader(stderr, &state.logs);
    }

    // Update state
    {
        let mut runner = state.runner.write().await;
        runner.process = Some(child);
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Runner process started (PID: {:?})", pid),
        )
        .await;

    // Spawn a task to monitor the process exit
    let state_clone = state.clone();
    tokio::spawn(async move {
        monitor_process_exit(state_clone).await;
    });

    Ok(())
}

async fn start_dev_mode(state: &SharedState) -> Result<tokio::process::Child, SupervisorError> {
    let npm_dir = state.config.runner_npm_dir();

    info!("Starting in dev mode from {:?}", npm_dir);

    #[cfg(windows)]
    let child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        Command::new("cmd")
            .args(["/C", "npm.cmd run tauri dev -- --no-watch"])
            .current_dir(&npm_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn dev mode: {}", e)))?
    };

    #[cfg(not(windows))]
    let child = {
        Command::new("npm")
            .args(["run", "tauri", "dev", "--", "--no-watch"])
            .current_dir(&npm_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn dev mode: {}", e)))?
    };

    Ok(child)
}

async fn start_exe_mode(state: &SharedState) -> Result<tokio::process::Child, SupervisorError> {
    let exe_path = state.config.runner_exe_path();

    if !exe_path.exists() {
        return Err(SupervisorError::Process(format!(
            "Runner exe not found at {:?}. Run a build first.",
            exe_path
        )));
    }

    info!("Starting in exe mode from {:?}", exe_path);

    #[cfg(windows)]
    let child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        Command::new(&exe_path)
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?
    };

    #[cfg(not(windows))]
    let child = {
        Command::new(&exe_path)
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?
    };

    Ok(child)
}

/// Monitor the runner process for exit. Updates state when process terminates.
async fn monitor_process_exit(state: SharedState) {
    let exit_status = {
        let mut runner = state.runner.write().await;
        if let Some(ref mut child) = runner.process {
            match child.wait().await {
                Ok(status) => Some(status),
                Err(e) => {
                    error!("Error waiting for runner process: {}", e);
                    None
                }
            }
        } else {
            None
        }
    };

    // Update state
    {
        let mut runner = state.runner.write().await;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
    }

    if let Some(status) = exit_status {
        let msg = if status.success() {
            "Runner process exited normally".to_string()
        } else {
            format!("Runner process exited with status: {}", status)
        };

        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, &msg)
            .await;
        info!("{}", msg);
    } else {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Warn,
                "Runner process terminated unexpectedly",
            )
            .await;
        warn!("Runner process terminated unexpectedly");
    }
}

/// Stop the runner process. Attempts graceful shutdown, then force kill.
pub async fn stop_runner(state: &SharedState) -> Result<(), SupervisorError> {
    {
        let mut runner = state.runner.write().await;
        runner.stop_requested = true;
    }

    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Info, "Stopping runner...")
        .await;

    // 1. Try to kill the child process gracefully
    let _had_process = {
        let mut runner = state.runner.write().await;
        if let Some(ref mut child) = runner.process {
            info!("Killing runner child process");
            let _ = child.kill().await;

            // Wait briefly for process to exit
            let wait_result = tokio::time::timeout(
                Duration::from_secs(GRACEFUL_KILL_TIMEOUT_SECS),
                child.wait(),
            )
            .await;

            match wait_result {
                Ok(Ok(_)) => info!("Runner process exited gracefully"),
                Ok(Err(e)) => warn!("Error waiting for runner: {}", e),
                Err(_) => warn!("Runner did not exit within timeout"),
            }
            true
        } else {
            false
        }
    };

    // 2. Comprehensive kill (taskkill + port cleanup)
    kill_runner_comprehensive().await;

    // 3. Wait for ports to be free
    let api_free = wait_for_port_free(RUNNER_API_PORT, 5).await;
    if state.config.dev_mode {
        let vite_free = wait_for_port_free(RUNNER_VITE_PORT, 5).await;
        if !vite_free {
            warn!("Vite port {} still in use after stop", RUNNER_VITE_PORT);
            let _ = kill_by_port(RUNNER_VITE_PORT).await;
        }
    }
    if !api_free {
        warn!("API port {} still in use after stop", RUNNER_API_PORT);
        let _ = kill_by_port(RUNNER_API_PORT).await;
    }

    // 4. Update state
    {
        let mut runner = state.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
        runner.stop_requested = false;
    }

    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Info, "Runner stopped")
        .await;
    info!("Runner stopped");

    Ok(())
}

/// Stop runner, optionally rebuild, then start.
pub async fn restart_runner(state: &SharedState, rebuild: bool) -> Result<(), SupervisorError> {
    {
        let mut runner = state.runner.write().await;
        runner.restart_requested = true;
    }

    // Stop if running
    {
        let runner = state.runner.read().await;
        if runner.running {
            drop(runner);
            stop_runner(state).await?;
        }
    }

    // Rebuild if requested
    if rebuild {
        crate::build_monitor::run_cargo_build(state).await?;
    }

    // Start
    start_runner(state).await?;

    {
        let mut runner = state.runner.write().await;
        runner.restart_requested = false;
    }

    Ok(())
}

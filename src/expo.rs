use std::process::Stdio;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::error::SupervisorError;
use crate::log_capture::{self, LogLevel, LogSource};
use crate::process::windows::kill_by_port;
use crate::state::SharedState;

/// Start the Expo dev server. Returns error if not configured or already running.
pub async fn start_expo(state: &SharedState) -> Result<(), SupervisorError> {
    let expo_dir = match &state.config.expo_dir {
        Some(dir) => dir.clone(),
        None => {
            return Err(SupervisorError::Other(
                "Expo not configured (no --expo-dir provided)".to_string(),
            ))
        }
    };

    {
        let expo = state.expo.read().await;
        if expo.running {
            return Err(SupervisorError::Other(
                "Expo dev server is already running".to_string(),
            ));
        }
    }

    if !expo_dir.exists() {
        return Err(SupervisorError::Other(format!(
            "Expo directory does not exist: {:?}",
            expo_dir
        )));
    }

    state
        .logs
        .emit(
            LogSource::Expo,
            LogLevel::Info,
            format!("Starting Expo dev server from {:?}", expo_dir),
        )
        .await;

    #[cfg(windows)]
    let mut child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        Command::new("cmd")
            .args(["/C", "npx expo start"])
            .current_dir(&expo_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn Expo: {}", e)))?
    };

    #[cfg(not(windows))]
    let mut child = {
        Command::new("npx")
            .args(["expo", "start"])
            .current_dir(&expo_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn Expo: {}", e)))?
    };

    let pid = child.id();
    info!("Expo dev server started with PID {:?}", pid);

    // Capture stdout/stderr via parameterized readers
    if let Some(stdout) = child.stdout.take() {
        log_capture::spawn_reader_with_source(stdout, &state.logs, LogSource::Expo, true);
    }
    if let Some(stderr) = child.stderr.take() {
        log_capture::spawn_reader_with_source(stderr, &state.logs, LogSource::Expo, false);
    }

    // Update state
    {
        let mut expo = state.expo.write().await;
        expo.process = Some(child);
        expo.running = true;
        expo.pid = pid;
        expo.started_at = Some(chrono::Utc::now());
    }

    state
        .logs
        .emit(
            LogSource::Expo,
            LogLevel::Info,
            format!("Expo dev server started (PID: {:?})", pid),
        )
        .await;

    state.notify_health_change();

    // Monitor process exit in background
    let state_clone = state.clone();
    tokio::spawn(async move {
        monitor_expo_exit(state_clone).await;
    });

    Ok(())
}

/// Stop the Expo dev server.
pub async fn stop_expo(state: &SharedState) -> Result<(), SupervisorError> {
    state
        .logs
        .emit(
            LogSource::Expo,
            LogLevel::Info,
            "Stopping Expo dev server...",
        )
        .await;

    // Take the child process out of state to avoid holding lock across await
    let child = {
        let mut expo = state.expo.write().await;
        expo.process.take()
    };

    if let Some(mut child) = child {
        info!("Killing Expo child process");
        let _ = child.kill().await;

        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

        match wait_result {
            Ok(Ok(_)) => info!("Expo process exited gracefully"),
            Ok(Err(e)) => warn!("Error waiting for Expo: {}", e),
            Err(_) => warn!("Expo did not exit within timeout"),
        }
    }

    // Fallback: kill by port
    let expo_port = state.config.expo_port;
    let _ = kill_by_port(expo_port).await;

    // Update state
    {
        let mut expo = state.expo.write().await;
        expo.process = None;
        expo.running = false;
        expo.pid = None;
        expo.started_at = None;
    }

    state
        .logs
        .emit(LogSource::Expo, LogLevel::Info, "Expo dev server stopped")
        .await;
    info!("Expo dev server stopped");

    state.notify_health_change();

    Ok(())
}

/// Background task that waits for the Expo process to exit, then updates state.
async fn monitor_expo_exit(state: SharedState) {
    // Take the child out of state so we can await without holding the lock
    let child = {
        let mut expo = state.expo.write().await;
        expo.process.take()
    };

    let exit_status = if let Some(mut child) = child {
        match child.wait().await {
            Ok(status) => Some(status),
            Err(e) => {
                error!("Error waiting for Expo process: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Update state
    {
        let mut expo = state.expo.write().await;
        expo.running = false;
        expo.process = None;
        expo.pid = None;
    }

    state.notify_health_change();

    if let Some(status) = exit_status {
        let msg = if status.success() {
            "Expo process exited normally".to_string()
        } else {
            format!("Expo process exited with status: {}", status)
        };
        state.logs.emit(LogSource::Expo, LogLevel::Info, &msg).await;
        info!("{}", msg);
    } else {
        state
            .logs
            .emit(
                LogSource::Expo,
                LogLevel::Warn,
                "Expo process terminated unexpectedly",
            )
            .await;
        warn!("Expo process terminated unexpectedly");
    }
}

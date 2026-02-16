use regex::Regex;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::{BUILD_MONITOR_WINDOW_SECS, BUILD_TIMEOUT_SECS};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::windows::cleanup_orphaned_build_processes;
use crate::state::SharedState;

static BUILD_ERROR_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"error\[E\d+\]").unwrap(),
        Regex::new(r"error: could not compile").unwrap(),
        Regex::new(r"error: aborting due to").unwrap(),
        Regex::new(r"error: linking with .* failed").unwrap(),
        Regex::new(r"error: cannot find").unwrap(),
        Regex::new(r"error: no matching package").unwrap(),
    ]
});

/// Run `cargo build` for the runner project.
pub async fn run_cargo_build(state: &SharedState) -> Result<(), SupervisorError> {
    // Check if build already in progress
    {
        let build = state.build.read().await;
        if build.build_in_progress {
            return Err(SupervisorError::BuildInProgress);
        }
    }

    // Mark build started
    {
        let mut build = state.build.write().await;
        build.build_in_progress = true;
        build.build_error_detected = false;
        build.last_build_error = None;
        build.last_build_at = Some(chrono::Utc::now());
    }

    state
        .logs
        .emit(LogSource::Build, LogLevel::Info, "Starting cargo build...")
        .await;
    info!("Starting cargo build in {:?}", state.config.project_dir);

    // Cleanup orphaned build processes first
    cleanup_orphaned_build_processes().await;

    let result = run_build_inner(state).await;

    // Mark build complete
    {
        let mut build = state.build.write().await;
        build.build_in_progress = false;
        if let Err(ref e) = result {
            build.build_error_detected = true;
            build.last_build_error = Some(e.to_string());
        }
    }

    result
}

async fn run_build_inner(state: &SharedState) -> Result<(), SupervisorError> {
    #[cfg(windows)]
    let mut child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        Command::new("cargo")
            .args(["build", "--bin", "qontinui-runner"])
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn cargo build: {}", e)))?
    };

    #[cfg(not(windows))]
    let mut child = {
        Command::new("cargo")
            .args(["build", "--bin", "qontinui-runner"])
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn cargo build: {}", e)))?
    };

    // Stream stderr (cargo outputs to stderr)
    let stderr = child.stderr.take();

    let stderr_handle = if let Some(stderr) = stderr {
        let state_clone = state.clone();
        Some(tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let mut error_lines = Vec::new();

            while let Ok(Some(line)) = lines.next_line().await {
                let is_error = BUILD_ERROR_PATTERNS.iter().any(|p| p.is_match(&line));
                let level = if is_error {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                };

                state_clone.logs.emit(LogSource::Build, level, &line).await;

                if is_error {
                    error_lines.push(line);
                }
            }

            error_lines
        }))
    } else {
        None
    };

    // Wait with timeout
    let wait_result =
        tokio::time::timeout(Duration::from_secs(BUILD_TIMEOUT_SECS), child.wait()).await;

    let status = match wait_result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Err(SupervisorError::Process(format!(
                "Build process error: {}",
                e
            )));
        }
        Err(_) => {
            warn!("Build timed out after {}s, killing", BUILD_TIMEOUT_SECS);
            let _ = child.kill().await;
            return Err(SupervisorError::Timeout(format!(
                "Build timed out after {}s",
                BUILD_TIMEOUT_SECS
            )));
        }
    };

    // Collect any error output
    let error_lines = if let Some(handle) = stderr_handle {
        handle.await.unwrap_or_default()
    } else {
        Vec::new()
    };

    if status.success() {
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                "Build completed successfully",
            )
            .await;
        info!("Build completed successfully");
        Ok(())
    } else {
        let error_summary = if error_lines.is_empty() {
            format!("Build failed with exit code: {}", status)
        } else {
            format!("Build failed:\n{}", error_lines.join("\n"))
        };
        error!("{}", error_summary);
        state
            .logs
            .emit(LogSource::Build, LogLevel::Error, &error_summary)
            .await;
        Err(SupervisorError::BuildFailed(error_summary))
    }
}

/// Monitor runner output for build errors during the first N seconds after startup.
/// Called as a background task when the runner starts.
pub fn spawn_build_error_monitor(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = state.logs.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(BUILD_MONITOR_WINDOW_SECS);

        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }

            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(entry)) => {
                    if entry.source == crate::log_capture::LogSource::Runner {
                        let is_error = BUILD_ERROR_PATTERNS
                            .iter()
                            .any(|p| p.is_match(&entry.message));
                        if is_error {
                            warn!("Build error detected in runner output: {}", entry.message);
                            let mut build = state.build.write().await;
                            build.build_error_detected = true;
                            build.last_build_error = Some(entry.message.clone());
                            break;
                        }
                    }
                }
                Ok(Err(_)) => {
                    // Channel lagged, continue
                }
                Err(_) => {
                    // Timeout reached
                    break;
                }
            }
        }
    })
}

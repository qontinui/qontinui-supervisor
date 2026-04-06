use regex::Regex;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::{BUILD_MONITOR_WINDOW_SECS, BUILD_TIMEOUT_SECS};
use crate::diagnostics::DiagnosticEventKind;
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
        Regex::new(r"error: failed to remove file").unwrap(),
    ]
});

/// Run `cargo build` for the runner project.
pub async fn run_cargo_build(state: &SharedState) -> Result<(), SupervisorError> {
    // Atomically check and mark build started (single write lock avoids TOCTOU race)
    {
        let mut build = state.build.write().await;
        if build.build_in_progress {
            return Err(SupervisorError::BuildInProgress);
        }
        build.build_in_progress = true;
        build.build_error_detected = false;
        build.last_build_error = None;
        build.last_build_at = Some(chrono::Utc::now());
    }

    state.notify_health_change();

    state
        .logs
        .emit(LogSource::Build, LogLevel::Info, "Starting cargo build...")
        .await;
    info!("Starting cargo build in {:?}", state.config.project_dir);

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::BuildStarted);

    // Stop non-primary exe-mode runners that lock the build artifact.
    // (Non-primary runners now use copied exes, but stop any still using the original.)
    stop_exe_runners_for_build(state).await;

    // Cleanup orphaned build processes first
    cleanup_orphaned_build_processes().await;

    // Wait for the runner exe to be unlocked (Windows holds file locks briefly after process exit)
    wait_for_exe_unlocked(state).await;

    let build_start = std::time::Instant::now();
    let result = run_build_inner(state).await;
    let duration_secs = build_start.elapsed().as_secs_f64();

    // Mark build complete
    {
        let mut build = state.build.write().await;
        build.build_in_progress = false;
        if let Err(ref e) = result {
            build.build_error_detected = true;
            build.last_build_error = Some(e.to_string());
        }
    }

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::BuildCompleted {
            duration_secs,
            success: result.is_ok(),
            error: result.as_ref().err().map(|e| e.to_string()),
        });

    state.notify_health_change();

    result
}

async fn run_build_inner(state: &SharedState) -> Result<(), SupervisorError> {
    // In exe mode the frontend is embedded in the binary via tauri_build, so we
    // must run `npm run build` first to produce a fresh dist/ before cargo build.
    // In dev mode Vite serves the frontend live, so this step is skipped.
    if !state.config.dev_mode {
        let npm_dir = state.config.runner_npm_dir();
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                "Building frontend (npm run build)...",
            )
            .await;
        info!("Building frontend in {:?}", npm_dir);

        #[cfg(windows)]
        let npm_result = {
            const CREATE_NO_WINDOW_: u32 = 0x0800_0000;
            Command::new("cmd")
                .args(["/C", "npm.cmd run build"])
                .current_dir(&npm_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW_)
                .output()
                .await
        };

        #[cfg(not(windows))]
        let npm_result = {
            Command::new("npm")
                .args(["run", "build"])
                .current_dir(&npm_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        };

        match npm_result {
            Ok(output) if output.status.success() => {
                info!("Frontend build succeeded");
                state
                    .logs
                    .emit(LogSource::Build, LogLevel::Info, "Frontend build succeeded")
                    .await;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("Frontend build failed: {}", stderr);
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Warn,
                        format!(
                            "Frontend build failed: {}",
                            stderr.chars().take(500).collect::<String>()
                        ),
                    )
                    .await;
                // Continue with cargo build — the old dist/ may still be usable
            }
            Err(e) => {
                warn!("Failed to spawn frontend build: {}", e);
            }
        }
    }

    #[cfg(windows)]
    let mut child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut cmd = Command::new("cargo");
        // In exe mode, pass --features custom-protocol so Tauri embeds the frontend
        // from dist/ instead of trying to connect to the Vite dev server (devUrl).
        // The tauri crate sets cfg(dev) = !custom_protocol, so without this feature
        // the binary loads from localhost:1420 which doesn't exist in exe mode.
        let args: Vec<&str> = if state.config.dev_mode {
            vec!["build", "--bin", "qontinui-runner"]
        } else {
            vec![
                "build",
                "--bin",
                "qontinui-runner",
                "--features",
                "custom-protocol",
            ]
        };
        cmd.args(&args)
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        cmd.spawn()
            .map_err(|e| SupervisorError::Process(format!("Failed to spawn cargo build: {}", e)))?
    };

    #[cfg(not(windows))]
    let mut child = {
        let mut cmd = Command::new("cargo");
        let args: Vec<&str> = if state.config.dev_mode {
            vec!["build", "--bin", "qontinui-runner"]
        } else {
            vec![
                "build",
                "--bin",
                "qontinui-runner",
                "--features",
                "custom-protocol",
            ]
        };
        cmd.args(&args)
            .current_dir(&state.config.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn()
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
            let mut all_lines = Vec::new();

            while let Ok(Some(line)) = lines.next_line().await {
                let is_error = BUILD_ERROR_PATTERNS.iter().any(|p| p.is_match(&line));
                let level = if is_error {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                };

                state_clone.logs.emit(LogSource::Build, level, &line).await;
                all_lines.push(line.clone());

                if is_error {
                    error_lines.push(line);
                }
            }

            (error_lines, all_lines)
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

    // Collect any remaining error output.  Give the stderr reader a few seconds
    // to finish — on Windows, orphaned grandchild processes (rustc, linker) can
    // keep the pipe open long after cargo itself exits, causing an indefinite hang.
    let (error_lines, all_stderr_lines) = if let Some(handle) = stderr_handle {
        match tokio::time::timeout(Duration::from_secs(5), handle).await {
            Ok(Ok(result)) => result,
            _ => {
                warn!("Timed out waiting for build stderr reader, proceeding without full output");
                (Vec::new(), Vec::new())
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };

    // Store full stderr for smart rebuild AI fix prompt
    if !all_stderr_lines.is_empty() {
        let mut build = state.build.write().await;
        build.last_build_stderr = Some(all_stderr_lines.join("\n"));
    }

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

/// Wait for the runner exe to be writable (unlocked) before building.
/// On Windows, the OS can hold file locks briefly after a process is killed.
async fn wait_for_exe_unlocked(state: &SharedState) {
    let exe_path = state.config.runner_exe_path();
    if !exe_path.exists() {
        return;
    }

    let max_attempts = 20; // 20 × 500ms = 10s max wait
    for attempt in 1..=max_attempts {
        match std::fs::OpenOptions::new().write(true).open(&exe_path) {
            Ok(_) => {
                if attempt > 1 {
                    info!("Runner exe unlocked after {}ms", attempt * 500);
                }
                return;
            }
            Err(e) if attempt < max_attempts => {
                warn!(
                    "Runner exe still locked (attempt {}/{}): {}",
                    attempt, max_attempts, e
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                warn!(
                    "Runner exe still locked after {}s, proceeding anyway: {}",
                    max_attempts / 2,
                    e
                );
            }
        }
    }
}

/// Stop non-primary exe-mode runners before a cargo build.
/// Stop only temp runners that may hold a file lock on the build output binary.
/// User runners are never stopped — they use copied exes and don't lock the build artifact.
async fn stop_exe_runners_for_build(state: &SharedState) {
    let runners = state.get_all_runners().await;
    for managed in &runners {
        if !crate::process::manager::is_temp_runner(&managed.config.id) {
            continue;
        }
        let running = managed.runner.read().await.running;
        if running {
            info!(
                "Stopping temp runner '{}' before build to release exe lock",
                managed.config.name
            );
            if let Err(e) =
                crate::process::manager::stop_runner_by_id(state, &managed.config.id).await
            {
                warn!(
                    "Failed to stop temp runner '{}' before build: {}",
                    managed.config.name, e
                );
            }
        }
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
                            {
                                let mut build = state.build.write().await;
                                build.build_error_detected = true;
                                build.last_build_error = Some(entry.message.clone());
                            }
                            state.notify_health_change();
                            crate::ai_debug::schedule_debug(
                                &state,
                                "Build error detected in runner output",
                            )
                            .await;
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

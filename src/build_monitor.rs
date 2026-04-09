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
use crate::state::{BuildInfo, BuildSlot, SharedState};
use std::sync::Arc;

/// RAII guard that clears a `BuildSlot::busy` field on drop.
///
/// Ensures the slot is released on every exit path — happy path, `?`
/// early-return, panic, and task cancellation. Without this, an aborted
/// build task would leave `slot.busy = Some(..)` forever even though its
/// permit gets dropped correctly, preventing `claim_idle_slot` from ever
/// picking that slot again.
struct SlotGuard {
    slot: Arc<BuildSlot>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.slot.busy.try_write() {
            *busy = None;
        } else {
            let slot = self.slot.clone();
            tokio::spawn(async move {
                let mut busy = slot.busy.write().await;
                *busy = None;
            });
        }
    }
}

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
///
/// Claims a slot from the build pool (blocking on the semaphore if all slots
/// are busy), sets `CARGO_TARGET_DIR` to the slot's isolated target dir, and
/// runs cargo. Concurrent calls execute in parallel up to `pool_size`.
///
/// `requester_id` is an optional hint (e.g. an agent name) stored with the
/// active build for visibility via `GET /builds`.
pub async fn run_cargo_build(state: &SharedState) -> Result<(), SupervisorError> {
    run_cargo_build_with_requester(state, None).await
}

/// Same as `run_cargo_build` but records a requester_id for queue visibility.
pub async fn run_cargo_build_with_requester(
    state: &SharedState,
    requester_id: Option<String>,
) -> Result<(), SupervisorError> {
    // Acquire a permit from the build pool. Blocks until a slot is free.
    // Queue depth counter lets `GET /builds` report how many callers are waiting.
    state
        .build_pool
        .queue_depth
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let permit_result = state.build_pool.permits.clone().acquire_owned().await;
    state
        .build_pool
        .queue_depth
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let _permit = permit_result.map_err(|_| {
        SupervisorError::Other("Build pool semaphore closed".to_string())
    })?;

    // Claim a slot and mark it busy with our BuildInfo.
    let info = BuildInfo {
        started_at: chrono::Utc::now(),
        requester_id,
        rebuild_kind: if state.config.dev_mode { "dev".to_string() } else { "exe".to_string() },
    };
    let slot = state.build_pool.claim_idle_slot(info).await;
    // RAII guard: clears `slot.busy = None` on every exit path (happy path,
    // `?`, panic, task cancellation). Prevents permanently-stuck slots.
    let _slot_guard = SlotGuard { slot: slot.clone() };

    // Update legacy build flag for external consumers (health API, smart rebuild,
    // overnight watchdog). Flag is true whenever any slot is busy.
    {
        let mut build = state.build.write().await;
        build.build_in_progress = true;
        build.build_error_detected = false;
        build.last_build_error = None;
        build.last_build_at = Some(chrono::Utc::now());
    }

    state.notify_health_change();

    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!(
                "Starting cargo build on slot {} (target: {:?})",
                slot.id, slot.target_dir
            ),
        )
        .await;
    info!(
        "Starting cargo build on slot {} in {:?} (CARGO_TARGET_DIR={:?})",
        slot.id, state.config.project_dir, slot.target_dir
    );

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
    wait_for_exe_unlocked_for_slot(state, &slot).await;

    let build_start = std::time::Instant::now();
    let result = run_build_inner(state, &slot).await;
    let duration_secs = build_start.elapsed().as_secs_f64();

    // Record build duration into this slot's rolling history BEFORE
    // releasing the slot, so the history write doesn't race with the next
    // build on this slot.
    {
        let mut history = slot.history.write().await;
        history.record(
            duration_secs,
            result.is_ok(),
            result.as_ref().err().map(|e| e.to_string()),
        );
    }

    // Release the slot via the RAII guard. Explicit drop so the slot is
    // cleared before we recompute `any_slot_busy` below.
    drop(_slot_guard);

    // If this build succeeded, record the slot as the most recent successful one.
    // Readers of `rebuild: false` use this to locate the exe to copy.
    if result.is_ok() {
        let mut last = state.build_pool.last_successful_slot.write().await;
        *last = Some(slot.id);
    }

    // Recompute legacy build_in_progress flag: true iff any slot is still busy.
    let any_busy = any_slot_busy(state).await;
    {
        let mut build = state.build.write().await;
        build.build_in_progress = any_busy;
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

    // Permit drops here, releasing the slot for the next waiter.
    drop(_permit);

    result
}

/// Scan slots and return true if any has `busy.is_some()`.
async fn any_slot_busy(state: &SharedState) -> bool {
    for slot in &state.build_pool.slots {
        if slot.busy.read().await.is_some() {
            return true;
        }
    }
    false
}

async fn run_build_inner(state: &SharedState, slot: &Arc<BuildSlot>) -> Result<(), SupervisorError> {
    // In exe mode the frontend is embedded in the binary via tauri_build, so we
    // must run `npm run build` first to produce a fresh dist/ before cargo build.
    // In dev mode Vite serves the frontend live, so this step is skipped.
    //
    // Frontend builds are serialized across slots via `build_pool.npm_lock`:
    // Tauri's `rust-embed` pulls from a single `dist/` directory, so two
    // concurrent npm builds would corrupt the output. The lock is held ONLY
    // for the npm invocation (~12s), not the whole cargo build (~180s), so
    // this is a much smaller serialization point than the legacy global flag.
    if !state.config.dev_mode {
        let npm_dir = state.config.runner_npm_dir();
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!("Slot {}: waiting for frontend build lock...", slot.id),
            )
            .await;

        let _npm_guard = state.build_pool.npm_lock.clone().lock_owned().await;

        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!("Slot {}: building frontend (npm run build)...", slot.id),
            )
            .await;
        info!("Slot {}: building frontend in {:?}", slot.id, npm_dir);

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
                info!("Slot {}: frontend build succeeded", slot.id);
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Info,
                        format!("Slot {}: frontend build succeeded", slot.id),
                    )
                    .await;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("Slot {}: frontend build failed: {}", slot.id, stderr);
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Warn,
                        format!(
                            "Slot {}: frontend build failed: {}",
                            slot.id,
                            stderr.chars().take(500).collect::<String>()
                        ),
                    )
                    .await;
                // Continue with cargo build — the old dist/ may still be usable
            }
            Err(e) => {
                warn!("Slot {}: failed to spawn frontend build: {}", slot.id, e);
            }
        }
        // npm_guard drops here, releasing the frontend build lock before cargo starts.
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
            // Redirect cargo output to this slot's isolated target dir so
            // concurrent builds on other slots don't contend on the same target/.
            .env("CARGO_TARGET_DIR", &slot.target_dir)
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
            .env("CARGO_TARGET_DIR", &slot.target_dir)
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

/// Wait for the runner exe in a specific slot's target dir to be writable
/// (unlocked) before building. On Windows, the OS can hold file locks briefly
/// after a process is killed.
async fn wait_for_exe_unlocked_for_slot(state: &SharedState, slot: &Arc<BuildSlot>) {
    let _ = state; // kept in signature for logging consistency
    let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
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

// =============================================================================
// Pre-warm
// =============================================================================

/// Timeout per slot's pre-warm `cargo check`.
const PREWARM_TIMEOUT_SECS: u64 = 60;

/// Pre-warm each build slot's incremental cache by running `cargo check`.
///
/// Spawned as `tokio::spawn` after the HTTP server binds so it doesn't delay
/// startup. Skipped in dev mode and when `--no-prewarm` is set.
pub async fn prewarm_build_slots(state: crate::state::SharedState) {
    if state.config.dev_mode {
        info!("Dev mode: skipping build slot pre-warm");
        return;
    }
    if state.config.no_prewarm {
        info!("Build slot pre-warm disabled via --no-prewarm / QONTINUI_SUPERVISOR_NO_PREWARM");
        return;
    }

    let slots: Vec<Arc<BuildSlot>> = state.build_pool.slots.clone();
    info!("Pre-warming {} build slot(s)...", slots.len());

    for slot in slots {
        let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
        if exe_path.exists() {
            info!("Slot {} already has a binary, skipping prewarm", slot.id);
            continue;
        }
        if let Err(e) = prewarm_single_slot(&state, &slot).await {
            warn!("Prewarm of slot {} failed: {}", slot.id, e);
            state
                .logs
                .emit(LogSource::Build, LogLevel::Warn, format!("Prewarm of slot {} failed: {}", slot.id, e))
                .await;
        }
    }
    info!("Build slot pre-warm complete");
}

async fn prewarm_single_slot(
    state: &crate::state::SharedState,
    slot: &Arc<BuildSlot>,
) -> Result<(), SupervisorError> {
    // Acquire a permit so concurrent spawn-test calls see this slot as busy.
    state.build_pool.queue_depth.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let permit_result = state.build_pool.permits.clone().acquire_owned().await;
    state.build_pool.queue_depth.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let _permit = permit_result
        .map_err(|_| SupervisorError::Other("Build pool semaphore closed".to_string()))?;

    // Re-check after acquiring: another caller may have populated this slot.
    let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
    if exe_path.exists() {
        info!("Slot {} populated while waiting for permit, skipping prewarm", slot.id);
        return Ok(());
    }

    // Claim this specific slot.
    {
        let mut busy = slot.busy.write().await;
        if busy.is_some() {
            return Ok(());
        }
        *busy = Some(BuildInfo {
            started_at: chrono::Utc::now(),
            requester_id: Some("supervisor-prewarm".to_string()),
            rebuild_kind: "prewarm".to_string(),
        });
    }
    let _slot_guard = SlotGuard { slot: slot.clone() };

    info!("Prewarming build slot {} (target: {:?})...", slot.id, slot.target_dir);
    state.logs.emit(LogSource::Build, LogLevel::Info, format!("Prewarming slot {}...", slot.id)).await;

    let start = std::time::Instant::now();

    let args: Vec<&str> = if state.config.dev_mode {
        vec!["check", "--bin", "qontinui-runner"]
    } else {
        vec!["check", "--bin", "qontinui-runner", "--features", "custom-protocol"]
    };

    #[cfg(windows)]
    let child_result = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("cargo")
            .args(&args)
            .current_dir(&state.config.project_dir)
            .env("CARGO_TARGET_DIR", &slot.target_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .spawn()
    };

    #[cfg(not(windows))]
    let child_result = {
        Command::new("cargo")
            .args(&args)
            .current_dir(&state.config.project_dir)
            .env("CARGO_TARGET_DIR", &slot.target_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    };

    let mut child = child_result
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn prewarm cargo check: {}", e)))?;

    // Stream stderr to logs
    if let Some(stderr) = child.stderr.take() {
        let state_clone = state.clone();
        let slot_id = slot.id;
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                state_clone
                    .logs
                    .emit(LogSource::Build, LogLevel::Info, format!("[prewarm slot {}] {}", slot_id, line))
                    .await;
            }
        });
    }

    match tokio::time::timeout(Duration::from_secs(PREWARM_TIMEOUT_SECS), child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            let ms = start.elapsed().as_millis();
            info!("Prewarmed slot {} in {}ms", slot.id, ms);
            state.logs.emit(LogSource::Build, LogLevel::Info, format!("Prewarmed slot {} in {}ms", slot.id, ms)).await;
            // Set last_successful_slot only if no real build has run yet.
            let mut last = state.build_pool.last_successful_slot.write().await;
            if last.is_none() {
                *last = Some(slot.id);
            }
            Ok(())
        }
        Ok(Ok(status)) => {
            warn!("Prewarm cargo check for slot {} exited with {}", slot.id, status);
            Err(SupervisorError::BuildFailed(format!("Prewarm exited with {}", status)))
        }
        Ok(Err(e)) => Err(SupervisorError::Process(format!("Prewarm process error: {}", e))),
        Err(_) => {
            warn!("Prewarm of slot {} timed out after {}s, killing", slot.id, PREWARM_TIMEOUT_SECS);
            let _ = child.kill().await;
            Err(SupervisorError::Timeout(format!("Prewarm timed out after {}s", PREWARM_TIMEOUT_SECS)))
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

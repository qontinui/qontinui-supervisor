use regex::Regex;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::build_timeout_secs;
use crate::diagnostics::DiagnosticEventKind;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::windows::{
    cleanup_orphaned_build_processes, find_pids_holding_exe, kill_by_pid, pid_exe_path,
};
use crate::state::{BuildInfo, BuildSlot, LkgInfo, SharedState};
use std::sync::Arc;

/// RAII guard that clears a `BuildSlot::busy` field on drop AND reconciles
/// the global `state.build.build_in_progress` legacy flag.
///
/// Ensures both pieces of state are released on every exit path — happy
/// path, `?` early-return, panic, and task cancellation. Without this, an
/// aborted build task would leave `slot.busy = Some(..)` forever and/or
/// the legacy `build_in_progress` flag stuck at `true`. The pre-2026-05-01
/// version only handled `slot.busy`; the global flag was reconciled by an
/// explicit recompute after the build finished, which was skipped on
/// cancellation, leaving `health.build.in_progress: true` while every slot
/// reported `idle`.
struct SlotGuard {
    slot: Arc<BuildSlot>,
    state: SharedState,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // Path 1 (sync, fast): try to clear the slot in-place.
        let cleared_inline = if let Ok(mut busy) = self.slot.busy.try_write() {
            *busy = None;
            true
        } else {
            false
        };

        // Path 2 (async fallback): if we couldn't take the slot lock here,
        // OR after we've cleared it, schedule a task that recomputes the
        // global flag from authoritative slot state. Spawn unconditionally
        // so the recompute always runs — `any_slot_busy(state)` requires
        // async access to every slot's RwLock, which we can't do from Drop.
        let slot = self.slot.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            if !cleared_inline {
                let mut busy = slot.busy.write().await;
                *busy = None;
            }
            // Reconcile the global legacy flag. Authoritative source is
            // `any_slot_busy` — never trust the cached flag during recovery.
            let any_busy = any_slot_busy(&state).await;
            let mut build = state.build.write().await;
            build.build_in_progress = any_busy;
        });
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
    let _permit = permit_result
        .map_err(|_| SupervisorError::Other("Build pool semaphore closed".to_string()))?;

    // Claim a slot and mark it busy with our BuildInfo.
    let info = BuildInfo {
        started_at: chrono::Utc::now(),
        requester_id,
        rebuild_kind: "exe".to_string(),
    };
    let slot = state.build_pool.claim_idle_slot(info).await;
    // RAII guard: clears `slot.busy = None` AND reconciles the global
    // `build_in_progress` flag on every exit path (happy path, `?`, panic,
    // task cancellation). Prevents permanently-stuck slots and stale flags.
    let _slot_guard = SlotGuard {
        slot: slot.clone(),
        state: state.clone(),
    };

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

    // Wait for the runner exe to be unlocked (Windows holds file locks briefly after process exit).
    // If the lock persists, identify the holder and kill orphans / stop registered temp runners.
    // Returns Err only if the holder is a user-managed primary/named runner; in that case we
    // skip cargo entirely so we don't masquerade a pre-build conflict as a build failure.
    let build_start = std::time::Instant::now();
    let result = match free_slot_exe(state, &slot).await {
        Ok(()) => run_build_inner(state, &slot).await,
        Err(e) => Err(e),
    };
    let duration_secs = build_start.elapsed().as_secs_f64();

    // Pull any captured cargo stderr the inner build deposited so it can be
    // recorded alongside the rolling history entry.
    let captured_stderr = slot.last_build_stderr_capture.write().await.take();

    // Record build duration into this slot's rolling history BEFORE
    // releasing the slot, so the history write doesn't race with the next
    // build on this slot.
    {
        let mut history = slot.history.write().await;
        history.record(
            duration_secs,
            result.is_ok(),
            result.as_ref().err().map(|e| e.to_string()),
            if result.is_err() {
                captured_stderr
            } else {
                None
            },
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
        drop(last);

        // Capture this exe as the new last-known-good. Survives subsequent
        // failed builds that overwrite or delete the slot's exe; agents
        // testing changes can fall back to it via spawn-test {use_lkg: true}
        // when their own build fails. Failures here are logged but do not
        // fail the build — LKG is a safety net, not a correctness gate.
        if let Err(e) = update_lkg_after_success(state, &slot).await {
            warn!(
                "Failed to update LKG copy after slot {} build success: {}",
                slot.id, e
            );
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Warn,
                    format!("LKG capture failed after slot {} build: {}", slot.id, e),
                )
                .await;
        }
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

async fn run_build_inner(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
) -> Result<(), SupervisorError> {
    // The frontend is embedded in the binary via tauri_build, so we must run
    // `npm run build` first to produce a fresh dist/ before cargo build.
    //
    // Frontend builds are serialized across slots via `build_pool.npm_lock`:
    // Tauri's `rust-embed` pulls from a single `dist/` directory, so two
    // concurrent npm builds would corrupt the output. The lock is held ONLY
    // for the npm invocation (~12s), not the whole cargo build (~180s), so
    // this is a much smaller serialization point than the legacy global flag.
    {
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
                // Tauri's CLI sets this when it drives the frontend build itself,
                // but we invoke `npm run build` directly — so vite.config.ts's
                // `target: process.env.TAURI_PLATFORM == "windows" ? "chrome105" : "safari13"`
                // would otherwise fall back to safari13 and esbuild fails on
                // destructuring transpilation, leaving dist/ stale.
                .env("TAURI_PLATFORM", "windows")
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
                // Clear any prior "stale frontend" marker — the dist/ snapshot
                // cargo is about to consume is known-fresh.
                *slot.frontend_stale.write().await = false;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let truncated: String = stderr.chars().take(500).collect();
                error!(
                    "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. stderr: {}",
                    slot.id, truncated
                );
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Error,
                        format!(
                            "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. stderr: {}",
                            slot.id, truncated
                        ),
                    )
                    .await;
                // Mark the slot as embedding a stale frontend until the next
                // successful npm build clears it.
                *slot.frontend_stale.write().await = true;
                // Record the npm failure reason in the slot's rolling history
                // so `GET /builds` can show it even though the cargo build may
                // ultimately succeed.
                {
                    let mut history = slot.history.write().await;
                    history.last_error = Some(format!(
                        "frontend_stale: npm run build failed: {}",
                        truncated
                    ));
                }
                // Continue with cargo build — the old dist/ may still be usable
            }
            Err(e) => {
                error!(
                    "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. spawn error: {}",
                    slot.id, e
                );
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Error,
                        format!(
                            "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. spawn error: {}",
                            slot.id, e
                        ),
                    )
                    .await;
                *slot.frontend_stale.write().await = true;
                {
                    let mut history = slot.history.write().await;
                    history.last_error = Some(format!(
                        "frontend_stale: npm run build failed to spawn: {}",
                        e
                    ));
                }
            }
        }
        // npm_guard drops here, releasing the frontend build lock before cargo starts.
    }

    // Always pass --features custom-protocol so Tauri embeds the frontend from
    // dist/. Without it, `cfg(dev) = !custom_protocol` makes the binary load
    // from devUrl (localhost:1420), which isn't running.
    const CARGO_BUILD_ARGS: &[&str] = &[
        "build",
        "--bin",
        "qontinui-runner",
        "--features",
        "custom-protocol",
    ];

    #[cfg(windows)]
    let mut child = {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut cmd = Command::new("cargo");
        cmd.args(CARGO_BUILD_ARGS)
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
        cmd.args(CARGO_BUILD_ARGS)
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
    let timeout_secs = build_timeout_secs();
    let wait_result = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;

    let status = match wait_result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Err(SupervisorError::Process(format!(
                "Build process error: {}",
                e
            )));
        }
        Err(_) => {
            warn!("Build timed out after {}s, killing", timeout_secs);
            let _ = child.kill().await;
            return Err(SupervisorError::Timeout(format!(
                "Build timed out after {}s",
                timeout_secs
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
        let full_stderr = all_stderr_lines.join("\n");

        // Persist the full captured stderr next to the slot so it survives
        // a supervisor restart for postmortem inspection. Best-effort: a
        // failed write is logged but does not change the build outcome.
        let stderr_path = slot.target_dir.join("last-build.stderr");
        if let Err(e) = tokio::fs::write(&stderr_path, full_stderr.as_bytes()).await {
            warn!(
                "Failed to persist last-build.stderr for slot {} at {:?}: {}",
                slot.id, stderr_path, e
            );
        }

        // Stash the tail (capped) on the slot so the outer caller can fold
        // it into SlotHistory::last_error_detail.
        let detail_tail = tail_bytes_keep_utf8(&full_stderr, LAST_BUILD_STDERR_DETAIL_BYTES);
        *slot.last_build_stderr_capture.write().await = Some(detail_tail.clone());

        // Append a short tail to the user-visible error so even the legacy
        // `last_error` string carries actionable info (the SlotHistory
        // detail field has the longer cap).
        let short_tail = tail_bytes_keep_utf8(&full_stderr, LAST_BUILD_STDERR_SHORT_TAIL_BYTES);
        let base = if error_lines.is_empty() {
            format!("Build failed with exit code: {}", status)
        } else {
            format!("Build failed:\n{}", error_lines.join("\n"))
        };
        let error_summary = if short_tail.is_empty() {
            base
        } else {
            format!("{}\n\n--- cargo stderr (last 2KB) ---\n{}", base, short_tail)
        };
        error!("{}", error_summary);
        state
            .logs
            .emit(LogSource::Build, LogLevel::Error, &error_summary)
            .await;
        Err(SupervisorError::BuildFailed(error_summary))
    }
}

/// Cap on the per-slot `last_build_stderr_capture` blob. Matches
/// `state::LAST_ERROR_DETAIL_MAX_BYTES`; lifted into a const so the constant
/// expression is local to the build_monitor and the source of truth for
/// `SlotHistory::last_error_detail` is `state.rs`.
const LAST_BUILD_STDERR_DETAIL_BYTES: usize = 4 * 1024;

/// Cap on the inline tail appended to the user-visible build error string.
const LAST_BUILD_STDERR_SHORT_TAIL_BYTES: usize = 2 * 1024;

/// Return the last `max_bytes` bytes of `s`, snapped forward to a UTF-8
/// character boundary so the result is always valid UTF-8. Returns `s`
/// unchanged when it's already shorter than `max_bytes`.
fn tail_bytes_keep_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = s.len() - max_bytes;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[cut..].to_string()
}

/// Wait for the runner exe in a specific slot's target dir to be writable
/// (unlocked) before building. On Windows, the OS can hold file locks briefly
/// after a process is killed.
///
/// If the lock persists past the brief grace period, identify the holder(s)
/// and resolve the conflict:
///
/// - **Orphan PID** (process exists but no registered runner claims it, or the
///   matching registry entry has `pid: None`/`running: false`): kill the PID
///   directly. By construction it's a zombie the supervisor lost track of —
///   typically a child the supervisor itself spawned that drifted out of the
///   registry. There is no scenario where leaving a slot binary running
///   detached from the registry is intentional.
/// - **Registered temp runner** holding the slot exe: stop it via the
///   supervisor's normal stop path. Temp runners *should* be running from a
///   copy in `target/debug/`; finding one running directly from the slot
///   means `start_managed_runner`'s copy step fell back to `source_exe`,
///   which is a bug we want to surface.
/// - **Registered primary or named runner** holding the slot exe: do *not*
///   auto-kill — that's the user's runner. Log loudly, surface a build
///   error, and let the operator decide. (This shouldn't happen because
///   non-temp runners also use copied exes; if it does, Fix B should
///   prevent it from recurring.)
async fn free_slot_exe(state: &SharedState, slot: &Arc<BuildSlot>) -> Result<(), SupervisorError> {
    let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
    if !exe_path.exists() {
        return Ok(());
    }

    // Short grace window — Windows often releases handles within ~1-2s after
    // a process exits. Don't escalate to PID enumeration unless we've waited
    // long enough that the lock is clearly persistent.
    let grace_attempts = 4; // 4 × 500ms = 2s
    for attempt in 1..=grace_attempts {
        match std::fs::OpenOptions::new().write(true).open(&exe_path) {
            Ok(_) => {
                if attempt > 1 {
                    let msg = format!("Slot {} exe unlocked after {}ms", slot.id, attempt * 500);
                    info!("{}", msg);
                    state.logs.emit(LogSource::Build, LogLevel::Info, msg).await;
                }
                return Ok(());
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    // Still locked. Enumerate holders and resolve.
    let holders = find_pids_holding_exe(&exe_path).await;
    if holders.is_empty() {
        let msg = format!(
            "Slot {} exe still locked but no holder PID found via sysinfo — proceeding anyway",
            slot.id
        );
        warn!("{}", msg);
        state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
        return Ok(());
    }

    let runners = state.get_all_runners().await;
    for holder_pid in holders {
        // Find the registered runner (if any) that owns this PID.
        let mut owner_match: Option<(String, bool, bool)> = None; // (id, is_temp, registry_running)
        for managed in &runners {
            let runner = managed.runner.read().await;
            if runner.pid == Some(holder_pid) && runner.running {
                let is_temp = crate::process::manager::is_temp_runner(&managed.config.id);
                owner_match = Some((managed.config.id.clone(), is_temp, true));
                break;
            }
        }

        match owner_match {
            None => {
                // Orphan — no registered runner claims this PID, or the entry
                // that claims it has running=false / pid=None. Either way the
                // supervisor cannot reach it via its API; kill directly.
                warn!(
                    "Slot {} exe held by orphan PID {} (no registered runner claims it). Killing.",
                    slot.id, holder_pid
                );
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Warn,
                        format!(
                            "Slot {} exe locked by orphan PID {} — killing to free build artifact",
                            slot.id, holder_pid
                        ),
                    )
                    .await;
                if let Err(e) = kill_by_pid(holder_pid).await {
                    warn!("kill_by_pid({}) failed: {}", holder_pid, e);
                }
            }
            Some((runner_id, is_temp, _running)) if is_temp => {
                // Registered temp runner is running directly from the slot exe.
                // Stop via API (graceful). Indicates Fix B's invariant was
                // violated — log so it's visible.
                warn!(
                    "Slot {} exe held by registered temp runner '{}' (PID {}) — stopping to free build artifact. \
                     This indicates start_managed_runner fell back to source_exe; investigate.",
                    slot.id, runner_id, holder_pid
                );
                if let Err(e) = crate::process::manager::stop_runner_by_id(state, &runner_id).await
                {
                    warn!(
                        "stop_runner_by_id('{}') failed: {} — escalating to direct kill",
                        runner_id, e
                    );
                    let _ = kill_by_pid(holder_pid).await;
                }
            }
            Some((runner_id, _is_temp, _running)) => {
                // Registered primary/named runner running from a slot exe.
                // Refuse to touch it — that's user-managed. Surface a hard
                // error so the build doesn't silently corrupt their session.
                let msg = format!(
                    "Slot {} exe locked by registered non-temp runner '{}' (PID {}). \
                     Refusing to auto-kill a user-managed runner. \
                     Stop it via the supervisor API or investigate why it is running directly from the slot binary.",
                    slot.id, runner_id, holder_pid
                );
                error!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Build, LogLevel::Error, &msg)
                    .await;
                return Err(SupervisorError::Other(msg));
            }
        }
    }

    // Re-poll after kills so the OS can release the file handle.
    let post_kill_attempts = 10; // 10 × 500ms = 5s
    for attempt in 1..=post_kill_attempts {
        match std::fs::OpenOptions::new().write(true).open(&exe_path) {
            Ok(_) => {
                let msg = format!(
                    "Slot {} exe unlocked {}ms after killing holder(s)",
                    slot.id,
                    attempt * 500
                );
                info!("{}", msg);
                state.logs.emit(LogSource::Build, LogLevel::Info, msg).await;
                return Ok(());
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    let msg = format!(
        "Slot {} exe still locked after killing holders — build will likely fail",
        slot.id
    );
    warn!("{}", msg);
    state
        .logs
        .emit(LogSource::Build, LogLevel::Warn, &msg)
        .await;
    Ok(())
}

/// Stop registered runners whose live process is running directly out of a
/// build-pool slot dir before a cargo build.
///
/// In normal operation every runner launches from a copy at
/// `target/debug/qontinui-runner-{id}.exe`, so this loop is a no-op. When
/// `start_managed_runner`'s copy step has previously fallen back to
/// `source_exe` (the slot binary), the resulting runner holds a slot exe
/// open and would block any cargo build that tries to overwrite it. Catch
/// that here with a graceful stop. `free_slot_exe` is the second-line
/// defence: it kicks in if a holder remains after this returns, including
/// orphan PIDs no registered runner claims.
///
/// We stop temp runners eagerly (they're cheap to recreate). For named or
/// primary runners running from a slot exe we log loudly but do not
/// auto-stop — the user's session shouldn't disappear from under them; the
/// build will surface a hard error via `free_slot_exe` so the operator can
/// resolve it intentionally.
async fn stop_exe_runners_for_build(state: &SharedState) {
    let runners = state.get_all_runners().await;
    for managed in &runners {
        let (running, pid) = {
            let runner = managed.runner.read().await;
            (runner.running, runner.pid)
        };
        if !running {
            continue;
        }
        let Some(pid) = pid else {
            continue;
        };

        // Resolve the live exe path for this PID. If it isn't running out
        // of the build pool, leave it alone.
        let exe_path = match resolve_pid_exe_path(pid).await {
            Some(p) => p,
            None => continue,
        };
        let in_slot = exe_path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("slot-"));
        if !in_slot {
            continue;
        }

        if crate::process::manager::is_temp_runner(&managed.config.id) {
            info!(
                "Stopping temp runner '{}' (PID {}) running from slot exe {:?} before build",
                managed.config.name, pid, exe_path
            );
            if let Err(e) =
                crate::process::manager::stop_runner_by_id(state, &managed.config.id).await
            {
                warn!(
                    "Failed to stop temp runner '{}' before build: {}",
                    managed.config.name, e
                );
            }
        } else {
            warn!(
                "Registered non-temp runner '{}' (PID {}) is running from slot exe {:?}. \
                 Refusing to auto-stop a user-managed runner; build will fail with a \
                 descriptive error. Stop it via the supervisor API or investigate why \
                 it launched from the slot binary.",
                managed.config.name, pid, exe_path
            );
        }
    }
}

/// Look up the executable path of a live PID. Returns `None` when the
/// process is gone or sysinfo could not read its image path.
///
/// Thin wrapper over `crate::process::windows::pid_exe_path` so callers in
/// this file can read like `resolve_pid_exe_path(pid)` and the sysinfo
/// plumbing lives in one place.
async fn resolve_pid_exe_path(pid: u32) -> Option<std::path::PathBuf> {
    pid_exe_path(pid).await
}

// =============================================================================
// Pre-warm
// =============================================================================

/// Timeout per slot's pre-warm `cargo check`.
const PREWARM_TIMEOUT_SECS: u64 = 60;

/// Sweep each slot's target dir for stale `.cargo-lock` advisory files left
/// behind by a previous supervisor that was killed mid-build.
///
/// Cargo deletes `.cargo-lock` on graceful exit; a `.cargo-lock` whose mtime
/// predates this supervisor process's start time is from a prior process and
/// can be safely removed. Locks newer than supervisor start belong to a build
/// in flight on this process and must not be touched.
///
/// Best-effort: any IO error is logged at warn level and processing continues
/// with the next slot. Never aborts startup.
pub async fn cleanup_stale_slot_locks(state: &crate::state::SharedState) {
    let supervisor_started_at = state.supervisor_started_at;
    let slots: Vec<Arc<BuildSlot>> = state.build_pool.slots.clone();
    for slot in &slots {
        sweep_slot_for_stale_locks(slot, supervisor_started_at).await;
        check_slot_fingerprint(slot).await;
    }
}

async fn sweep_slot_for_stale_locks(
    slot: &Arc<BuildSlot>,
    supervisor_started_at: std::time::SystemTime,
) {
    let mut stack: Vec<std::path::PathBuf> = vec![slot.target_dir.clone()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        "Slot {}: read_dir {:?} failed during stale-lock sweep: {}",
                        slot.id, dir, e
                    );
                }
                continue;
            }
        };
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    warn!(
                        "Slot {}: next_entry under {:?} failed: {}",
                        slot.id, dir, e
                    );
                    break;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let is_cargo_lock = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == ".cargo-lock");
            if !is_cargo_lock {
                continue;
            }
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        "Slot {}: metadata for {:?} failed: {}",
                        slot.id, path, e
                    );
                    continue;
                }
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if mtime < supervisor_started_at {
                let mtime_str = chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339();
                match tokio::fs::remove_file(&path).await {
                    Ok(_) => {
                        info!(
                            "Removed stale .cargo-lock from slot {} at {:?} (mtime: {})",
                            slot.id, path, mtime_str
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Slot {}: failed to remove stale .cargo-lock {:?}: {}",
                            slot.id, path, e
                        );
                    }
                }
            }
        }
    }
}

async fn check_slot_fingerprint(slot: &Arc<BuildSlot>) {
    let fingerprint = slot.target_dir.join("debug").join(".fingerprint");
    let exists = tokio::fs::metadata(&fingerprint).await.is_ok();
    if !exists {
        let exe = slot.target_dir.join("debug").join("qontinui-runner.exe");
        if tokio::fs::metadata(&exe).await.is_ok() {
            warn!(
                "Slot {}: target/debug/.fingerprint missing but exe is present at {:?}; \
                 incremental state may be inconsistent. Consider a manual \
                 `cargo clean` (CARGO_TARGET_DIR={:?}).",
                slot.id, exe, slot.target_dir
            );
        }
    }
}

/// Pre-warm each build slot's incremental cache by running `cargo check`.
///
/// Spawned as `tokio::spawn` after the HTTP server binds so it doesn't delay
/// startup. Skipped when `--no-prewarm` is set.
pub async fn prewarm_build_slots(state: crate::state::SharedState) {
    cleanup_stale_slot_locks(&state).await;

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
                .emit(
                    LogSource::Build,
                    LogLevel::Warn,
                    format!("Prewarm of slot {} failed: {}", slot.id, e),
                )
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
    state
        .build_pool
        .queue_depth
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let permit_result = state.build_pool.permits.clone().acquire_owned().await;
    state
        .build_pool
        .queue_depth
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let _permit = permit_result
        .map_err(|_| SupervisorError::Other("Build pool semaphore closed".to_string()))?;

    // Re-check after acquiring: another caller may have populated this slot.
    let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
    if exe_path.exists() {
        info!(
            "Slot {} populated while waiting for permit, skipping prewarm",
            slot.id
        );
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
    let _slot_guard = SlotGuard {
        slot: slot.clone(),
        state: state.clone(),
    };

    info!(
        "Prewarming build slot {} (target: {:?})...",
        slot.id, slot.target_dir
    );
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!("Prewarming slot {}...", slot.id),
        )
        .await;

    let start = std::time::Instant::now();

    let args: Vec<&str> = vec![
        "check",
        "--bin",
        "qontinui-runner",
        "--features",
        "custom-protocol",
    ];

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

    let mut child = child_result.map_err(|e| {
        SupervisorError::Process(format!("Failed to spawn prewarm cargo check: {}", e))
    })?;

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
                    .emit(
                        LogSource::Build,
                        LogLevel::Info,
                        format!("[prewarm slot {}] {}", slot_id, line),
                    )
                    .await;
            }
        });
    }

    match tokio::time::timeout(Duration::from_secs(PREWARM_TIMEOUT_SECS), child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            let ms = start.elapsed().as_millis();
            info!("Prewarmed slot {} in {}ms", slot.id, ms);
            state
                .logs
                .emit(
                    LogSource::Build,
                    LogLevel::Info,
                    format!("Prewarmed slot {} in {}ms", slot.id, ms),
                )
                .await;
            // Set last_successful_slot only if no real build has run yet.
            let mut last = state.build_pool.last_successful_slot.write().await;
            if last.is_none() {
                *last = Some(slot.id);
            }
            Ok(())
        }
        Ok(Ok(status)) => {
            warn!(
                "Prewarm cargo check for slot {} exited with {}",
                slot.id, status
            );
            Err(SupervisorError::BuildFailed(format!(
                "Prewarm exited with {}",
                status
            )))
        }
        Ok(Err(e)) => Err(SupervisorError::Process(format!(
            "Prewarm process error: {}",
            e
        ))),
        Err(_) => {
            warn!(
                "Prewarm of slot {} timed out after {}s, killing",
                slot.id, PREWARM_TIMEOUT_SECS
            );
            let _ = child.kill().await;
            Err(SupervisorError::Timeout(format!(
                "Prewarm timed out after {}s",
                PREWARM_TIMEOUT_SECS
            )))
        }
    }
}

// =============================================================================
// Last-known-good (LKG) capture
// =============================================================================

/// Copy the freshly-built slot exe to `target-pool/lkg/qontinui-runner.exe`
/// and write a `lkg.json` sidecar with `{built_at, source_slot, exe_size}`.
///
/// Both writes go through a temp-file + atomic rename so a crash partway
/// through cannot leave the LKG dir holding a torn binary or a sidecar that
/// describes a different exe than the one on disk.
///
/// Called from the build-success path with the slot whose cargo build just
/// returned `Ok`. On any failure, the previous LKG (if any) is left intact
/// — the caller logs the error but the build still counts as succeeded.
async fn update_lkg_after_success(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
) -> Result<(), SupervisorError> {
    let source_exe = state.config.runner_exe_path_for_slot(slot.id);
    if !source_exe.exists() {
        return Err(SupervisorError::Process(format!(
            "build succeeded but slot {} exe not found at {:?}",
            slot.id, source_exe
        )));
    }

    let lkg_dir = state.config.lkg_dir();
    if let Err(e) = std::fs::create_dir_all(&lkg_dir) {
        return Err(SupervisorError::Process(format!(
            "failed to create lkg dir {:?}: {}",
            lkg_dir, e
        )));
    }

    let final_exe = state.config.lkg_exe_path();
    // Per-slot temp filenames so two concurrent successful builds can't
    // clobber each other's in-flight copies. Without the suffix, slot 0's
    // remove_file would race slot 1's copy/rename and the final exe could
    // end up holding one slot's bytes while the sidecar claims the other's.
    let tmp_exe = lkg_dir.join(format!("qontinui-runner.exe.tmp.{}", slot.id));
    // Best-effort cleanup of any leftover tmp file from a previous crash on
    // THIS slot — slot ids are stable across builds so a stale file from
    // last session is still ours to clean.
    let _ = std::fs::remove_file(&tmp_exe);

    std::fs::copy(&source_exe, &tmp_exe).map_err(|e| {
        SupervisorError::Process(format!(
            "failed to copy {:?} -> {:?}: {}",
            source_exe, tmp_exe, e
        ))
    })?;

    let exe_size = std::fs::metadata(&tmp_exe)
        .map(|m| m.len())
        .map_err(|e| SupervisorError::Process(format!("stat {:?}: {}", tmp_exe, e)))?;

    // Atomic replace. Rust 1.65+ implements `std::fs::rename` on Windows via
    // `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` for same-volume renames, so
    // dropping the prior remove_file removes the brief window where the LKG
    // dir held a sidecar but no exe. If the dest is held open by another
    // process the rename returns the real error.
    std::fs::rename(&tmp_exe, &final_exe).map_err(|e| {
        SupervisorError::Process(format!(
            "failed to rename {:?} -> {:?}: {}",
            tmp_exe, final_exe, e
        ))
    })?;

    let info = LkgInfo {
        built_at: chrono::Utc::now(),
        source_slot: slot.id,
        exe_size,
    };

    let final_meta = state.config.lkg_metadata_path();
    let tmp_meta = lkg_dir.join(format!("lkg.json.tmp.{}", slot.id));
    let _ = std::fs::remove_file(&tmp_meta);
    let json = serde_json::to_string_pretty(&info)
        .map_err(|e| SupervisorError::Process(format!("serialize lkg.json: {}", e)))?;
    std::fs::write(&tmp_meta, json.as_bytes())
        .map_err(|e| SupervisorError::Process(format!("write {:?}: {}", tmp_meta, e)))?;
    std::fs::rename(&tmp_meta, &final_meta).map_err(|e| {
        SupervisorError::Process(format!(
            "failed to rename {:?} -> {:?}: {}",
            tmp_meta, final_meta, e
        ))
    })?;

    info!(
        "LKG updated from slot {} ({} bytes, built_at {})",
        info.source_slot, info.exe_size, info.built_at
    );
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!(
                "LKG runner binary updated (slot {}, {} bytes)",
                info.source_slot, info.exe_size
            ),
        )
        .await;

    let mut lkg_lock = state.build_pool.last_known_good.write().await;
    *lkg_lock = Some(info);
    Ok(())
}

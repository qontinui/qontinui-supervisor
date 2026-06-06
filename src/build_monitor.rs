use regex::Regex;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::build_timeout_secs;
use crate::diagnostics::DiagnosticEventKind;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::guarded_command::{GuardedCommand, GuardedOutcome};
#[cfg(target_os = "windows")]
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
        // Path 1 (sync, fast): try to clear the slot in-place. Clear BOTH the
        // busy marker and the per-slot cancel token (same try-write-then-spawn
        // pattern), so a stale token from this build can never pre-empt the
        // NEXT build that lands on this slot.
        let cleared_busy_inline = if let Ok(mut busy) = self.slot.busy.try_write() {
            *busy = None;
            true
        } else {
            false
        };
        let cleared_cancel_inline = if let Ok(mut cancel) = self.slot.cancel.try_write() {
            *cancel = None;
            true
        } else {
            false
        };

        // Path 2 (async fallback): if we couldn't take a lock here, OR after
        // we've cleared it, schedule a task that recomputes the global flag
        // from authoritative slot state. Spawn unconditionally so the recompute
        // always runs — `any_slot_busy(state)` requires async access to every
        // slot's RwLock, which we can't do from Drop.
        let slot = self.slot.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            if !cleared_busy_inline {
                let mut busy = slot.busy.write().await;
                *busy = None;
            }
            if !cleared_cancel_inline {
                let mut cancel = slot.cancel.write().await;
                *cancel = None;
            }
            // Reconcile the global legacy flag. Authoritative source is
            // `any_slot_busy` — never trust the cached flag during recovery.
            let any_busy = any_slot_busy(&state).await;
            let mut build = state.build.write().await;
            build.build_in_progress = any_busy;
        });
    }
}

/// RAII lease over one build-pool slot: a held semaphore permit + a claimed
/// idle slot (marked busy with the caller's [`BuildInfo`]) + a freshly-minted
/// per-slot [`CancellationToken`]. Bundles the permit acquisition, slot claim,
/// cancel-token publish, and the [`SlotGuard`] cleanup that both the runner
/// build (`run_cargo_build_with_dir`) and the generalized
/// `/build/submit` path (`build_submissions::run_submission`) need, so the
/// slot accounting that `GET /builds` derives from `slot.busy` is identical for
/// both — there is ONE slot-accounting path, not two.
///
/// Drop order matters: the [`SlotGuard`] frees `busy`/`cancel` first, then the
/// permit is released for the next waiter.
pub struct BuildSlotLease {
    pub slot: Arc<BuildSlot>,
    /// The cancel token published on `slot.cancel`. Hand this to
    /// `GuardedCommand::cancel_token` so a `cancel_slot`/`cancel_builds_for_*`
    /// call pre-empts this build.
    pub cancel: CancellationToken,
    // Dropped LAST: releases the semaphore permit after the guard clears the
    // slot. Field order in a struct = drop order, so `_guard` must precede
    // `_permit`.
    _guard: SlotGuard,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl BuildSlotLease {
    /// Acquire a permit (blocking until a slot frees, bumping `queue_depth`
    /// while waiting), claim an idle slot with `info`, mint + publish a cancel
    /// token, and arm the [`SlotGuard`]. The returned lease keeps the slot busy
    /// for `GET /builds` visibility until it is dropped.
    pub async fn acquire(state: &SharedState, info: BuildInfo) -> Result<Self, SupervisorError> {
        state
            .build_pool
            .queue_depth
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let permit_result = state.build_pool.permits.clone().acquire_owned().await;
        state
            .build_pool
            .queue_depth
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let permit = permit_result
            .map_err(|_| SupervisorError::Other("Build pool semaphore closed".to_string()))?;

        let slot = state.build_pool.claim_idle_slot(info).await;
        let cancel = CancellationToken::new();
        {
            let mut guard = slot.cancel.write().await;
            *guard = Some(cancel.clone());
        }
        let guard = SlotGuard {
            slot: slot.clone(),
            state: state.clone(),
        };
        Ok(Self {
            slot,
            cancel,
            _guard: guard,
            _permit: permit,
        })
    }

    /// Split the lease into its two RAII pieces so a caller can control their
    /// drop timing independently. `run_cargo_build_with_dir` needs to release
    /// the [`SlotGuard`] (clearing `slot.busy`/`slot.cancel`) BEFORE it
    /// recomputes `any_slot_busy` for the legacy `build_in_progress` flag, yet
    /// hold the semaphore permit until ALL post-build bookkeeping (LKG capture,
    /// `last_successful_slot`, sha sidecar) has finished — otherwise a queued
    /// build could grab a permit, `claim_idle_slot` this very slot, and start
    /// overwriting its exe mid-`update_lkg_after_success` copy. Returning the
    /// guard first and the permit second documents (and the binding order
    /// enforces) that drop ordering at the call site.
    fn into_parts(self) -> (SlotGuard, tokio::sync::OwnedSemaphorePermit) {
        (self._guard, self._permit)
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
///
/// `build_dir_override` is always `None` for this entry point; callers that
/// need to compile a source tree other than `state.config.project_dir`
/// (e.g. a detached git-ref worktree built by `spawn-test {git_ref}`) call
/// [`run_cargo_build_with_dir`] directly.
pub async fn run_cargo_build_with_requester(
    state: &SharedState,
    requester_id: Option<String>,
) -> Result<(), SupervisorError> {
    run_cargo_build_with_dir(state, requester_id, None, false).await
}

/// Run a cargo build, optionally compiling a source tree other than
/// `state.config.project_dir`.
///
/// `build_dir_override`:
/// - `None` ⇒ cargo's `current_dir` is `state.config.project_dir` (the live
///   working tree), exactly as the legacy behavior.
/// - `Some(dir)` ⇒ cargo's `current_dir` is `dir` (must be a runner
///   `src-tauri` directory). Slot isolation is unchanged — `CARGO_TARGET_DIR`
///   still points at the claimed slot's `target_dir`, and the built exe is
///   resolved from that slot exactly as today. Only the *source* tree
///   differs.
///
/// `force_frontend_build` (Phase 3 `frontend_only`): when true AND
/// `build_dir_override` is set, the worktree frontend prebuild ALWAYS runs
/// `pnpm run build` even if the dist-present idempotency gate would skip it —
/// so a TS edit made after the tree's last build is re-embedded rather than
/// serving the stale dist. `pnpm install` is still skipped when the
/// `node_modules` marker is present. No effect on a live-tree build (no
/// override).
pub async fn run_cargo_build_with_dir(
    state: &SharedState,
    requester_id: Option<String>,
    build_dir_override: Option<PathBuf>,
    force_frontend_build: bool,
) -> Result<(), SupervisorError> {
    // Acquire a build-pool lease: a permit (blocking until a slot frees) + a
    // claimed idle slot marked busy with our BuildInfo + a published cancel
    // token + the SlotGuard cleanup. ONE slot-accounting path, shared with
    // `/build/submit` (see [`BuildSlotLease`]). The guard clears
    // `slot.busy`/`slot.cancel` and reconciles the global `build_in_progress`
    // flag on every exit path (happy path, `?`, panic, task cancellation).
    let info = BuildInfo {
        started_at: chrono::Utc::now(),
        requester_id,
        rebuild_kind: "exe".to_string(),
    };
    let lease = BuildSlotLease::acquire(state, info).await?;
    let slot = lease.slot.clone();
    // Split the lease so we can drop the slot guard (clearing busy/cancel)
    // before the `any_slot_busy` recompute, while holding the permit until the
    // end so no queued build can re-claim this slot mid post-build bookkeeping.
    let (slot_guard, build_permit) = lease.into_parts();

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
        slot.id,
        build_dir_override
            .as_deref()
            .unwrap_or(state.config.project_dir.as_path()),
        slot.target_dir
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
    #[cfg(target_os = "windows")]
    cleanup_orphaned_build_processes().await;

    // Wait for the runner exe to be unlocked (Windows holds file locks briefly after process exit).
    // If the lock persists, identify the holder and kill orphans / stop registered temp runners.
    // Returns Err only if the holder is a user-managed primary/named runner; in that case we
    // skip cargo entirely so we don't masquerade a pre-build conflict as a build failure.
    let build_start = std::time::Instant::now();
    #[cfg(target_os = "windows")]
    let result = match free_slot_exe(state, &slot).await {
        Ok(()) => {
            run_build_inner(
                state,
                &slot,
                build_dir_override.as_deref(),
                force_frontend_build,
            )
            .await
        }
        Err(e) => Err(e),
    };
    #[cfg(not(target_os = "windows"))]
    let result = run_build_inner(
        state,
        &slot,
        build_dir_override.as_deref(),
        force_frontend_build,
    )
    .await;
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
    drop(slot_guard);

    // If this build succeeded, record the slot as the most recent successful one.
    // Readers of `rebuild: false` use this to locate the exe to copy.
    if result.is_ok() {
        let mut last = state.build_pool.last_successful_slot.write().await;
        *last = Some(slot.id);
        drop(last);

        // Stamp the slot's exe with the HEAD SHA so resolve_source_exe and
        // /builds can detect drift across slots (a fresh exe staged into one
        // slot while a stale exe lingers in another). Best-effort: no git, no
        // sidecar, no warning surface — but the build still succeeded.
        write_slot_sha_sidecar(state, &slot).await;

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
    drop(build_permit);

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

/// Cancel the in-flight build occupying `slot_id`, if any.
///
/// Fires the slot's [`CancellationToken`]. The occupying build's
/// `GuardedCommand::run` observes the token, drops its `CommandJob` (the kernel
/// kills the whole subprocess tree), and returns `GuardedOutcome::Cancelled`;
/// its `SlotGuard` then frees `busy` + `cancel` and the npm lock on the normal
/// exit path. No-op if the slot is idle or out of range. Returns true if a
/// token was fired.
#[allow(dead_code)]
pub async fn cancel_slot(state: &SharedState, slot_id: usize) -> bool {
    let Some(slot) = state.build_pool.slots.get(slot_id) else {
        return false;
    };
    let token = slot.cancel.read().await.clone();
    match token {
        Some(token) => {
            token.cancel();
            state
                .logs
                .emit(
                    LogSource::Build,
                    LogLevel::Info,
                    format!("Cancelled in-flight build on slot {}", slot_id),
                )
                .await;
            true
        }
        None => false,
    }
}

/// Cancel every in-flight build whose `requester_id` contains `needle`.
///
/// Used by the rebuild-and-restart flow to pre-empt a prior build targeting the
/// SAME runner before claiming a fresh slot — so a rapid second restart doesn't
/// pile a second cargo build on top of the first (its tree is killed, its slot
/// + npm lock are freed). Returns the number of builds cancelled.
pub async fn cancel_builds_for_requester(state: &SharedState, needle: &str) -> usize {
    let mut cancelled = 0;
    for slot in &state.build_pool.slots {
        let matches = {
            let busy = slot.busy.read().await;
            busy.as_ref()
                .and_then(|info| info.requester_id.as_deref())
                .map(|r| r.contains(needle))
                .unwrap_or(false)
        };
        if matches {
            let token = slot.cancel.read().await.clone();
            if let Some(token) = token {
                token.cancel();
                cancelled += 1;
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Info,
                        format!(
                            "Cancelled in-flight build on slot {} (requester contains {:?})",
                            slot.id, needle
                        ),
                    )
                    .await;
            }
        }
    }
    cancelled
}

async fn run_build_inner(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    build_dir_override: Option<&std::path::Path>,
    force_frontend_build: bool,
) -> Result<(), SupervisorError> {
    // Source tree cargo will compile. `None` ⇒ the live project dir (legacy);
    // `Some(dir)` ⇒ a detached git-ref worktree's `src-tauri`.
    let cargo_cwd: &std::path::Path = build_dir_override.unwrap_or(&state.config.project_dir);

    // When building a detached git-ref worktree, the worktree starts empty —
    // no `node_modules/`, no `dist/`. The legacy `state.config.project_dir`
    // tree has both because devs run `pnpm install` + `pnpm run build`
    // routinely; a fresh `git worktree add` does not. Without this step the
    // subsequent `pnpm run build` fails (`tsc`/`vite`/`ui-bridge-build-ir`
    // not installed) and even if it didn't, cargo's
    // `tauri::generate_context!` macro would panic on the missing
    // `<wt>/dist/index.html` (the empirical 2026-05-21 manual-test failure
    // mode this gate exists to prevent).
    //
    // Runs ONLY when `build_dir_override` is set. The live-tree code path
    // below is unchanged byte-for-byte.
    if let Some(src_tauri) = build_dir_override {
        let wt_root: PathBuf = src_tauri.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
            SupervisorError::Other(format!(
                "build_dir_override src-tauri path {:?} has no parent",
                src_tauri
            ))
        })?;
        prebuild_worktree_frontend(state, slot, &wt_root, force_frontend_build).await?;
    }
    // The frontend is embedded in the binary via tauri_build, so we must run
    // `pnpm run build` first to produce a fresh dist/ before cargo build.
    //
    // Frontend builds are serialized across slots via `build_pool.npm_lock`:
    // Tauri's `rust-embed` pulls from a single `dist/` directory, so two
    // concurrent npm builds would corrupt the output. The lock is held ONLY
    // for the npm invocation (~12s), not the whole cargo build (~180s), so
    // this is a much smaller serialization point than the legacy global flag.
    {
        // For a git-ref worktree build the frontend must also come from the
        // worktree (parent of its `src-tauri`), not the live tree's dist/.
        // Otherwise cargo would embed the live tree's dist/ into a binary
        // compiled from the ref's source — defeating the provenance goal.
        let npm_dir = match build_dir_override {
            Some(src_tauri) => src_tauri
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| state.config.runner_npm_dir()),
            None => state.config.runner_npm_dir(),
        };
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
                format!("Slot {}: building frontend (pnpm run build)...", slot.id),
            )
            .await;
        info!("Slot {}: building frontend in {:?}", slot.id, npm_dir);

        let npm_cancel = slot.cancel.read().await.clone();
        let npm_result = run_pnpm_command(&npm_dir, "run build", npm_cancel).await;

        match npm_result {
            Ok(output) if output.status.success() => {
                // Defense-in-depth: even though pnpm exited 0, verify the
                // dist/ output is actually present and non-empty BEFORE
                // flipping `frontend_stale = false`. The pnpm step is
                // serialized inside this supervisor via `npm_lock`, but
                // a concurrent EXTERNAL `pnpm run build` (multi-agent
                // machines, manual builds) can wipe dist/ between pnpm
                // exit and cargo's embed. We've also seen empty-output
                // regressions where vite exits 0 with nothing written
                // (proj_issue_runner_npm_build_safari13_target.md).
                //
                // Existence + non-emptiness only — leave mtime drift to
                // `routes::runners::check_dist_freshness` which runs on
                // every spawn. We deliberately don't compare against
                // package.json/tsconfig.json/vite.config.ts here:
                // package.json is touched on every `pnpm install`, which
                // would produce a flood of false positives.
                if dist_index_ok(&npm_dir) {
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
                } else {
                    let msg = format!(
                        "Slot {}: frontend_stale: pnpm exit 0 but dist/index.html missing or empty (likely concurrent external `pnpm run build` wiped dist/, or empty-output regression)",
                        slot.id
                    );
                    error!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Build, LogLevel::Error, &msg)
                        .await;
                    *slot.frontend_stale.write().await = true;
                    {
                        let mut history = slot.history.write().await;
                        history.last_error = Some(
                            "frontend_stale: pnpm exit 0 but dist/index.html missing or empty (likely concurrent external `pnpm run build` wiped dist/, or empty-output regression)".to_string()
                        );
                    }
                    // Continue with cargo build — the binary will still
                    // build (rust-embed of an empty dir succeeds), but
                    // the slot flag now honestly reflects the broken
                    // state and spawn-test will surface it via
                    // `frontend_stale_reason: "build_failed"`.
                }
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
                // successful pnpm build clears it.
                *slot.frontend_stale.write().await = true;
                // Record the pnpm failure reason in the slot's rolling history
                // so `GET /builds` can show it even though the cargo build may
                // ultimately succeed.
                {
                    let mut history = slot.history.write().await;
                    history.last_error = Some(format!(
                        "frontend_stale: pnpm run build failed: {}",
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
                        "frontend_stale: pnpm run build failed to spawn: {}",
                        e
                    ));
                }
            }
        }
        // npm_guard drops here, releasing the frontend build lock before cargo starts.
    }

    // Diagnostic-only: emit a WARN if the runner working tree isn't on
    // origin/main. Multi-agent flow can leave the tree on a feature branch
    // between sessions, and cargo silently compiles whatever's there. The
    // warn surfaces the mismatch in supervisor.log so a caller intending to
    // test main-side code has a chance to spot it before reading `git_sha`
    // on the spawn response. See qontinui-supervisor#21.
    warn_if_working_tree_off_main(state, slot.id).await;

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

    // Reset the per-slot full-build log at the start of each build so a reader
    // hitting `GET /builds/{slot_id}/log` while a build is in flight doesn't
    // see a confusing mix of "old log + still building". `None` = "no log
    // captured yet for the current build attempt".
    *slot.last_build_log.write().await = None;

    // Route cargo through the job-guarded runner: it spawns cargo in a
    // kill-on-close Windows JobObject so a timeout/cancel kills the WHOLE tree
    // (rustc, the linker, any grandchild holding the pipe open) instead of just
    // the direct child — the Bug-1 hang this consolidation closes. Merged
    // stderr lines stream to this slot's SSE broadcast sender as they're read.
    let cancel = slot.cancel.read().await.clone();
    let mut guarded = GuardedCommand::new("cargo", Duration::from_secs(build_timeout_secs()))
        .args(CARGO_BUILD_ARGS)
        .current_dir(cargo_cwd)
        // Redirect cargo output to this slot's isolated target dir so
        // concurrent builds on other slots don't contend on the same target/.
        .env("CARGO_TARGET_DIR", &slot.target_dir)
        .stream_lines(slot.log_stream.clone());
    if let Some(token) = cancel {
        guarded = guarded.cancel_token(token);
    }

    let outcome = guarded
        .run()
        .await
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn cargo build: {}", e)))?;

    // Convert the guarded outcome into (status, captured stderr lines). The
    // SSE/log emission that the legacy manual stderr task did per-line is now
    // handled below in one pass over the captured bytes; `stream_lines` already
    // fanned the lines out to SSE subscribers live.
    let (status, all_stderr_lines): (std::process::ExitStatus, Vec<String>) = match outcome {
        GuardedOutcome::Exited(output) => {
            let lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(|l| l.to_string())
                .collect();
            (output.status, lines)
        }
        GuardedOutcome::TimedOut { after, partial } => {
            let timeout_secs = after.as_secs();
            let tail = tail_bytes_keep_utf8(
                &String::from_utf8_lossy(&partial.stderr),
                LAST_BUILD_STDERR_SHORT_TAIL_BYTES,
            );
            warn!(
                "Build timed out after {}s, killed the cargo tree. stderr tail: {}",
                timeout_secs, tail
            );
            return Err(SupervisorError::Timeout(format!(
                "Build timed out after {}s. stderr tail: {}",
                timeout_secs, tail
            )));
        }
        GuardedOutcome::Cancelled { .. } => {
            warn!("Slot {}: cargo build cancelled (superseded)", slot.id);
            return Err(SupervisorError::Cancelled(format!(
                "Slot {} cargo build cancelled (superseded by a newer build)",
                slot.id
            )));
        }
    };

    // Mirror the captured lines into the supervisor log buffer (the live SSE
    // fanout already happened via `stream_lines` during the build). Classify
    // error lines for the failure summary.
    let mut error_lines: Vec<String> = Vec::new();
    for line in &all_stderr_lines {
        let is_error = BUILD_ERROR_PATTERNS.iter().any(|p| p.is_match(line));
        let level = if is_error {
            LogLevel::Error
        } else {
            LogLevel::Info
        };
        state.logs.emit(LogSource::Build, level, line).await;
        if is_error {
            error_lines.push(line.clone());
        }
    }

    // Store full stderr for smart rebuild AI fix prompt
    let joined_stderr = all_stderr_lines.join("\n");
    if !all_stderr_lines.is_empty() {
        let mut build = state.build.write().await;
        build.last_build_stderr = Some(joined_stderr.clone());
    }

    // Record the full combined log on the slot regardless of build outcome
    // so `GET /builds/{slot_id}/log` works after every attempt. Cap at
    // LAST_BUILD_LOG_MAX_BYTES — preserve the tail since cargo's actual
    // error messages live near the end of its output.
    {
        let captured_at = chrono::Utc::now();
        let log = if joined_stderr.is_empty() {
            String::new()
        } else {
            crate::state::tail_bytes_keep_utf8(
                &joined_stderr,
                crate::state::LAST_BUILD_LOG_MAX_BYTES,
            )
        };
        *slot.last_build_log.write().await = Some((captured_at, log));
    }

    if status.success() {
        // HARD GATE: cargo succeeded, but a binary whose embedded frontend is
        // missing/empty renders a blank "asset not found: index.html" window.
        // Such a build must NEVER be promoted to LKG / `last_successful_slot`
        // and shipped to the operator. `frontend_stale == true` means the
        // pnpm step failed OR `dist/index.html` was missing/empty
        // (`!dist_index_ok`) earlier in this function, so cargo just embedded
        // a broken/stale frontend. Convert that into a hard build error here
        // so the outer `run_cargo_build_with_dir` skips LKG promotion + the
        // `last_successful_slot` update (both gated on `result.is_ok()`) and
        // surfaces `build.build_error_detected = true` + `last_build_error`
        // to the operator instead of a silent "successful enough" LKG.
        if *slot.frontend_stale.read().await {
            // Prefer the precise reason recorded by the frontend-build branch
            // above (pnpm failure vs. empty dist) for the operator-facing error.
            let reason = slot
                .history
                .read()
                .await
                .last_error
                .clone()
                .unwrap_or_else(|| {
                    "frontend build failed or dist/index.html missing/empty".to_string()
                });
            let msg = format!(
                "Slot {}: cargo build succeeded but the frontend is broken \u{2014} \
                 NOT promoting to LKG/last_successful_slot (the binary would render a \
                 blank \"asset not found: index.html\" window). {}",
                slot.id, reason
            );
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Build, LogLevel::Error, &msg)
                .await;
            return Err(SupervisorError::BuildFailed(msg));
        }
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
        // Reuse `joined_stderr` from above; identical to `all_stderr_lines.join("\n")`.
        let full_stderr = joined_stderr;

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
            format!(
                "{}\n\n--- cargo stderr (last 2KB) ---\n{}",
                base, short_tail
            )
        };
        error!("{}", error_summary);
        state
            .logs
            .emit(LogSource::Build, LogLevel::Error, &error_summary)
            .await;
        Err(SupervisorError::BuildFailed(error_summary))
    }
}

/// Prebuild the frontend inside a fresh spawn worktree before cargo runs.
///
/// A fresh `git worktree add --detach` produces an empty checkout — no
/// `node_modules/`, no `dist/`. The next `pnpm run build` would fail because
/// the dep binaries (`tsc`, `vite`, `ui-bridge-build-ir`, …) aren't
/// installed, and even if they were, cargo's `tauri::generate_context!`
/// would panic on the missing `<wt>/dist/index.html`. Idempotent: once both
/// `<wt>/node_modules/.bin/ui-bridge-build-ir` and `<wt>/dist/index.html`
/// exist, this returns immediately with the `frontend_prebuild_skipped`
/// log reason — repeated spawn-test calls on the same ref don't re-pay
/// the ~30s pnpm install cost.
///
/// The whole prebuild is serialized via `BuildPool.npm_lock` (the same
/// mutex the live-tree `pnpm run build` uses): `tsc` + `vite` are heavy
/// enough that two concurrent runs on the same machine routinely OOM in
/// CI, and the lock guarantees only one frontend build is in flight at a
/// time across all worktrees + the live tree.
///
/// On any failure (pnpm install non-zero exit, pnpm build non-zero exit, or
/// post-build `dist/index.html` still missing) returns
/// `SupervisorError::BuildFailed` with the cargo-style 2KB stderr tail
/// embedded so callers see what went wrong without trawling the supervisor
/// log.
async fn prebuild_worktree_frontend(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    wt_root: &std::path::Path,
    force_frontend_build: bool,
) -> Result<(), SupervisorError> {
    // Idempotency gate, split for the Phase 3 `frontend_only` fast path:
    //   * `needs_install` — the `node_modules/.bin/ui-bridge-build-ir` marker
    //     is absent, so `pnpm install` must run.
    //   * `needs_build`   — `dist/index.html` is absent, so `pnpm run build`
    //     must run.
    // Default (force_frontend_build=false): if BOTH artifacts already exist,
    // skip the whole prebuild (the historical behavior — repeated spawns on the
    // same ref don't re-pay install/build). `frontend_only:true` FORCES
    // `pnpm run build` regardless of `dist/index.html` presence, because a TS
    // edit made after the tree's last build would otherwise silently embed the
    // stale dist — exactly the case frontend_only exists for. `pnpm install`
    // is still skipped when the marker is present (the "fast" in fast path).
    let needs_install = frontend_install_marker_missing(wt_root);
    let needs_build = !dist_index_present(wt_root);

    if !needs_install && !needs_build && !force_frontend_build {
        info!(
            "Slot {}: frontend_prebuild_skipped — {:?} already has node_modules + dist/",
            slot.id, wt_root
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: frontend_prebuild_skipped — {:?} already has node_modules + dist/",
                    slot.id, wt_root
                ),
            )
            .await;
        return Ok(());
    }

    // Serialize against the live-tree pnpm step + any other worktree's
    // prebuild. Held for both `pnpm install` and `pnpm run build`.
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!(
                "Slot {}: waiting for npm lock (worktree frontend prebuild in {:?}, force_build={})",
                slot.id, wt_root, force_frontend_build
            ),
        )
        .await;
    let _npm_guard = state.build_pool.npm_lock.clone().lock_owned().await;

    // 1) pnpm install — produces node_modules/.bin/ui-bridge-build-ir +
    //    everything else `pnpm run build` needs. Use `--frozen-lockfile`
    //    when a pnpm-lock.yaml exists (reproducible, matches CI's
    //    `pnpm install --frozen-lockfile`); otherwise fall back to a plain
    //    `pnpm install`. Skipped when the marker already exists (so a
    //    `frontend_only` re-spawn pays only the `pnpm run build` cost).
    if needs_install {
        let has_lockfile = wt_root.join("pnpm-lock.yaml").exists();
        let install_args = if has_lockfile {
            "install --frozen-lockfile"
        } else {
            "install"
        };

        info!(
            "Slot {}: pnpm {} starting in {:?}",
            slot.id, install_args, wt_root
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: pnpm {} starting in {:?}",
                    slot.id, install_args, wt_root
                ),
            )
            .await;

        let install_started = std::time::Instant::now();
        let install_cancel = slot.cancel.read().await.clone();
        let install_output = run_pnpm_command(wt_root, install_args, install_cancel)
            .await
            .map_err(|e| {
                SupervisorError::BuildFailed(format!(
                    "pnpm {} failed to spawn in spawn worktree {:?}: {}",
                    install_args, wt_root, e
                ))
            })?;
        if !install_output.status.success() {
            let stderr_tail = tail_bytes_keep_utf8(
                &String::from_utf8_lossy(&install_output.stderr),
                LAST_BUILD_STDERR_SHORT_TAIL_BYTES,
            );
            return Err(SupervisorError::BuildFailed(format!(
                "pnpm {} failed in spawn worktree {:?} (exit {}): {}",
                install_args, wt_root, install_output.status, stderr_tail
            )));
        }
        let install_secs = install_started.elapsed().as_secs();
        info!(
            "Slot {}: pnpm {} completed in {:?} ({}s)",
            slot.id, install_args, wt_root, install_secs
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: pnpm {} completed in {:?} ({}s)",
                    slot.id, install_args, wt_root, install_secs
                ),
            )
            .await;
    } else {
        info!(
            "Slot {}: pnpm install skipped — node_modules marker present in {:?}",
            slot.id, wt_root
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: pnpm install skipped — node_modules marker present in {:?}",
                    slot.id, wt_root
                ),
            )
            .await;
    }

    // 2) pnpm run build — produces dist/index.html.
    info!("Slot {}: pnpm run build starting in {:?}", slot.id, wt_root);
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!("Slot {}: pnpm run build starting in {:?}", slot.id, wt_root),
        )
        .await;

    let build_started = std::time::Instant::now();
    let build_cancel = slot.cancel.read().await.clone();
    let build_output = run_pnpm_command(wt_root, "run build", build_cancel)
        .await
        .map_err(|e| {
            SupervisorError::BuildFailed(format!(
                "pnpm run build failed to spawn in spawn worktree {:?}: {}",
                wt_root, e
            ))
        })?;
    if !build_output.status.success() {
        let stderr_tail = tail_bytes_keep_utf8(
            &String::from_utf8_lossy(&build_output.stderr),
            LAST_BUILD_STDERR_SHORT_TAIL_BYTES,
        );
        return Err(SupervisorError::BuildFailed(format!(
            "pnpm run build failed in spawn worktree {:?} (exit {}): {}",
            wt_root, build_output.status, stderr_tail
        )));
    }
    let build_secs = build_started.elapsed().as_secs();
    info!(
        "Slot {}: pnpm run build completed in {:?} ({}s)",
        slot.id, wt_root, build_secs
    );
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!(
                "Slot {}: npm run build completed in {:?} ({}s)",
                slot.id, wt_root, build_secs
            ),
        )
        .await;

    // 3) Defense-in-depth: even on exit 0 verify dist/index.html actually
    //    landed before handing off to cargo. The very thing
    //    `tauri::generate_context!` needs.
    verify_frontend_built(wt_root)?;

    Ok(())
}

/// True iff `<wt_root>/node_modules/.bin/ui-bridge-build-ir` is ABSENT — i.e.
/// `pnpm install` still needs to run. Half of the split idempotency gate used
/// by [`prebuild_worktree_frontend`]; proves the pnpm dependency tree was
/// installed (the marker is a workspace bin produced by `pnpm install`).
fn frontend_install_marker_missing(wt_root: &std::path::Path) -> bool {
    let bin = wt_root
        .join("node_modules")
        .join(".bin")
        .join(if cfg!(windows) {
            "ui-bridge-build-ir.cmd"
        } else {
            "ui-bridge-build-ir"
        });
    !bin.exists()
}

/// True iff `<wt_root>/dist/index.html` EXISTS — i.e. a previous frontend build
/// already produced output. The other half of the split idempotency gate. When
/// false, `pnpm run build` must run; the Phase 3 `frontend_only` fast path
/// forces the build even when this is true (a stale dist from before a TS edit
/// must be re-embedded).
fn dist_index_present(wt_root: &std::path::Path) -> bool {
    wt_root.join("dist").join("index.html").exists()
}

/// True iff `<wt_root>` is missing EITHER the `pnpm install` marker OR
/// `dist/index.html` — the original combined idempotency gate, retained for the
/// unit tests that pin its behavior. Equivalent to
/// `frontend_install_marker_missing(wt_root) || !dist_index_present(wt_root)`.
#[cfg(test)]
fn needs_frontend_prebuild(wt_root: &std::path::Path) -> bool {
    frontend_install_marker_missing(wt_root) || !dist_index_present(wt_root)
}

/// Verify the frontend output exists after a successful `npm run build`.
/// Returns `SupervisorError::BuildFailed` mentioning `dist/index.html` on
/// failure so callers see exactly which artifact is missing.
fn verify_frontend_built(wt_root: &std::path::Path) -> Result<(), SupervisorError> {
    let dist_index = wt_root.join("dist").join("index.html");
    let metadata = match std::fs::metadata(&dist_index) {
        Ok(m) => m,
        Err(_) => {
            return Err(SupervisorError::BuildFailed(format!(
                "frontend prebuild produced no {:?} — `tauri::generate_context!` \
                 would panic on the missing artifact when cargo runs",
                dist_index
            )));
        }
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(SupervisorError::BuildFailed(format!(
            "frontend prebuild left an empty/invalid {:?} — \
             `tauri::generate_context!` requires a non-empty dist/index.html",
            dist_index
        )));
    }
    Ok(())
}

/// Run `pnpm <args>` in `cwd` and return the captured `std::process::Output`.
/// On Windows uses `cmd /C pnpm.cmd <args>` (pnpm ships as a `.cmd` shim) +
/// `CREATE_NO_WINDOW` so headless supervisor builds don't flash a console.
/// `args` is a single string passed unchanged to the shell (mirrors the
/// live-tree pnpm invocation style in `run_build_inner`).
///
/// The runner is a pnpm workspace (`packageManager: pnpm@…` + `pnpm-lock.yaml`,
/// CI installs with `pnpm install --frozen-lockfile`). `npm install` produces
/// a flat `node_modules` layout that fails to dedupe the nested
/// `@qontinui/ui-bridge-auto/node_modules/@qontinui/shared-types` against the
/// top-level copy, breaking the frontend `tsc`/`vite` build with a
/// `requiredElements` type mismatch and an unresolved `graphql-ws` import.
/// pnpm's symlinked store reproduces the exact layout CI validates, so the
/// supervisor must use pnpm too.
/// Run `pnpm <args>` through the job-guarded runner so a wedged pnpm/vite/tsc
/// subprocess (or a grandchild holding the stdout/stderr pipe open) is killed
/// at [`pnpm_timeout_secs`] rather than hanging the build forever — the HIGH
/// bug this consolidation closes. The frontend build previously had NO timeout
/// at all.
///
/// On a clean exit returns the captured [`std::process::Output`] (status +
/// stdout + stderr), exactly like the legacy `.output()` call so the 3 call
/// sites' success/failure logic is unchanged. On timeout/cancel returns an
/// `io::Error` of kind `TimedOut` so those call sites surface it through their
/// existing `.map_err(...)` paths.
async fn run_pnpm_command(
    cwd: &std::path::Path,
    args: &str,
    cancel: Option<CancellationToken>,
) -> Result<std::process::Output, std::io::Error> {
    use crate::config::pnpm_timeout_secs;
    use crate::process::guarded_command::{GuardedCommand, GuardedOutcome};

    let timeout = Duration::from_secs(pnpm_timeout_secs());

    #[cfg(windows)]
    let mut guarded = {
        // pnpm ships as a `.cmd` shim; invoke via `cmd /C "pnpm.cmd <args>"`.
        GuardedCommand::new("cmd", timeout)
            .args(["/C", &format!("pnpm.cmd {}", args)])
            .current_dir(cwd)
            // Match the live-tree invocation: vite.config.ts gates the build
            // target on TAURI_PLATFORM=windows.
            .env("TAURI_PLATFORM", "windows")
    };
    #[cfg(not(windows))]
    let mut guarded = {
        // Split the args string so tokens land as separate argv entries.
        let split_args: Vec<&str> = args.split_whitespace().collect();
        GuardedCommand::new("pnpm", timeout)
            .args(&split_args)
            .current_dir(cwd)
    };

    // When a per-slot cancel token is supplied (live-tree + worktree builds),
    // a superseding restart kills the pnpm subprocess tree and frees the global
    // npm lock immediately instead of blocking the new build's pnpm behind a
    // doomed one. git/prewarm callers pass `None`.
    if let Some(token) = cancel {
        guarded = guarded.cancel_token(token);
    }

    match guarded.run().await? {
        GuardedOutcome::Exited(output) => Ok(output),
        GuardedOutcome::TimedOut { after, partial } => {
            let tail = tail_bytes_keep_utf8(
                &String::from_utf8_lossy(&partial.stderr),
                LAST_BUILD_STDERR_SHORT_TAIL_BYTES,
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "pnpm {} timed out after {}s (killed the pnpm subprocess tree). stderr tail: {}",
                    args,
                    after.as_secs(),
                    tail
                ),
            ))
        }
        GuardedOutcome::Cancelled { partial } => {
            let tail = tail_bytes_keep_utf8(
                &String::from_utf8_lossy(&partial.stderr),
                LAST_BUILD_STDERR_SHORT_TAIL_BYTES,
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "pnpm {} cancelled (superseded by a newer build). stderr tail: {}",
                    args, tail
                ),
            ))
        }
    }
}

/// True iff `<npm_dir>/dist/index.html` exists, is a regular file, and is
/// non-empty.
///
/// Used by the frontend-build success arm in `run_build_inner` as a
/// defense-in-depth check after `npm run build` exits 0: an empty or
/// missing `dist/index.html` means the cargo `rust-embed` step is about to
/// embed a broken frontend even though the npm child reported success.
/// The most common causes are a concurrent external `npm run build` that
/// wiped `dist/` between npm-exit and cargo-embed, and historical
/// empty-output vite regressions
/// (`proj_issue_runner_npm_build_safari13_target.md`).
///
/// Pulled into a separate helper so the slot-mutating success-arm logic
/// can be exercised by unit tests without invoking npm.
fn dist_index_ok(npm_dir: &std::path::Path) -> bool {
    let dist_index = npm_dir.join("dist").join("index.html");
    match std::fs::metadata(&dist_index) {
        Ok(m) => m.is_file() && m.len() > 0,
        Err(_) => false,
    }
}

/// Emit a `WARN`-level log line when the qontinui-runner working tree's
/// HEAD does not match `origin/main`. `cargo build` compiles whatever is
/// on disk regardless of branch, so in a multi-agent setup where another
/// session has `git switch`ed the runner tree to a feature branch a
/// caller intending to test main-side code will silently get the feat
/// branch's binary instead. The only existing signal is the `git_sha`
/// field on the spawn-test response, which most callers don't compare.
///
/// Best-effort: any git error (not a repo, no `origin/main` remote ref,
/// git missing from PATH) returns without emitting. The warn is
/// diagnostic, not gate. See [qontinui-supervisor#21] for context.
///
/// `project_dir` is `qontinui-runner/src-tauri`; the git repo root is
/// the parent.
///
/// [qontinui-supervisor#21]: https://github.com/qontinui/qontinui-supervisor/issues/21
async fn warn_if_working_tree_off_main(state: &SharedState, slot_id: usize) {
    let project_dir = &state.config.project_dir;
    let git_dir = match project_dir.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };

    async fn run_git(args: &[&str], cwd: &std::path::Path) -> Option<String> {
        guarded_git(args, cwd).await
    }

    let head = match run_git(&["rev-parse", "HEAD"], &git_dir).await {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let origin_main = match run_git(&["rev-parse", "origin/main"], &git_dir).await {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    if head == origin_main {
        return;
    }

    let branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], &git_dir)
        .await
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_string());

    let head_short: String = head.chars().take(12).collect();
    let main_short: String = origin_main.chars().take(12).collect();

    let msg = format!(
        "Slot {}: working tree HEAD ({}, branch={}) differs from origin/main ({}). \
         This build will compile {}, NOT main. Read `git_sha` from the spawn response \
         to confirm what actually ran. See qontinui-supervisor#21.",
        slot_id, head_short, branch, main_short, head_short
    );
    warn!("{}", msg);
    state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
}

/// Run a best-effort `git <args>` in `cwd` through the job-guarded runner with
/// the [`git_timeout_secs`](crate::config::git_timeout_secs) bound, returning
/// the trimmed stdout on a clean zero-exit and `None` on ANY failure (git
/// missing, non-zero exit, timeout, cancel). Callers already treat git as
/// advisory, so a wedged git silently yields `None` instead of hanging the
/// build.
async fn guarded_git(args: &[&str], cwd: &std::path::Path) -> Option<String> {
    use crate::config::git_timeout_secs;
    let outcome = GuardedCommand::new("git", Duration::from_secs(git_timeout_secs()))
        .args(args)
        .current_dir(cwd)
        .run()
        .await
        .ok()?;
    match outcome {
        GuardedOutcome::Exited(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        _ => None,
    }
}

/// Resolve the qontinui-runner repo HEAD SHA. Returns `None` on any error
/// (git missing, not a repo, detached HEAD with no SHA, timeout, etc.).
/// Best-effort.
async fn rev_parse_head(git_dir: &std::path::Path) -> Option<String> {
    let sha = guarded_git(&["rev-parse", "HEAD"], git_dir).await?;
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Stamp the slot's freshly-built runner exe with the HEAD SHA in a sidecar
/// file (`<slot>/debug/qontinui-runner.exe.git_sha`). Best-effort — every
/// failure path is logged at debug and ignored. The build still counts as
/// succeeded; the sidecar is observability for cross-slot drift detection.
///
/// Sidecar format: plain UTF-8, single line, exactly the SHA, no trailing
/// newline. Read back by [`crate::process::manager::read_slot_sha`].
async fn write_slot_sha_sidecar(state: &SharedState, slot: &Arc<BuildSlot>) {
    use tracing::debug;
    let project_dir = &state.config.project_dir;
    let git_dir = match project_dir.parent() {
        Some(p) => p.to_path_buf(),
        None => {
            debug!(
                "Slot {} sha sidecar: project_dir has no parent ({:?}); skipping",
                slot.id, project_dir
            );
            return;
        }
    };
    let sha = match rev_parse_head(&git_dir).await {
        Some(s) => s,
        None => {
            debug!(
                "Slot {} sha sidecar: git rev-parse HEAD failed or returned empty in {:?}; skipping",
                slot.id, git_dir
            );
            return;
        }
    };
    let exe_path = state.config.runner_exe_path_for_slot(slot.id);
    let sidecar = match exe_path.parent() {
        Some(dir) => dir.join(crate::process::manager::SLOT_SHA_SIDECAR_FILENAME),
        None => {
            debug!(
                "Slot {} sha sidecar: exe path {:?} has no parent dir; skipping",
                slot.id, exe_path
            );
            return;
        }
    };
    if let Err(e) = std::fs::write(&sidecar, sha.as_bytes()) {
        debug!(
            "Slot {} sha sidecar: write failed for {:?}: {}",
            slot.id, sidecar, e
        );
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
#[cfg(target_os = "windows")]
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
    #[cfg(target_os = "windows")]
    {
        pid_exe_path(pid).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
        None
    }
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
                    warn!("Slot {}: next_entry under {:?} failed: {}", slot.id, dir, e);
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
                    warn!("Slot {}: metadata for {:?} failed: {}", slot.id, path, e);
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

    // Route the prewarm `cargo check` through the job-guarded runner too, so a
    // wedged check (rustc grandchild holding the pipe) is killed at the prewarm
    // timeout rather than hanging the slot. Stream stderr lines to the slot's
    // SSE sender (same as a real build) and the supervisor log.
    let outcome = GuardedCommand::new("cargo", Duration::from_secs(PREWARM_TIMEOUT_SECS))
        .args(&args)
        .current_dir(&state.config.project_dir)
        .env("CARGO_TARGET_DIR", &slot.target_dir)
        .stream_lines(slot.log_stream.clone())
        .run()
        .await
        .map_err(|e| {
            SupervisorError::Process(format!("Failed to spawn prewarm cargo check: {}", e))
        })?;

    match outcome {
        GuardedOutcome::Exited(output) if output.status.success() => {
            // Mirror captured stderr into the supervisor log for parity with
            // the old per-line emission.
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Info,
                        format!("[prewarm slot {}] {}", slot.id, line),
                    )
                    .await;
            }
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
        GuardedOutcome::Exited(output) => {
            warn!(
                "Prewarm cargo check for slot {} exited with {}",
                slot.id, output.status
            );
            Err(SupervisorError::BuildFailed(format!(
                "Prewarm exited with {}",
                output.status
            )))
        }
        GuardedOutcome::TimedOut { after, .. } => {
            warn!(
                "Prewarm of slot {} timed out after {}s, killed the check tree",
                slot.id,
                after.as_secs()
            );
            Err(SupervisorError::Timeout(format!(
                "Prewarm timed out after {}s",
                after.as_secs()
            )))
        }
        GuardedOutcome::Cancelled { .. } => Err(SupervisorError::Cancelled(format!(
            "Prewarm of slot {} cancelled",
            slot.id
        ))),
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

#[cfg(test)]
mod tests {
    //! Regression tests for the post-`npm exit 0` defense-in-depth `dist/`
    //! sanity gate. See `supervisor-frontend-build-silent-success.md` for
    //! the bug these guard against.
    use super::{dist_index_ok, needs_frontend_prebuild, verify_frontend_built};
    use std::fs;
    use tempfile::TempDir;

    /// Filename of the pnpm bin stub. `.cmd` on Windows (where pnpm installs
    /// `.bin/<tool>.cmd` shims), bare elsewhere. Mirrors the platform check
    /// inside [`needs_frontend_prebuild`].
    fn ui_bridge_build_ir_bin() -> &'static str {
        if cfg!(windows) {
            "ui-bridge-build-ir.cmd"
        } else {
            "ui-bridge-build-ir"
        }
    }

    #[test]
    fn needs_frontend_prebuild_true_when_node_modules_and_dist_absent() {
        // Simulates a fresh `git worktree add --detach` — nothing in the
        // workspace, no prior frontend build. Must trigger the prebuild.
        let tmp = TempDir::new().expect("tempdir");
        assert!(
            needs_frontend_prebuild(tmp.path()),
            "fresh worktree (no node_modules + no dist/) must require prebuild"
        );
    }

    #[test]
    fn needs_frontend_prebuild_true_when_only_node_modules_present() {
        // Half-installed state — pnpm install succeeded but the previous
        // `pnpm run build` never ran or failed. We should NOT skip the
        // prebuild because dist/index.html is what cargo embeds.
        let tmp = TempDir::new().expect("tempdir");
        let bin_dir = tmp.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");
        fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
        assert!(
            needs_frontend_prebuild(tmp.path()),
            "node_modules present but no dist/index.html must still require prebuild"
        );
    }

    #[test]
    fn needs_frontend_prebuild_true_when_only_dist_present() {
        // Inverse half-installed state — somehow dist/ exists but
        // node_modules is gone (e.g. someone ran `rm -rf node_modules`
        // between sessions). Must re-prebuild because `pnpm run build`
        // can't run without the dep tree.
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(dist.join("index.html"), b"<!doctype html>").expect("write index");
        assert!(
            needs_frontend_prebuild(tmp.path()),
            "dist/ present but no node_modules must still require prebuild"
        );
    }

    #[test]
    fn needs_frontend_prebuild_false_when_both_present() {
        // Idempotency gate — both signals say a prior prebuild succeeded
        // and we should reuse it. This is the path that saves ~30s per
        // repeated spawn-test on the same ref.
        let tmp = TempDir::new().expect("tempdir");
        let bin_dir = tmp.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");
        fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(dist.join("index.html"), b"<!doctype html>").expect("write index");
        assert!(
            !needs_frontend_prebuild(tmp.path()),
            "fully populated worktree must skip prebuild (idempotent reuse)"
        );
    }

    #[test]
    fn verify_frontend_built_err_when_index_missing() {
        // Simulates the empirical 2026-05-21 failure mode: npm exit 0 but
        // dist/index.html still missing. Must surface a clear error
        // mentioning the missing artifact so the user can correlate it
        // with the eventual `tauri::generate_context!` panic.
        let tmp = TempDir::new().expect("tempdir");
        let res = verify_frontend_built(tmp.path());
        let err = res.expect_err("missing dist/index.html must error");
        let s = err.to_string();
        assert!(
            s.contains("dist") && s.contains("index.html"),
            "error must name the missing artifact (dist/index.html); got: {}",
            s
        );
    }

    #[test]
    fn verify_frontend_built_err_when_index_empty() {
        // Pathological case carried over from the legacy safari13
        // regression: vite exits 0 having written zero bytes. Cargo would
        // embed an empty index.html and the runner would render a blank
        // page. Surface as an error too.
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(dist.join("index.html"), b"").expect("write empty index");
        let res = verify_frontend_built(tmp.path());
        let err = res.expect_err("empty dist/index.html must error");
        let s = err.to_string();
        assert!(
            s.contains("dist") && s.contains("index.html"),
            "error must name the empty artifact (dist/index.html); got: {}",
            s
        );
    }

    #[test]
    fn verify_frontend_built_ok_when_index_present_and_nonempty() {
        // Happy path — a real npm build wrote a non-empty index.html.
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(
            dist.join("index.html"),
            b"<!doctype html><html><body>ok</body></html>",
        )
        .expect("write index");
        verify_frontend_built(tmp.path()).expect("non-empty dist/index.html must verify clean");
    }

    #[test]
    fn dist_index_ok_returns_false_when_dist_dir_missing() {
        // Simulates the multi-agent scenario where a concurrent external
        // `npm run build` wiped the entire dist/ directory between this
        // supervisor's npm exit and cargo's embed step.
        let tmp = TempDir::new().expect("tempdir");
        assert!(
            !dist_index_ok(tmp.path()),
            "missing dist/ must be reported as not-ok so the slot is flagged stale"
        );
    }

    #[test]
    fn dist_index_ok_returns_false_when_index_html_missing() {
        // Simulates an empty-output regression: dist/ exists (an earlier
        // build created it) but index.html specifically is gone.
        let tmp = TempDir::new().expect("tempdir");
        fs::create_dir_all(tmp.path().join("dist")).expect("mkdir dist");
        assert!(
            !dist_index_ok(tmp.path()),
            "dist/ without index.html must be reported as not-ok"
        );
    }

    #[test]
    fn dist_index_ok_returns_false_when_index_html_is_empty() {
        // Simulates the historical safari13 regression where vite exited 0
        // having written zero bytes (proj_issue_runner_npm_build_safari13_target.md).
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(dist.join("index.html"), b"").expect("write empty index");
        assert!(
            !dist_index_ok(tmp.path()),
            "empty dist/index.html must be reported as not-ok"
        );
    }

    #[test]
    fn dist_index_ok_returns_true_when_index_html_present_and_nonempty() {
        // Happy path — a real build wrote a non-empty index.html.
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).expect("mkdir dist");
        fs::write(
            dist.join("index.html"),
            b"<!doctype html><html><body>ok</body></html>",
        )
        .expect("write index");
        assert!(
            dist_index_ok(tmp.path()),
            "non-empty dist/index.html is the only signal of a healthy build"
        );
    }

    #[test]
    fn dist_index_ok_returns_false_when_index_html_is_a_directory() {
        // Pathological case: someone created dist/index.html as a
        // directory (mkdir -p dist/index.html). The metadata.is_file()
        // guard catches this — without it, len() would return junk.
        let tmp = TempDir::new().expect("tempdir");
        fs::create_dir_all(tmp.path().join("dist").join("index.html")).expect("mkdir");
        assert!(
            !dist_index_ok(tmp.path()),
            "dist/index.html as a directory must be reported as not-ok"
        );
    }
}

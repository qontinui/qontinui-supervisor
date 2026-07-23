use regex::Regex;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::config::build_timeout_secs;
use crate::diagnostics::DiagnosticEventKind;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::guarded_command::{GuardedCommand, GuardedOutcome};
use crate::process::manager::{BuildProvenance, BuildSource};
#[cfg(target_os = "windows")]
use crate::process::windows::{
    cleanup_orphaned_slot_processes, find_pids_holding_exe, kill_by_pid, pid_exe_path,
};
use crate::state::{BuildInfo, BuildSlot, LkgInfo, SharedState};
use std::sync::Arc;

/// Pure threshold check for the pre-permit disk guard (plan
/// `2026-06-05-supervisor-build-artifact-footprint`, Phase 2).
///
/// Returns `true` when a build is allowed to proceed, `false` when free disk
/// is below the required minimum. Split out as a pure function so the policy is
/// unit-testable without touching the filesystem or any global state.
///
/// - `min_free_gb == 0` ⇒ guard disabled, always allow.
/// - `disk_free_bytes == None` ⇒ we could not read the disk; FAIL OPEN (allow)
///   rather than wedge every build on a probe failure. The motivating incident
///   was a disk that was demonstrably near-full; a probe that returns *nothing*
///   is a different (rare) failure and blocking all builds on it is worse than
///   the status quo.
pub fn disk_guard_allows(disk_free_bytes: Option<u64>, min_free_gb: u64) -> bool {
    if min_free_gb == 0 {
        return true;
    }
    match disk_free_bytes {
        None => true,
        Some(free) => {
            let required = min_free_gb.saturating_mul(1024 * 1024 * 1024);
            free >= required
        }
    }
}

/// The phase a `spawn-test` build future is in, tracked via a shared
/// `Arc<AtomicU8>` so the route-level `tokio::time::timeout` wrapper can read
/// the LIVE phase at the instant it fires and compose a phase-accurate timeout
/// message. The single timeout wraps the WHOLE
/// `run_cargo_build_with_dir_detailed` future, and the future is cancelled
/// mid-flight on timeout (so no `BuildAttempt` is returned to the route) — the
/// only way the route can tell which phase actually blocked is to observe this
/// marker, which the build future advances monotonically as it progresses.
///
/// Ordering (see `run_cargo_build_with_dir_detailed` / `run_build_inner`):
/// `AwaitingSlot` (cargo permit acquire) → `AwaitingNpmLock` (frontend lock
/// acquire) → `BuildingFrontend` (`pnpm run build`) → `Compiling` (cargo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BuildPhase {
    /// Waiting on `permits.acquire_owned()` — the genuine "build slot" wait.
    AwaitingSlot = 0,
    /// Slot held; waiting on `npm_lock.lock_owned()` (frontend serialization).
    AwaitingNpmLock = 1,
    /// npm_lock held; running `pnpm run build`.
    BuildingFrontend = 2,
    /// Frontend done (or skipped); running `cargo build`.
    Compiling = 3,
}

impl BuildPhase {
    /// Reconstruct a phase from the atomic marker. Any out-of-range value is
    /// treated as `AwaitingSlot` (the conservative default at attempt start).
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => BuildPhase::AwaitingNpmLock,
            2 => BuildPhase::BuildingFrontend,
            3 => BuildPhase::Compiling,
            _ => BuildPhase::AwaitingSlot,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Store this phase into the shared marker (Relaxed — the marker is a
    /// single-writer observability hint, not a synchronization point).
    pub fn store(self, marker: &std::sync::atomic::AtomicU8) {
        marker.store(self.as_u8(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Compose a phase-accurate `spawn-test` queue-timeout message. `secs` is
    /// the configured `queue_timeout_secs`; `available_permits` is the live
    /// cargo-permit count at the instant of timeout (so the operator can see
    /// that permits were free while the frontend lock starved the build).
    pub fn timeout_message(self, secs: u64, available_permits: usize) -> String {
        match self {
            BuildPhase::AwaitingSlot => {
                format!(
                    "Build queue timeout: waited {}s for a cargo build slot",
                    secs
                )
            }
            BuildPhase::AwaitingNpmLock => format!(
                "Build queue timeout: waited {}s, blocked on the frontend (pnpm) lock \
                 with {} cargo permits free",
                secs, available_permits
            ),
            BuildPhase::BuildingFrontend => format!(
                "Build queue timeout: waited {}s, still running the frontend (pnpm) build \
                 with {} cargo permits free",
                secs, available_permits
            ),
            BuildPhase::Compiling => format!(
                "Build queue timeout: waited {}s while compiling (cargo); the slot was \
                 already held — not a slot wait",
                secs
            ),
        }
    }
}

/// Pre-permit disk guard. Called BEFORE acquiring a build-pool permit/slot at
/// every build-spawning site so a doomed build never consumes a slot. When
/// free disk is below `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB`, returns
/// `Err(SupervisorError::InsufficientDisk { .. })` whose body embeds the cached
/// footprint snapshot and names both prune endpoints. `Ok(())` when the build
/// may proceed.
///
/// Uses the CACHED footprint snapshot's `disk_free_bytes` if it is fresh enough
/// to be useful; otherwise probes the disk directly (cheap — a single sysinfo
/// `Disks` enumeration, not a tree walk). The embedded snapshot is whatever is
/// cached (may be `None` if no refresh has run yet — the caller still gets the
/// numeric free/required bytes and the prune-endpoint hints).
pub async fn check_disk_guard(state: &SharedState) -> Result<(), SupervisorError> {
    let min_free_gb = crate::config::min_free_disk_gb();
    if min_free_gb == 0 {
        return Ok(());
    }

    // Probe disk free directly (fast) for the pool root so the decision is on
    // current reality, not a possibly-stale cached number.
    let pool_root = state.config.runner_npm_dir().join("target-pool");
    let probe = if pool_root.exists() {
        pool_root
    } else {
        state.config.runner_npm_dir()
    };
    let free = crate::footprint::disk_free_bytes_for(&probe);

    if disk_guard_allows(free, min_free_gb) {
        return Ok(());
    }

    let required_bytes = min_free_gb.saturating_mul(1024 * 1024 * 1024);
    let free_bytes = free.unwrap_or(0);
    let footprint = state
        .footprint
        .read()
        .await
        .as_ref()
        .and_then(|s| serde_json::to_value(s).ok());

    let msg = format!(
        "Pre-permit disk guard: refusing build — {} GB free, need at least {} GB \
         (QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB). Reclaim space via \
         DELETE /spawn-worktrees or POST /builds/slots/{{id}}/clean.",
        free_bytes / (1024 * 1024 * 1024),
        min_free_gb,
    );
    warn!("{}", msg);
    state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;

    Err(SupervisorError::InsufficientDisk {
        free_bytes,
        required_bytes,
        footprint: Box::new(footprint),
    })
}

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

/// Explicit caller intent for how a build's tree should be classified into a
/// [`BuildSource`] — the signal the build path previously could not derive.
///
/// Before Phase B, provenance was inferred purely from
/// `build_dir_override.is_some()` ([`provenance_tree_root`]): `None` ⇒
/// `LiveTree`, `Some(_)` ⇒ `Override`. That conflated TWO different
/// `Some(src_tauri)` builds — a default primary rebuild from a
/// supervisor-materialized `origin/main` worktree, and a spawn-test
/// `git_ref` / `worktree_path` preview of a foreign tree — both of which
/// arrive as `Some(src_tauri)`. They must classify differently
/// (`OriginMain` is LKG-eligible and startable as primary; `Override` is
/// neither), so the caller now passes this explicit kind rather than letting
/// the build infer it from the path.
///
/// The kind is the SINGLE source of the resulting [`BuildSource`] AND, for
/// `OriginMain`, the SHA written into provenance (`resolved_sha` from
/// `prepare_worktree`, NOT a re-probe of the override dir).
#[derive(Debug, Clone)]
pub enum BuildSourceKind {
    /// The live runner working tree (`state.config.project_dir`,
    /// `build_dir_override == None`). Legacy default. SHA probed from the live
    /// tree root.
    LiveTree,
    /// A supervisor-materialized `origin/main` worktree (the default primary
    /// rebuild path). `build_dir_override` is the worktree's `src-tauri`; the
    /// SHA is `prepare_worktree`'s `resolved_sha` (the exact origin/main commit
    /// that was checked out), carried here so provenance records merged truth
    /// rather than re-probing.
    OriginMain { resolved_sha: String },
    /// A foreign override tree the supervisor does not vouch for (spawn-test
    /// `git_ref` / `worktree_path`). `build_dir_override` is its `src-tauri`;
    /// SHA probed from that tree root. Excluded from LKG + non-temp start.
    Override,
}

impl BuildSourceKind {
    fn build_source(&self) -> BuildSource {
        match self {
            BuildSourceKind::LiveTree => BuildSource::LiveTree,
            BuildSourceKind::OriginMain { .. } => BuildSource::OriginMain,
            BuildSourceKind::Override => BuildSource::Override,
        }
    }
}

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
    run_cargo_build_with_dir(state, requester_id, None, false, BuildSourceKind::LiveTree).await
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
///
/// `source_kind` (Phase B): the EXPLICIT provenance classification for the
/// build, supplied by the caller rather than inferred from
/// `build_dir_override`. It must be consistent with `build_dir_override`:
/// `LiveTree` ⇔ `None`; `OriginMain { resolved_sha }` / `Override` ⇔
/// `Some(src_tauri)`. The kind alone decides the recorded [`BuildSource`] and
/// (for `OriginMain`) the recorded SHA — disambiguating a primary
/// origin/main build from a spawn-test foreign override, which were
/// indistinguishable by path alone.
pub async fn run_cargo_build_with_dir(
    state: &SharedState,
    requester_id: Option<String>,
    build_dir_override: Option<PathBuf>,
    force_frontend_build: bool,
    source_kind: BuildSourceKind,
) -> Result<(), SupervisorError> {
    // Thin wrapper: discard the per-attempt detail (slot id / full stderr) that
    // only the spawn-test self-heal path consumes, preserving the legacy
    // `Result<(), _>` contract for every other caller.
    // Callers that don't observe build phase still need a marker to thread
    // through; a throwaway atomic satisfies the signature with zero behavior
    // change.
    let phase = Arc::new(std::sync::atomic::AtomicU8::new(
        BuildPhase::AwaitingSlot.as_u8(),
    ));
    run_cargo_build_with_dir_detailed(
        state,
        requester_id,
        build_dir_override,
        force_frontend_build,
        source_kind,
        phase,
    )
    .await
    .map(|_attempt| ())
    .map_err(|(e, _attempt)| e)
}

/// Same as [`run_cargo_build_with_dir`] but additionally returns a
/// [`BuildAttempt`] on BOTH the success and failure paths — the slot id the
/// build ran on, plus the FULL cargo stderr on failure. The spawn-test handler
/// uses this to (a) feed the real compiler error into the build submission's
/// `stderr_tail` (Issue 3 fix part 1) and (b) clean + retry exactly that slot
/// when the failure is environmental, not a compiler diagnostic (fix part 2).
///
/// On the error path the `BuildAttempt` is returned alongside the
/// `SupervisorError` so the caller has the slot id + full stderr without
/// re-deriving them.
pub async fn run_cargo_build_with_dir_detailed(
    state: &SharedState,
    requester_id: Option<String>,
    build_dir_override: Option<PathBuf>,
    force_frontend_build: bool,
    source_kind: BuildSourceKind,
    // Shared phase marker advanced as the build progresses so a route-level
    // `tokio::time::timeout` wrapper can read the LIVE phase on the timeout
    // branch (the cancelled future returns no `BuildAttempt`). See [`BuildPhase`].
    phase: Arc<std::sync::atomic::AtomicU8>,
) -> Result<BuildAttempt, (SupervisorError, BuildAttempt)> {
    // Pre-permit disk guard (Phase 2): refuse a doomed build BEFORE consuming a
    // permit/slot when free disk is below the configured floor. The refusal
    // embeds the cached footprint + prune-endpoint hints so the caller can act.
    if let Err(e) = check_disk_guard(state).await {
        return Err((e, BuildAttempt::default()));
    }

    // Acquire a permit from the build pool. Blocks until a slot is free.
    // Queue depth counter lets `GET /builds` report how many callers are waiting.
    BuildPhase::AwaitingSlot.store(&phase);
    state
        .build_pool
        .queue_depth
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let permit_result = state.build_pool.permits.clone().acquire_owned().await;
    state
        .build_pool
        .queue_depth
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let _permit = match permit_result {
        Ok(p) => p,
        Err(_) => {
            return Err((
                SupervisorError::Other("Build pool semaphore closed".to_string()),
                BuildAttempt::default(),
            ));
        }
    };

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

    // Reap stale cargo/rustc left building into THIS slot by a previous build
    // (e.g. after a supervisor crash), so the slot's target/exe is not locked.
    // Slot-scoped on purpose: a machine-wide kill would take out peer agents'
    // worktree builds and sibling pool slots mid-compile.
    #[cfg(target_os = "windows")]
    cleanup_orphaned_slot_processes(&slot.target_dir).await;

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
                &phase,
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
        &phase,
    )
    .await;
    let duration_secs = build_start.elapsed().as_secs_f64();

    // Slot id this attempt ran on — surfaced in the BuildAttempt so the caller
    // can clean + retry exactly this slot when the failure is environmental
    // (Issue 3 fix part 2).
    let attempt_slot_id = slot.id;

    // On failure, read the FULL persisted cargo stderr from the slot's
    // `last-build.stderr` sidecar (written by `run_build_inner`) so the caller
    // can surface a generous tail through `GET /build/{id}/status` instead of
    // the 2 KB inline `last_error` cap (Issue 3 fix part 1). Best-effort: a
    // missing/unreadable sidecar yields `None` and the caller falls back to the
    // error string. Read BEFORE the slot is released so a concurrent build on
    // the same slot can't overwrite the sidecar mid-read.
    let attempt_full_stderr: Option<String> = if result.is_err() {
        let stderr_path = slot.target_dir.join("last-build.stderr");
        match tokio::fs::read_to_string(&stderr_path).await {
            Ok(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    } else {
        None
    };

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
        info!(
            "GCMD: build succeeded, promoting slot {} to last_successful_slot + computing provenance/LKG",
            slot.id
        );
        let mut last = state.build_pool.last_successful_slot.write().await;
        *last = Some(slot.id);
        drop(last);

        // Compute the provenance of THIS build ONCE — the SHA of the tree that
        // was actually compiled (the override worktree root when
        // `build_dir_override` is set, else the live tree), classified by the
        // caller's explicit `source_kind` (LiveTree / OriginMain / Override),
        // the absolute dir built, and the build time. This is the root fix for
        // the 2026-06-05 incident: the legacy sidecar always probed the live
        // tree's HEAD and so recorded the wrong SHA for an override build; and
        // (Phase B) the explicit kind disambiguates a primary origin/main build
        // from a spawn-test foreign override, which share a `Some(src_tauri)`.
        // The value is in scope for the sidecar write below AND for the
        // `update_lkg_after_success` call (Phase 2's LKG gate consumes it).
        let provenance =
            compute_build_provenance(state, build_dir_override.as_deref(), &source_kind).await;

        // Stamp the slot's exe with this provenance so resolve_source_exe and
        // /builds can detect drift across slots (a fresh exe staged into one
        // slot while a stale or foreign exe lingers in another). Best-effort:
        // a write failure is logged but the build still succeeded.
        write_slot_provenance_sidecar(state, &slot, &provenance).await;

        // Capture this exe as the new last-known-good — UNLESS this was an
        // override build (a spawn-test git_ref / worktree_path preview of a
        // foreign tree). Promoting an override build to LKG is the exact
        // 2026-06-05 incident: a branch exe became LKG and a restart deployed
        // it to the primary. The gate keys on `provenance.source`, not on a
        // sha-vs-HEAD comparison, and `update_lkg_after_success` consumes the
        // SAME provenance value computed above (no re-probe). LKG survives
        // subsequent failed builds that overwrite or delete the slot's exe;
        // agents testing changes can fall back to it via spawn-test
        // {use_lkg: true}. Failures here are logged but do not fail the build
        // — LKG is a safety net, not a correctness gate.
        if let Err(e) = update_lkg_after_success(state, &slot, &provenance).await {
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

    let attempt = BuildAttempt {
        slot_id: Some(attempt_slot_id),
        full_stderr: attempt_full_stderr,
    };
    match result {
        Ok(()) => Ok(attempt),
        Err(e) => Err((e, attempt)),
    }
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
    build_dir_override: Option<&std::path::Path>,
    force_frontend_build: bool,
    // Phase marker advanced through the npm-lock wait, frontend build, and
    // cargo compile so a route-level timeout can attribute the blocked phase.
    phase: &std::sync::atomic::AtomicU8,
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

        // Entering the frontend (pnpm) lock wait. Bracket the acquire with the
        // `npm_lock_waiters` counter EXACTLY as `queue_depth` brackets the
        // permit acquire above, so `GET /builds` can attribute a starving
        // spawn-test to npm-lock contention while cargo permits are free.
        BuildPhase::AwaitingNpmLock.store(phase);
        state
            .build_pool
            .npm_lock_waiters
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let npm_wait_start = std::time::Instant::now();
        let _npm_guard = state.build_pool.npm_lock.clone().lock_owned().await;
        state
            .build_pool
            .npm_lock_waiters
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        // (c) Highest-signal starvation diagnostic: we waited a long time on the
        // frontend lock while cargo slots were free — the exact mis-attributed
        // wait this surfacing targets. `available_permits() > 0` proves it was
        // NOT slot exhaustion.
        let npm_wait_secs = npm_wait_start.elapsed().as_secs();
        let free_permits = state.build_pool.permits.available_permits();
        if npm_wait_secs > 60 && free_permits > 0 {
            warn!(
                slot_id = slot.id,
                npm_wait_secs,
                free_cargo_permits = free_permits,
                "spawn-test waited >60s on the frontend (pnpm) lock while cargo build \
                 permits were free — frontend serialization is starving the build, not slot \
                 exhaustion (check for a concurrent `pnpm run build`)"
            );
        }

        BuildPhase::BuildingFrontend.store(phase);
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!("Slot {}: building frontend (pnpm run build)...", slot.id),
            )
            .await;
        info!("Slot {}: building frontend in {:?}", slot.id, npm_dir);

        // LIVE-TREE ONLY: sync node_modules to the lockfile BEFORE building,
        // exactly as CI does (`pnpm install --frozen-lockfile`). The live tree
        // reuses whatever node_modules already exists on disk and never
        // reinstalls, so a STALE dep (e.g. a `pnpm install` that aborted on a
        // no-TTY node_modules purge, leaving `@qontinui/ui-bridge@0.21.1` where
        // package.json pins `^0.22.0`) silently breaks the `tsc`/`vite` build
        // with a phantom `error TS2339`. CI never hits this because it installs
        // into a clean tree. A frozen install here makes the supervisor's
        // live-tree build resolve dependencies the same way CI does. The
        // worktree path (`build_dir_override.is_some()`) already installs via
        // `prebuild_worktree_frontend`, so this runs only for the live tree.
        // Kept inside the already-held `npm_lock` so install + build are one
        // serialized unit. A `--frozen-lockfile` failure is a REAL error
        // (lockfile/package.json drift, or a genuinely broken install) — match
        // CI and fail the build loudly rather than proceeding to build stale;
        // do NOT fall back to a non-frozen install (that reintroduces
        // silent-stale).
        if build_dir_override.is_none() {
            state
                .logs
                .emit(
                    LogSource::Build,
                    LogLevel::Info,
                    format!(
                        "Slot {}: syncing frontend deps (pnpm install --frozen-lockfile)...",
                        slot.id
                    ),
                )
                .await;
            let install_result = run_pnpm_command(&npm_dir, "install --frozen-lockfile").await;
            match install_result {
                Ok(output) if output.status.success() => {
                    info!(
                        "Slot {}: frontend deps synced (pnpm install --frozen-lockfile)",
                        slot.id
                    );
                }
                Ok(output) => {
                    let merged = merge_process_output(&output);
                    let tail = record_frontend_failure(state, slot, &merged).await;
                    let msg = format!(
                        "Slot {}: pnpm install --frozen-lockfile failed (exit {}) in {:?} — \
                         likely lockfile/package.json drift or a broken install. Refusing to \
                         build against a stale node_modules:\n{}",
                        slot.id, output.status, npm_dir, tail
                    );
                    error!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Build, LogLevel::Error, &msg)
                        .await;
                    return Err(SupervisorError::BuildFailed(msg));
                }
                Err(e) => {
                    let msg = format!(
                        "Slot {}: pnpm install --frozen-lockfile failed to spawn in {:?}: {}",
                        slot.id, npm_dir, e
                    );
                    error!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Build, LogLevel::Error, &msg)
                        .await;
                    return Err(SupervisorError::BuildFailed(msg));
                }
            }
        }

        let npm_result = run_pnpm_command(&npm_dir, "run build").await;

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
                // S2: `tsc`/`vite` write their diagnostics to **stdout**, so the
                // legacy stderr-only capture printed "frontend build FAILED"
                // with no `error TS####` anywhere in it. Merge both streams and
                // keep the TAIL (where the error summary lives), not the head.
                let merged = merge_process_output(&output);
                let truncated = tail_bytes_keep_utf8(&merged, LAST_BUILD_STDERR_SHORT_TAIL_BYTES);
                error!(
                    "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. output:\n{}",
                    slot.id, truncated
                );
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Error,
                        format!(
                            "Slot {}: frontend build FAILED \u{2014} cargo will proceed with the previous dist/ snapshot, so this binary may embed a stale frontend. Fix tsc errors and rebuild to refresh. output:\n{}",
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
                        "frontend_stale: pnpm run build failed:\n{}",
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

    // Frontend phase done (or skipped); the remainder is the cargo compile.
    BuildPhase::Compiling.store(phase);

    // Diagnostic-only: emit a WARN if the runner working tree isn't on
    // origin/main. Multi-agent flow can leave the tree on a feature branch
    // between sessions, and cargo silently compiles whatever's there. The
    // warn surfaces the mismatch in supervisor.log so a caller intending to
    // test main-side code has a chance to spot it before reading `git_sha`
    // on the spawn response. See qontinui-supervisor#21.
    warn_if_working_tree_off_main(state, slot.id).await;

    info!(
        "GCMD: frontend step returned, starting cargo (slot={})",
        slot.id
    );

    // Always pass --features custom-protocol so Tauri embeds the frontend from
    // dist/. Without it, `cfg(dev) = !custom_protocol` makes the binary load
    // from devUrl (localhost:1420), which isn't running.
    //
    // NOTE: after this build succeeds, `build_shim_sidecar` runs a second,
    // fail-open `cargo build --bin qontinui-shim` on the same warm target dir
    // so the install-interception stub is produced in lockstep with the runner
    // exe (see [`SHIM_EXE_FILENAME`] for the placement contract).
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

    // Run cargo through GuardedCommand: it spawns the child, assigns it to a
    // kill-on-close JobObject (Windows) so the wall-clock timeout reliably
    // tears down the WHOLE build tree (cargo → rustc → linker grandchildren),
    // and bounds the post-exit pipe drain so a pipe-holding grandchild can't
    // silently wedge the build. We attach a per-build broadcast channel via
    // `stream_lines` and process each stderr line live (error classification
    // + `state.logs.emit` + fanout to the slot's SSE sender + collection),
    // preserving the exact live-logging behavior of the legacy reader task.
    let timeout_secs = build_timeout_secs();
    info!(
        "GCMD: cargo start slot={} cwd={:?} target={:?} timeout={}s",
        slot.id, cargo_cwd, slot.target_dir, timeout_secs
    );

    // Per-build line bus. `stream_lines` forwards cargo's merged stderr lines
    // here as they're read; the consumer task below mirrors the legacy
    // classification + emit + SSE-fanout + collection.
    let (line_tx, mut line_rx) = tokio::sync::broadcast::channel::<String>(4096);
    let consumer = {
        let state_clone = state.clone();
        let log_stream = slot.log_stream.clone();
        tokio::spawn(async move {
            let mut error_lines = Vec::new();
            let mut all_lines = Vec::new();
            loop {
                match line_rx.recv().await {
                    Ok(line) => {
                        let is_error = BUILD_ERROR_PATTERNS.iter().any(|p| p.is_match(&line));
                        let level = if is_error {
                            LogLevel::Error
                        } else {
                            LogLevel::Info
                        };
                        state_clone.logs.emit(LogSource::Build, level, &line).await;
                        // Fanout to per-slot SSE subscribers. Err == no
                        // subscribers (common case) — drop silently.
                        let _ = log_stream.send(line.clone());
                        all_lines.push(line.clone());
                        if is_error {
                            error_lines.push(line);
                        }
                    }
                    // Sender dropped (run finished) → done. `Lagged` means we
                    // fell behind the bounded channel; skip the dropped frames
                    // and keep going so we still collect the tail.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("GCMD: cargo line consumer lagged, dropped {} lines", n);
                    }
                }
            }
            (error_lines, all_lines)
        })
    };

    let guarded = GuardedCommand::new("cargo", Duration::from_secs(timeout_secs))
        .args(CARGO_BUILD_ARGS)
        .current_dir(cargo_cwd)
        // Redirect cargo output to this slot's isolated target dir so
        // concurrent builds on other slots don't contend on the same target/.
        .env("CARGO_TARGET_DIR", &slot.target_dir)
        .job_guarded(true)
        .stream_lines(line_tx);

    let outcome = guarded.run().await;
    // Drop the GuardedOutcome's grip is implicit; the sender (line_tx) was
    // moved into the command and is dropped when `run` returns, closing the
    // consumer's channel so it terminates.
    let (status, captured_stderr_bytes): (std::process::ExitStatus, Vec<u8>) = match outcome {
        Ok(GuardedOutcome::Exited(output)) => {
            info!(
                "GCMD: cargo step returned status={} slot={}",
                output.status, slot.id
            );
            (output.status, output.stderr)
        }
        Ok(GuardedOutcome::TimedOut { after, partial }) => {
            warn!(
                "GCMD: cargo TimedOut after {}s, killing — slot={}",
                after.as_secs(),
                slot.id
            );
            // Make sure the consumer terminates even though we early-return.
            let _ = consumer.await;
            let _ = partial; // partial stderr already streamed live to logs
            return Err(SupervisorError::Timeout(format!(
                "Build timed out after {}s",
                after.as_secs()
            )));
        }
        Ok(GuardedOutcome::Cancelled { .. }) => {
            warn!("GCMD: cargo Cancelled — slot={}", slot.id);
            let _ = consumer.await;
            return Err(SupervisorError::Process("Build cancelled".to_string()));
        }
        Err(e) => {
            warn!("GCMD: cargo run() returned err={} slot={}", e, slot.id);
            let _ = consumer.await;
            return Err(SupervisorError::Process(format!(
                "Failed to run cargo build: {}",
                e
            )));
        }
    };

    // The live consumer task has the authoritative classified line vectors
    // (it mirrors the legacy reader). Join it under a short bound — the sender
    // is already dropped, so it should close promptly.
    let (error_lines, all_stderr_lines) =
        match tokio::time::timeout(Duration::from_secs(5), consumer).await {
            Ok(Ok(result)) => result,
            _ => {
                warn!("Timed out waiting for build line consumer, falling back to captured bytes");
                // Fallback: reconstruct from the captured stderr bytes so we
                // never lose the build output entirely.
                let text = String::from_utf8_lossy(&captured_stderr_bytes);
                let all: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                let errs: Vec<String> = all
                    .iter()
                    .filter(|l| BUILD_ERROR_PATTERNS.iter().any(|p| p.is_match(l)))
                    .cloned()
                    .collect();
                (errs, all)
            }
        };

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

        // Lockstep sidecar: also produce `qontinui-shim.exe` in this slot's
        // debug dir so the deploy/placement steps can carry it alongside the
        // runner exe. Fail-open — a shim build failure logs one WARN and never
        // fails the (already successful) runner build.
        build_shim_sidecar(state, slot, cargo_cwd).await;

        info!(
            "GCMD: cargo step returned status=success, run_build_inner returning Ok (slot={})",
            slot.id
        );
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

/// Filename of the install-interception shadow-stub sidecar binary built
/// alongside the runner (`[[bin]] name = "qontinui-shim"` in the runner's
/// `src-tauri/Cargo.toml`).
///
/// PLACEMENT CONTRACT: every runner resolves this stub via
/// `current_exe().parent()` (`locate_stub_exe` in the runner's
/// `shim_materializer.rs`) and materializes it into each terminal's identity
/// shim dir. The stub must therefore be deployed IN LOCKSTEP with the runner
/// exe:
///   1. built into the same slot `debug/` dir right after the runner build
///      ([`build_shim_sidecar`], called from `run_build_inner`);
///   2. carried into `target-pool/lkg/` by [`update_lkg_after_success`];
///   3. copied next to the per-runner exe copy in `target/debug/` by
///      `process::manager::start_exe_mode_for_runner` (the single deploy
///      funnel for primary, named, and temp runners).
///
/// A runner deployed without a fresh sidecar materializes whatever stale stub
/// happens to sit next to it — the 2026-07-03 incident where a 3-week-old
/// stub (predating identity mode) silently exited 0 and broke every pane
/// claude launch until it was hand-swapped.
pub const SHIM_EXE_FILENAME: &str = "qontinui-shim.exe";

/// Wall-clock budget for the fail-open `cargo build --bin qontinui-shim`
/// sidecar step. It runs right after the main runner build succeeded on the
/// SAME warm `CARGO_TARGET_DIR` with the SAME feature set, so every shared
/// dependency is already compiled — the shim itself is a small,
/// dependency-light bin that compiles + links in seconds. 10 minutes is a
/// generous ceiling for a heavily contended machine.
const SHIM_BUILD_TIMEOUT_SECS: u64 = 600;

/// Build the `qontinui-shim` sidecar into the slot's target dir, right after
/// a successful runner build (see [`SHIM_EXE_FILENAME`] for the contract).
///
/// FAIL-OPEN by design: the sidecar is a lockstep rider on the runner deploy,
/// not a gate on it. Any failure (spawn error, timeout, non-zero exit, exe
/// missing afterwards) logs a single actionable WARN and returns — it never
/// fails the runner build/restart. The cost of a missing sidecar is stale
/// identity shims; the cost of failing the build would be no runner at all.
async fn build_shim_sidecar(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    cargo_cwd: &std::path::Path,
) {
    // Same feature set as the runner build so cargo reuses the warm
    // fingerprints instead of recompiling shared deps under a different
    // feature resolution.
    const SHIM_BUILD_ARGS: &[&str] = &[
        "build",
        "--bin",
        "qontinui-shim",
        "--features",
        "custom-protocol",
    ];

    let shim_exe = slot.target_dir.join("debug").join(SHIM_EXE_FILENAME);
    let warn_stale = |detail: String| {
        format!(
            "qontinui-shim sidecar build failed on slot {} — identity shims will be stale \
             until a rebuild produces {:?} (runner build itself succeeded; deploy continues): {}",
            slot.id, shim_exe, detail
        )
    };

    info!(
        "Slot {}: building qontinui-shim sidecar (warm target {:?})",
        slot.id, slot.target_dir
    );

    let guarded = GuardedCommand::new("cargo", Duration::from_secs(SHIM_BUILD_TIMEOUT_SECS))
        .args(SHIM_BUILD_ARGS)
        .current_dir(cargo_cwd)
        .env("CARGO_TARGET_DIR", &slot.target_dir)
        .job_guarded(true);

    let failure = match guarded.run().await {
        Ok(GuardedOutcome::Exited(output)) if output.status.success() => {
            if shim_exe.exists() {
                None
            } else {
                Some(warn_stale(format!(
                    "cargo exited 0 but {:?} is missing",
                    shim_exe
                )))
            }
        }
        Ok(GuardedOutcome::Exited(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail = tail_bytes_keep_utf8(&stderr, LAST_BUILD_STDERR_SHORT_TAIL_BYTES);
            Some(warn_stale(format!(
                "cargo exit {}: {}",
                output.status,
                tail.replace('\n', " | ")
            )))
        }
        Ok(GuardedOutcome::TimedOut { after, .. }) => {
            Some(warn_stale(format!("timed out after {}s", after.as_secs())))
        }
        Ok(GuardedOutcome::Cancelled { .. }) => Some(warn_stale("cancelled".to_string())),
        Err(e) => Some(warn_stale(format!("failed to spawn cargo: {}", e))),
    };

    match failure {
        None => {
            info!(
                "Slot {}: qontinui-shim sidecar built at {:?}",
                slot.id, shim_exe
            );
        }
        Some(msg) => {
            warn!("{}", msg);
            state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
        }
    }
}

/// Prebuild the frontend inside a fresh spawn worktree before cargo runs.
///
/// A fresh `git worktree add --detach` produces an empty checkout — no
/// `node_modules/`, no `dist/`. The next `pnpm run build` would fail because
/// the dep binaries (`tsc`, `vite`, `ui-bridge-build-ir`, …) aren't
/// installed, and even if they were, cargo's `tauri::generate_context!`
/// would panic on the missing `<wt>/dist/index.html`. Idempotent: a repeated
/// spawn on the same ref whose deps are UNCHANGED returns immediately with the
/// `frontend_prebuild_skipped` log reason and doesn't re-pay the ~30s pnpm
/// install cost.
///
/// **The dep-install gate is FRESHNESS-gated, not presence-gated** — see
/// [`dep_install_reason`]. A `.spawn-<ref>` container is REUSED across refs
/// (`prepare_worktree` force-resets the same dir to the new ref), so a mere
/// `node_modules/` presence check would happily build the new ref's source
/// against the OLD ref's installed dependency tree. That is the 2026-07-12
/// P0: a reused container held `@qontinui/navigation@0.1.5` while the ref
/// pinned `^0.2.0`, and the resulting `TS2339: Property 'hasOwnPage' does not
/// exist on type 'NavigationItem'` looked exactly like a red `origin/main`
/// even though runner `origin/main` built clean.
///
/// The whole prebuild is serialized via `BuildPool.npm_lock` (the same
/// mutex the live-tree `pnpm run build` uses): `tsc` + `vite` are heavy
/// enough that two concurrent runs on the same machine routinely OOM in
/// CI, and the lock guarantees only one frontend build is in flight at a
/// time across all worktrees + the live tree.
///
/// On any failure (pnpm install non-zero exit, pnpm build non-zero exit, or
/// post-build `dist/index.html` still missing) returns
/// `SupervisorError::BuildFailed` with a 2KB tail of the **merged
/// stdout+stderr** embedded, and records the same blob on the slot so it
/// reaches `SlotHistory::last_error_detail` / `last_error_log` and the build
/// submission's `stderr_tail` (see [`record_frontend_failure`]).
async fn prebuild_worktree_frontend(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    wt_root: &std::path::Path,
    force_frontend_build: bool,
) -> Result<(), SupervisorError> {
    // Idempotency gate, split for the Phase 3 `frontend_only` fast path:
    //   * `install_reason` — Some(why) when `pnpm install` must run: either the
    //     `node_modules/.bin/ui-bridge-build-ir` marker is absent, or the
    //     dependency manifests (`pnpm-lock.yaml` + `package.json`) hash
    //     differently from the sidecar recorded at the last successful install
    //     — i.e. the installed tree is STALE relative to the checked-out ref.
    //   * `needs_build`   — `dist/index.html` is absent, so `pnpm run build`
    //     must run.
    // Default (force_frontend_build=false): if the installed deps are FRESH and
    // dist/ exists, skip the whole prebuild (repeated spawns on the same ref
    // don't re-pay install/build). `frontend_only:true` FORCES `pnpm run build`
    // regardless of `dist/index.html` presence, because a TS edit made after the
    // tree's last build would otherwise silently embed the stale dist — exactly
    // the case frontend_only exists for. `pnpm install` is still skipped when
    // the deps are provably fresh (the "fast" in fast path).
    let install_reason = dep_install_reason(wt_root);
    let needs_install = install_reason.is_some();
    let needs_build = !dist_index_present(wt_root);

    if !needs_install && !needs_build && !force_frontend_build {
        info!(
            "Slot {}: frontend_prebuild_skipped — {:?} already has fresh node_modules (dep hash matches) + dist/",
            slot.id, wt_root
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: frontend_prebuild_skipped — {:?} already has fresh node_modules (dep hash matches) + dist/",
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
    //    `pnpm install`. Skipped ONLY when the installed dep tree is provably
    //    FRESH for this checkout (marker present AND the dep-manifest hash
    //    matches the sidecar written by the last successful install) — so a
    //    `frontend_only` re-spawn on an unchanged ref pays only the
    //    `pnpm run build` cost, while a container reused across a dep change
    //    reinstalls instead of compiling against stale `node_modules`.
    if let Some(reason) = &install_reason {
        let has_lockfile = wt_root.join("pnpm-lock.yaml").exists();
        let install_args = if has_lockfile {
            "install --frozen-lockfile"
        } else {
            "install"
        };

        info!(
            "Slot {}: pnpm {} starting in {:?} — {}",
            slot.id, install_args, wt_root, reason
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: pnpm {} starting in {:?} — {}",
                    slot.id, install_args, wt_root, reason
                ),
            )
            .await;

        let install_started = std::time::Instant::now();
        let install_output = run_pnpm_command(wt_root, install_args).await.map_err(|e| {
            SupervisorError::BuildFailed(format!(
                "pnpm {} failed to spawn in spawn worktree {:?}: {}",
                install_args, wt_root, e
            ))
        })?;
        if !install_output.status.success() {
            // S2: pnpm writes resolution/peer-dep diagnostics to BOTH streams —
            // capture the merged blob, not stderr alone.
            let merged = merge_process_output(&install_output);
            let tail = record_frontend_failure(state, slot, &merged).await;
            return Err(SupervisorError::BuildFailed(format!(
                "pnpm {} failed in spawn worktree {:?} (exit {}):\n{}",
                install_args, wt_root, install_output.status, tail
            )));
        }
        let install_secs = install_started.elapsed().as_secs();

        // Stamp the dep-manifest hash AFTER a successful install: this is the
        // sidecar the next spawn's freshness gate compares against. Hashing
        // post-install (not pre-) is deliberate — a non-frozen `pnpm install`
        // may rewrite `pnpm-lock.yaml`, and it is the POST state that the
        // installed `node_modules` actually corresponds to.
        write_dep_hash_sidecar(state, slot, wt_root).await;

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
            "Slot {}: pnpm install skipped — node_modules is FRESH (dep-manifest hash matches sidecar) in {:?}",
            slot.id, wt_root
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "Slot {}: pnpm install skipped — node_modules is FRESH (dep-manifest hash matches sidecar) in {:?}",
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
    let build_output = run_pnpm_command(wt_root, "run build").await.map_err(|e| {
        SupervisorError::BuildFailed(format!(
            "pnpm run build failed to spawn in spawn worktree {:?}: {}",
            wt_root, e
        ))
    })?;
    if !build_output.status.success() {
        // S2 (the P0's second half): `tsc` and `vite` print their compiler
        // diagnostics (`error TS2339: …`) to **stdout**; stderr usually holds
        // only pnpm's own harness noise and is frequently EMPTY. Reading stderr
        // alone produced a spawn-test error body with no compiler error in it at
        // all, so the operator saw a failed build with no reason. Merge both
        // streams and record them on the slot so they reach
        // `last_error_detail` / `last_error_log` and the submission's
        // `stderr_tail`.
        let merged = merge_process_output(&build_output);
        let tail = record_frontend_failure(state, slot, &merged).await;
        return Err(SupervisorError::BuildFailed(format!(
            "pnpm run build failed in spawn worktree {:?} (exit {}):\n{}",
            wt_root, build_output.status, tail
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

// ---------------------------------------------------------------------------
// S1 — dep-install FRESHNESS gate (lockfile-hash sidecar)
// ---------------------------------------------------------------------------

/// Files whose CONTENT governs what `pnpm install` resolves into
/// `node_modules/`, in a fixed order so the hash is stable.
///
/// The runner is a **pnpm** workspace: `packageManager: pnpm@…` +
/// `pnpm-lock.yaml`, and there is no `package-lock.json` anywhere in the tree.
/// [`run_pnpm_command`] always shells `pnpm`, and `prebuild_worktree_frontend`
/// passes `--frozen-lockfile` exactly when `pnpm-lock.yaml` exists — so
/// `pnpm-lock.yaml` is THE lockfile that governs the install. `package.json` is
/// hashed too because it governs the plain-`pnpm install` fallback used when no
/// lockfile is present (and because `--frozen-lockfile` cross-checks it, so a
/// package.json/lockfile disagreement must not be skipped). `pnpm-workspace.yaml`
/// defines which packages take part in the install graph. `package-lock.json` is
/// deliberately NOT hashed: pnpm never consumes it, so an npm-written one would
/// only cause spurious reinstalls.
const DEP_MANIFEST_FILES: &[&str] = &["pnpm-lock.yaml", "package.json", "pnpm-workspace.yaml"];

/// Sidecar recording the [`dep_manifest_hash`] of the tree as it was at the last
/// SUCCESSFUL `pnpm install` in this worktree.
///
/// Lives INSIDE `node_modules/` on purpose: `node_modules/` is gitignored (so
/// this never dirties a `.spawn-<ref>` worktree or a caller-owned
/// `worktree_path` checkout), and it is the exact artifact the hash describes —
/// `rm -rf node_modules` takes the sidecar with it, which correctly degrades to
/// "install needed" instead of leaving a hash that vouches for a tree that no
/// longer exists. Every failure mode of this file (absent, unreadable, pruned by
/// a future pnpm) degrades toward REINSTALL, never toward a stale skip.
const DEP_HASH_SIDECAR: &str = ".qontinui-supervisor-dep-hash";

/// SHA-256 over the dependency-governing manifests in `wt_root`
/// ([`DEP_MANIFEST_FILES`]), hex-encoded.
///
/// Absent files are folded in as an explicit `absent` marker so that
/// *removing* a lockfile changes the hash just as much as editing it. Each
/// file's name and byte length are mixed in ahead of its bytes so no
/// concatenation of two files can collide with another pair.
///
/// Returns `None` iff NONE of the manifests exist — i.e. the tree is not a JS
/// project at all and there is nothing for `pnpm install` to govern. Callers
/// treat that as "cannot reason about freshness" and fall back to the legacy
/// marker-presence gate rather than forcing a doomed `pnpm install`.
fn dep_manifest_hash(wt_root: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut any_present = false;

    for name in DEP_MANIFEST_FILES {
        hasher.update(name.as_bytes());
        match std::fs::read(wt_root.join(name)) {
            Ok(bytes) => {
                any_present = true;
                hasher.update(b"\x01");
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
            Err(_) => {
                hasher.update(b"\x00absent");
            }
        }
    }

    if !any_present {
        return None;
    }
    Some(hex::encode(hasher.finalize()))
}

/// Absolute path of the dep-hash sidecar for `wt_root`.
fn dep_hash_sidecar_path(wt_root: &std::path::Path) -> PathBuf {
    wt_root.join("node_modules").join(DEP_HASH_SIDECAR)
}

/// Read the recorded dep-manifest hash for `wt_root`. `None` on any failure
/// (absent, unreadable, empty) — which the gate treats as "unknown provenance ⇒
/// reinstall".
fn read_dep_hash_sidecar(wt_root: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(dep_hash_sidecar_path(wt_root)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// THE dep-install gate. `Some(reason)` ⇒ `pnpm install` must run (the reason is
/// logged and surfaced); `None` ⇒ the installed `node_modules` is provably fresh
/// for this checkout and the install is skipped.
///
/// Freshness-gated, not presence-gated. The predecessor gate asked only "does
/// `node_modules/.bin/ui-bridge-build-ir` exist?", which is true for a
/// `.spawn-<ref>` container that was populated for a DIFFERENT ref — the
/// container is reused and force-reset to the new ref, but `node_modules/` is
/// not touched. The result was a build of the new ref's TypeScript against the
/// old ref's dependency tree (2026-07-12: `@qontinui/navigation@0.1.5` installed
/// against a `^0.2.0` pin ⇒ `TS2339 … 'hasOwnPage' … 'NavigationItem'`), which
/// is indistinguishable from a genuinely red `origin/main`.
///
/// Order matters: the marker check comes first so a wiped/half-installed
/// `node_modules` always reinstalls even if a stale sidecar somehow survived.
fn dep_install_reason(wt_root: &std::path::Path) -> Option<String> {
    if frontend_install_marker_missing(wt_root) {
        return Some(
            "node_modules install marker (node_modules/.bin/ui-bridge-build-ir) is absent"
                .to_string(),
        );
    }

    // `None` ⇒ no pnpm-lock.yaml / package.json / pnpm-workspace.yaml at all:
    // not a JS project, nothing for the install to govern. Preserve the legacy
    // presence-gate outcome (marker exists ⇒ skip) rather than running a
    // `pnpm install` that would just fail.
    let current = dep_manifest_hash(wt_root)?;

    match read_dep_hash_sidecar(wt_root) {
        None => Some(format!(
            "dep-manifest hash sidecar (node_modules/{}) is absent — the provenance of the \
             installed node_modules is UNKNOWN, so it cannot be trusted for this checkout \
             (expected {})",
            DEP_HASH_SIDECAR,
            short_hash(&current),
        )),
        Some(recorded) if recorded != current => Some(format!(
            "dep-manifest hash CHANGED ({} → {}) — node_modules was installed for a different \
             set of {:?} and is STALE for this checkout",
            short_hash(&recorded),
            short_hash(&current),
            DEP_MANIFEST_FILES,
        )),
        Some(_) => None,
    }
}

/// First 12 hex chars of a hash, for log-readable reporting.
fn short_hash(h: &str) -> String {
    h.chars().take(12).collect()
}

/// Record the current dep-manifest hash for `wt_root` after a successful
/// `pnpm install`, so the next spawn on this container can prove freshness.
///
/// Best-effort: a write failure logs a WARN and leaves no sidecar, which makes
/// the next spawn reinstall (slow, correct) rather than skip (fast, wrong). It
/// never fails the build.
async fn write_dep_hash_sidecar(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    wt_root: &std::path::Path,
) {
    let Some(hash) = dep_manifest_hash(wt_root) else {
        return;
    };
    let path = dep_hash_sidecar_path(wt_root);
    match tokio::fs::write(&path, hash.as_bytes()).await {
        Ok(()) => {
            info!(
                "Slot {}: dep-manifest hash sidecar written ({} = {})",
                slot.id,
                short_hash(&hash),
                path.display()
            );
        }
        Err(e) => {
            let msg = format!(
                "Slot {}: failed to write dep-manifest hash sidecar {:?}: {} — the next spawn on \
                 this worktree will reinstall dependencies instead of skipping (safe, just slower)",
                slot.id, path, e
            );
            warn!("{}", msg);
            state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
        }
    }
}

// ---------------------------------------------------------------------------
// S2 — frontend failures must carry the compiler error (stdout, not just stderr)
// ---------------------------------------------------------------------------

/// Merge a finished child's stdout AND stderr into one labelled diagnostic blob.
///
/// `tsc` and `vite` print their compiler diagnostics (`error TS2339: …`) to
/// **stdout**; stderr typically carries only pnpm's harness noise and is often
/// EMPTY. Every caller that reported "the frontend build failed" from
/// `output.stderr` alone therefore produced an error body with no compiler error
/// in it. Empty sections are omitted so a stderr-only failure reads cleanly.
fn merge_process_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut merged = String::with_capacity(stdout.len() + stderr.len() + 32);
    if !stdout.trim().is_empty() {
        merged.push_str("--- stdout ---\n");
        merged.push_str(stdout.trim_end());
        merged.push('\n');
    }
    if !stderr.trim().is_empty() {
        merged.push_str("--- stderr ---\n");
        merged.push_str(stderr.trim_end());
        merged.push('\n');
    }
    merged
}

/// Record a frontend (pnpm) failure blob on the slot so it reaches every
/// downstream error surface, and return the short tail to embed in the returned
/// `SupervisorError::BuildFailed` message.
///
/// The frontend prebuild fails BEFORE cargo ever runs, so none of the cargo
/// failure plumbing fires. Without this, `run_cargo_build_with_dir_detailed`
/// finds no `last-build.stderr` sidecar and no `last_build_stderr_capture`, and
/// the spawn-test submission comes back with `stderr_tail: []` and
/// `last_error_detail: null` — a failed build whose reason is invisible. Writing
/// both surfaces here reuses the EXACT plumbing a cargo failure uses:
///
///  * `<slot.target_dir>/last-build.stderr` → read back into
///    `BuildAttempt::full_stderr` → the submission's `stderr_tail`;
///  * `slot.last_build_stderr_capture`      → folded into
///    `SlotHistory::last_error_detail` + `last_error_log`.
async fn record_frontend_failure(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    merged_output: &str,
) -> String {
    let stderr_path = slot.target_dir.join("last-build.stderr");
    if let Err(e) = tokio::fs::write(&stderr_path, merged_output.as_bytes()).await {
        warn!(
            "Failed to persist frontend last-build.stderr for slot {} at {:?}: {}",
            slot.id, stderr_path, e
        );
    }

    let detail_tail = tail_bytes_keep_utf8(merged_output, LAST_BUILD_STDERR_DETAIL_BYTES);
    *slot.last_build_stderr_capture.write().await = Some(detail_tail);

    let short_tail = tail_bytes_keep_utf8(merged_output, LAST_BUILD_STDERR_SHORT_TAIL_BYTES);
    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Error,
            format!(
                "Slot {}: frontend (pnpm) step FAILED — captured stdout+stderr:\n{}",
                slot.id, short_tail
            ),
        )
        .await;
    short_tail
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
async fn run_pnpm_command(
    cwd: &std::path::Path,
    args: &str,
) -> Result<std::process::Output, std::io::Error> {
    let timeout_secs = crate::config::pnpm_timeout_secs();
    info!(
        "GCMD: pnpm start args={:?} cwd={:?} timeout={}s",
        args, cwd, timeout_secs
    );

    // Build the GuardedCommand. On Windows pnpm ships as a `.cmd` shim, so it
    // must be invoked via `cmd /C pnpm.cmd <args>` exactly as the legacy
    // invocation did. On POSIX call `pnpm` directly with split argv tokens.
    #[cfg(windows)]
    let guarded = GuardedCommand::new("cmd", Duration::from_secs(timeout_secs))
        .args(["/C", &format!("pnpm.cmd {}", args)])
        .current_dir(cwd)
        // Match the live-tree invocation: vite.config.ts gates the build
        // target on TAURI_PLATFORM=windows.
        .env("TAURI_PLATFORM", "windows")
        // Force non-interactive pnpm. Without a TTY, `pnpm install` will abort
        // with ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY when it wants to
        // purge node_modules, leaving a STALE tree behind. CI=true makes pnpm
        // assume a CI environment (same as GitHub Actions) and proceed
        // non-interactively, so the install can never abort mid-purge.
        .env("CI", "true")
        .job_guarded(true);

    #[cfg(not(windows))]
    let guarded = {
        let split_args: Vec<&str> = args.split_whitespace().collect();
        GuardedCommand::new("pnpm", Duration::from_secs(timeout_secs))
            .args(split_args)
            .current_dir(cwd)
            // Force non-interactive pnpm (see the Windows branch above):
            // CI=true prevents ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY so a
            // node_modules purge can never abort and leave a stale tree.
            .env("CI", "true")
            .job_guarded(true)
    };

    let outcome = guarded.run().await?;

    match outcome {
        GuardedOutcome::Exited(output) => {
            info!(
                "GCMD: pnpm done outcome=Exited exit={} args={:?}",
                output.status, args
            );
            Ok(output)
        }
        GuardedOutcome::TimedOut { after, .. } => {
            warn!(
                "GCMD: pnpm done outcome=TimedOut after={}s args={:?}",
                after.as_secs(),
                args
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "pnpm '{}' timed out after {}s in {:?}",
                    args,
                    after.as_secs(),
                    cwd
                ),
            ))
        }
        GuardedOutcome::Cancelled { .. } => {
            warn!("GCMD: pnpm done outcome=Cancelled args={:?}", args);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("pnpm '{}' cancelled in {:?}", args, cwd),
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
        // git rev-parse is a fast leaf process that never forks a pipe-holding
        // grandchild, so `job_guarded(false)` — the wall-clock timeout +
        // direct-child kill is sufficient and we avoid a JobObject per probe.
        let outcome = GuardedCommand::new(
            "git",
            Duration::from_secs(crate::config::git_timeout_secs()),
        )
        .args(args)
        .current_dir(cwd)
        .job_guarded(false)
        .run()
        .await
        .ok()?;
        let out = match outcome {
            GuardedOutcome::Exited(out) => out,
            // A wedged git probe times out (or is cancelled) → treat as
            // "couldn't determine", same as a non-zero exit.
            _ => return None,
        };
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    let head = match run_git(&["rev-parse", "HEAD"], &git_dir).await {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    // Phase A: compute the full drift (behind-count + ancestor test) rather
    // than the old short-sha equality. `origin_main_drift` fetches origin first
    // (best-effort, offline-tolerant) so the origin/main ref is fresh — a stale
    // local origin/main is exactly how the 2026-06-07 incident hid the
    // regression.
    let drift = crate::git_provenance::origin_main_drift(&git_dir, &head).await;

    // Not computable (no remote / no origin/main / not a repo) ⇒ skip silently,
    // preserving the legacy best-effort tolerance.
    if drift.origin_main_sha.is_empty() {
        return;
    }
    // Up to date ⇒ nothing to warn about.
    if drift.is_up_to_date() {
        return;
    }

    let branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], &git_dir)
        .await
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_string());

    let head_short: String = drift.built_sha.chars().take(12).collect();
    let main_short: String = drift.origin_main_sha.chars().take(12).collect();

    // `is_ancestor == false` is the more dangerous case (diverged / parked on a
    // feature branch); `is_ancestor == true && behind_count > 0` is the
    // clean-but-stale incident shape.
    let drift_kind = if drift.is_diverged() {
        "DIVERGED from"
    } else {
        "behind"
    };
    let msg = format!(
        "Slot {}: working tree HEAD ({}, branch={}) is {} origin/main ({}) by {} commit(s) \
         (diverged={}). This build will compile {}, NOT main. Read `git_sha` from the spawn \
         response to confirm what actually ran. See qontinui-supervisor#21.",
        slot_id,
        head_short,
        branch,
        drift_kind,
        main_short,
        drift.behind_count,
        drift.is_diverged(),
        head_short
    );
    warn!("{}", msg);
    state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
}

/// Resolve the qontinui-runner repo HEAD SHA. Returns `None` on any error
/// (git missing, not a repo, detached HEAD with no SHA, etc.). Best-effort.
///
/// `pub(crate)` so the detached rebuild path (`routes/runner.rs`, Phase A4) can
/// resolve the just-built primary sha to evaluate origin/main drift against it.
pub(crate) async fn rev_parse_head(git_dir: &std::path::Path) -> Option<String> {
    let outcome = GuardedCommand::new(
        "git",
        Duration::from_secs(crate::config::git_timeout_secs()),
    )
    .args(["rev-parse", "HEAD"])
    .current_dir(git_dir)
    .job_guarded(false)
    .run()
    .await
    .ok()?;
    let out = match outcome {
        GuardedOutcome::Exited(out) => out,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Pure selection of the tree ROOT to probe for provenance — which directory
/// was actually compiled.
///
/// `build_dir_override` and `project_dir` both point at a runner `src-tauri`
/// dir, so the tree root is `.parent()` in both cases (the same relationship).
/// On the degenerate no-parent case we fall back to the dir itself rather than
/// panic — the SHA probe will then just fail and record `sha: None`.
///
/// NOTE: this no longer decides the [`BuildSource`]. The source classification
/// is now carried explicitly by [`BuildSourceKind`] from the caller, because a
/// `Some(src_tauri)` override can be either an `OriginMain` primary build or a
/// foreign `Override` spawn-test — indistinguishable by path alone.
fn provenance_tree_root(
    project_dir: &std::path::Path,
    build_dir_override: Option<&std::path::Path>,
) -> PathBuf {
    let src_tauri = build_dir_override.unwrap_or(project_dir);
    src_tauri
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| src_tauri.to_path_buf())
}

/// Compute the [`BuildProvenance`] of a just-completed successful build.
///
/// The recorded [`BuildSource`] comes from the caller's explicit `source_kind`
/// (NOT inferred from `build_dir_override`), so a primary `OriginMain` build is
/// distinguished from a spawn-test `Override` even though both carry a
/// `Some(src_tauri)`.
///
/// The SHA:
/// - `OriginMain { resolved_sha }` ⇒ the exact `origin/main` commit
///   `prepare_worktree` checked out, recorded verbatim (no re-probe — the
///   worktree's HEAD is already known and authoritative).
/// - `LiveTree` / `Override` ⇒ probed from the tree root that was built
///   (`build_dir_override.parent()` or `project_dir.parent()`). Best-effort,
///   mirroring the legacy posture: a probe failure yields `sha: None` (logged
///   as a warning) and the build still succeeds.
///
/// `built_from` always records the absolute tree root that was probed/built,
/// even when the SHA probe fails, so the forensic trail survives.
async fn compute_build_provenance(
    state: &SharedState,
    build_dir_override: Option<&std::path::Path>,
    source_kind: &BuildSourceKind,
) -> BuildProvenance {
    let source = source_kind.build_source();
    let tree_root = provenance_tree_root(&state.config.project_dir, build_dir_override);

    let sha = match source_kind {
        // origin/main worktree: the resolved SHA is already authoritative.
        BuildSourceKind::OriginMain { resolved_sha } => Some(resolved_sha.clone()),
        // live tree / foreign override: probe the built tree's HEAD.
        BuildSourceKind::LiveTree | BuildSourceKind::Override => {
            match rev_parse_head(&tree_root).await {
                Some(s) => Some(s),
                None => {
                    warn!(
                        "Build provenance: git rev-parse HEAD failed or returned empty in {:?} \
                         (source={:?}); recording sha=null. Build still succeeded.",
                        tree_root, source
                    );
                    None
                }
            }
        }
    };

    BuildProvenance {
        sha,
        source,
        built_from: tree_root.to_string_lossy().to_string(),
        built_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Stamp the slot's freshly-built runner exe with its [`BuildProvenance`] in a
/// JSON sidecar (`<slot>/debug/qontinui-runner.exe.provenance.json`).
/// Best-effort — a write failure is logged but the build still counts as
/// succeeded; the sidecar is observability for cross-slot drift detection and
/// (Phase 2) the LKG promotion gate.
///
/// Read back by [`crate::process::manager::read_slot_provenance`].
async fn write_slot_provenance_sidecar(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    provenance: &BuildProvenance,
) {
    use tracing::debug;
    let exe_path = state.config.runner_exe_path_for_slot(slot.id);
    let sidecar = match exe_path.parent() {
        Some(dir) => dir.join(crate::process::manager::SLOT_PROVENANCE_SIDECAR_FILENAME),
        None => {
            debug!(
                "Slot {} provenance sidecar: exe path {:?} has no parent dir; skipping",
                slot.id, exe_path
            );
            return;
        }
    };
    let json = match serde_json::to_string(provenance) {
        Ok(s) => s,
        Err(e) => {
            debug!(
                "Slot {} provenance sidecar: serialize failed: {}",
                slot.id, e
            );
            return;
        }
    };
    if let Err(e) = std::fs::write(&sidecar, json.as_bytes()) {
        debug!(
            "Slot {} provenance sidecar: write failed for {:?}: {}",
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

/// Cap on the generous stderr tail surfaced through `GET /build/{id}/status`'s
/// `stderr_tail` on a cargo-pool build failure. Much larger than the 2 KB
/// inline `last_error` cap so the real compiler diagnostic (which often lives
/// far above cargo's terminal "could not compile" summary) is recoverable from
/// the build submission without trawling `last-build.stderr` on disk. Issue 3.
const LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES: usize = 16 * 1024;

/// Classification of a failed cargo build's stderr — does it carry a genuine
/// Rust compiler diagnostic, or is it environmental noise (a poisoned slot,
/// stale fingerprint, linker hiccup) that a clean retry would clear?
///
/// Drives the spawn-test poisoned-slot self-heal (Issue 3, fix part 2): only
/// `Environmental` failures are worth a single cleaned-slot retry. A
/// `CompilerDiagnostic` is the user's code error — retrying is wasteful and
/// must return immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrClass {
    /// Contains a genuine compiler diagnostic (`error[E####]` or cargo's
    /// `could not compile` summary). The failure is in the *code*; a retry
    /// would just reproduce it.
    CompilerDiagnostic,
    /// No compiler diagnostic detected — the failure is almost certainly slot
    /// state corruption (poisoned incremental/fingerprint state, a transient
    /// linker/IO error). Worth one retry in a cleaned slot.
    Environmental,
}

/// Matches a real rustc diagnostic code `error[E0432]` (the `E` followed by at
/// least one digit, inside the brackets cargo prints). Deliberately strict:
/// a bare `error:` line (which cargo also prints for environmental failures
/// like "could not find `Cargo.toml`") must NOT count as a compiler
/// diagnostic, or every environmental failure would suppress the self-heal.
static COMPILER_ERROR_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"error\[E\d+\]").expect("static regex compiles"));

/// Classify a failed cargo build's stderr. Returns [`StderrClass::CompilerDiagnostic`]
/// iff the text contains an `error[E####]` diagnostic code OR cargo's
/// `could not compile` terminal summary — both unambiguous signals that the
/// build failed on the *code*. Everything else (linker errors, fingerprint
/// corruption, "Compiling …" noise with no diagnostic) is
/// [`StderrClass::Environmental`] and eligible for a cleaned-slot retry.
pub fn classify_build_stderr(stderr: &str) -> StderrClass {
    if COMPILER_ERROR_CODE.is_match(stderr) || stderr.contains("could not compile") {
        StderrClass::CompilerDiagnostic
    } else {
        StderrClass::Environmental
    }
}

/// Outcome detail of a single cargo build attempt, returned by
/// [`run_cargo_build_with_dir_detailed`] alongside the `Result`. Carries the
/// slot id the build actually ran on (so the caller can clean + retry exactly
/// that slot — Issue 3 fix part 2) and, on failure, the FULL captured cargo
/// stderr (so the caller can surface it through `GET /build/{id}/status` rather
/// than the 2 KB inline `last_error` cap — Issue 3 fix part 1).
#[derive(Debug, Clone, Default)]
pub struct BuildAttempt {
    /// The build-pool slot id this attempt claimed, when one was claimed. `None`
    /// only when the build was refused before claiming a slot (disk guard /
    /// semaphore closed).
    pub slot_id: Option<usize>,
    /// Full cargo stderr captured for this attempt, when a slot ran cargo and
    /// the `last-build.stderr` sidecar was readable. `None` on success or when
    /// the failure happened before/around cargo (no stderr produced).
    pub full_stderr: Option<String>,
}

/// Generous tail of a failed build's full cargo stderr, capped at
/// [`LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES`] (16 KB) for surfacing through
/// `GET /build/{id}/status`'s `stderr_tail`. Much larger than the 2 KB inline
/// `last_error` cap so the real compiler diagnostic survives even when cargo
/// prints a lot of "Compiling …" noise after it. Issue 3 fix part 1.
pub fn stderr_submission_tail(full_stderr: &str) -> String {
    tail_bytes_keep_utf8(full_stderr, LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES)
}

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

/// Clear a single build-pool slot's incremental/fingerprint state by emptying
/// its `target_dir`, then recreating the (now empty) dir so the next build
/// finds it present (matching how `BuildPool::new` provisions slots eagerly).
///
/// This is the cleaned-slot primitive the spawn-test poisoned-slot self-heal
/// (Issue 3 fix part 2) calls before retrying an environmentally-failed build:
/// wiping the slot's `CARGO_TARGET_DIR` discards the poisoned incremental
/// fingerprints that made an otherwise-clean tree fail to compile. Returns the
/// number of bytes freed (best-effort; 0 on a fresh/missing dir). A
/// `NotFound`-on-remove is treated as already-empty (Ok); any other remove
/// error is returned so the caller can decide whether the retry is still worth
/// attempting.
pub async fn clean_slot_target(slot: &Arc<BuildSlot>) -> Result<u64, std::io::Error> {
    let bytes_before = crate::footprint::dir_size_bytes(&slot.target_dir);
    match tokio::fs::remove_dir_all(&slot.target_dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Recreate the empty dir; a failure here is non-fatal (the next build's
    // CARGO_TARGET_DIR handling recreates it) but worth surfacing as an error.
    tokio::fs::create_dir_all(&slot.target_dir).await?;
    let bytes_after = crate::footprint::dir_size_bytes(&slot.target_dir);
    Ok(bytes_before.saturating_sub(bytes_after))
}

/// Check-only holder detection for a slot exe (Phase 3, `POST
/// /builds/slots/{id}/clean`). Returns the PIDs of live processes whose image
/// is `exe_path`. Reuses the same `find_pids_holding_exe` machinery
/// [`free_slot_exe`] uses, but performs NO kills — the clean endpoint only
/// needs to know whether it's safe to delete. Empty on non-Windows (no
/// image-path holder concept for a stalled file lock) and when the exe is
/// absent.
pub async fn slot_exe_holders(exe_path: &std::path::Path) -> Vec<u32> {
    if !exe_path.exists() {
        return Vec::new();
    }
    #[cfg(target_os = "windows")]
    {
        find_pids_holding_exe(exe_path).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = exe_path;
        Vec::new()
    }
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
    // Pre-permit disk guard (Phase 2): the prewarm `cargo check` also writes
    // GBs into a slot, so it is gated by the same disk floor as a real build.
    // Guarding ONLY the real-build path would let prewarm fill a near-full
    // disk (a vet-flagged defect) — both permit-acquisition sites are covered.
    check_disk_guard(state).await?;

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

    // Per-build line bus so prewarm stderr lines stream to logs live (mirrors
    // the legacy reader task) while GuardedCommand owns the pipe + JobObject.
    let (line_tx, mut line_rx) = tokio::sync::broadcast::channel::<String>(4096);
    {
        let state_clone = state.clone();
        let slot_id = slot.id;
        tokio::spawn(async move {
            loop {
                match line_rx.recv().await {
                    Ok(line) => {
                        state_clone
                            .logs
                            .emit(
                                LogSource::Build,
                                LogLevel::Info,
                                format!("[prewarm slot {}] {}", slot_id, line),
                            )
                            .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    info!(
        "GCMD: prewarm cargo check start slot={} timeout={}s",
        slot.id, PREWARM_TIMEOUT_SECS
    );
    let outcome = GuardedCommand::new("cargo", Duration::from_secs(PREWARM_TIMEOUT_SECS))
        .args(args)
        .current_dir(&state.config.project_dir)
        .env("CARGO_TARGET_DIR", &slot.target_dir)
        .job_guarded(true)
        .stream_lines(line_tx)
        .run()
        .await;

    // Map the GuardedOutcome back onto the legacy match shape: `Ok(Ok(status))`
    // for a clean exit, the timeout arm for TimedOut/Cancelled, and a process
    // error for a spawn failure.
    let wait_result: Result<Result<std::process::ExitStatus, std::io::Error>, ()> = match outcome {
        Ok(GuardedOutcome::Exited(out)) => Ok(Ok(out.status)),
        Ok(GuardedOutcome::TimedOut { .. }) | Ok(GuardedOutcome::Cancelled { .. }) => Err(()),
        Err(e) => Ok(Err(e)),
    };

    match wait_result {
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
            // GuardedCommand already killed the (whole) process tree on its
            // timeout/cancel arm before returning, so there's nothing left to
            // kill here.
            warn!(
                "Prewarm of slot {} timed out after {}s (tree killed by GuardedCommand)",
                slot.id, PREWARM_TIMEOUT_SECS
            );
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
/// and write a `lkg.json` sidecar with `{built_at, source_slot, exe_size, sha,
/// source}`.
///
/// **Override builds are not promoted.** When `provenance.source` is
/// [`BuildSource::Override`] (a spawn-test `git_ref` / `worktree_path` preview
/// of a foreign tree) the function logs and returns `Ok(())` WITHOUT touching
/// the LKG exe or sidecar. This is the root fix for the 2026-06-05 incident
/// where a branch build was promoted to LKG and a restart deployed it to the
/// primary. Because the gate consumes the same `provenance` value the slot
/// sidecar was written from, the writer and the gate can never disagree. The
/// gate is `BuildSource::is_vouched()` — `LiveTree` AND `OriginMain` are both
/// promoted (an origin/main primary build SHOULD advance LKG); only `Override`
/// is skipped. Consequently every `lkg.json` written here records
/// `source: "live_tree"` or `source: "origin_main"` — taken from
/// `provenance.source`, not hard-coded, so the record is honest by
/// construction.
///
/// Both writes go through a temp-file + atomic rename so a crash partway
/// through cannot leave the LKG dir holding a torn binary or a sidecar that
/// describes a different exe than the one on disk.
///
/// Called from the build-success path with the slot whose cargo build just
/// returned `Ok` and that build's provenance. On any failure, the previous
/// LKG (if any) is left intact — the caller logs the error but the build
/// still counts as succeeded.
async fn update_lkg_after_success(
    state: &SharedState,
    slot: &Arc<BuildSlot>,
    provenance: &BuildProvenance,
) -> Result<(), SupervisorError> {
    // LKG promotion gate: an override build of a foreign tree must never
    // become the deploy fallback. The slot sidecar was still written by the
    // caller (Phase 1 behavior unchanged) — only LKG promotion is skipped.
    // `is_vouched()` promotes both LiveTree and OriginMain (a default primary
    // origin/main build SHOULD advance LKG); only Override is excluded.
    if !provenance.source.is_vouched() {
        info!(
            "skipping LKG promotion (override build of {})",
            provenance.built_from
        );
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "LKG promotion skipped: override build of {} (slot {})",
                    provenance.built_from, slot.id
                ),
            )
            .await;
        return Ok(());
    }

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

    // Carry the `qontinui-shim.exe` sidecar into the LKG dir alongside the
    // exe (same tmp+rename dance) so an LKG-pinned start (spawn-test
    // {use_lkg: true}) deploys a matching stub, not whatever stale one sits
    // in `target/debug/`. See [`SHIM_EXE_FILENAME`] for the placement
    // contract. Fail-open: the exe promotion above already succeeded; a
    // missing or uncopyable shim logs one WARN and never fails the build.
    {
        let shim_src = source_exe.with_file_name(SHIM_EXE_FILENAME);
        let shim_dst = lkg_dir.join(SHIM_EXE_FILENAME);
        let shim_tmp = lkg_dir.join(format!("{}.tmp.{}", SHIM_EXE_FILENAME, slot.id));
        let shim_result: Result<(), String> = if !shim_src.exists() {
            Err(format!("{:?} not found next to the slot exe", shim_src))
        } else {
            let _ = std::fs::remove_file(&shim_tmp);
            std::fs::copy(&shim_src, &shim_tmp)
                .map_err(|e| format!("copy {:?} -> {:?}: {}", shim_src, shim_tmp, e))
                .and_then(|_| {
                    std::fs::rename(&shim_tmp, &shim_dst)
                        .map_err(|e| format!("rename {:?} -> {:?}: {}", shim_tmp, shim_dst, e))
                })
        };
        if let Err(detail) = shim_result {
            let _ = std::fs::remove_file(&shim_tmp);
            let msg = format!(
                "LKG shim sidecar capture failed (slot {}) — identity shims will be stale \
                 on LKG-pinned starts until a rebuild refreshes {:?}: {}",
                slot.id, shim_dst, detail
            );
            warn!("{}", msg);
            state.logs.emit(LogSource::Build, LogLevel::Warn, msg).await;
        }
    }

    let info = LkgInfo {
        built_at: chrono::Utc::now(),
        source_slot: slot.id,
        exe_size,
        // From provenance — never re-probed. `source` is necessarily
        // `LiveTree` here (override builds returned early above), but we write
        // it from `provenance.source` so the record is honest by construction,
        // not by assumption.
        sha: provenance.sha.clone(),
        source: provenance.source,
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
    use super::{
        classify_build_stderr, dep_hash_sidecar_path, dep_install_reason, dep_manifest_hash,
        dist_index_ok, merge_process_output, needs_frontend_prebuild, provenance_tree_root,
        rev_parse_head, stderr_submission_tail, update_lkg_after_success, verify_frontend_built,
        BuildPhase, BuildProvenance, BuildSource, BuildSourceKind, StderrClass,
        LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES,
    };
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
    use crate::state::{SharedState, SupervisorState};
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// The phase marker must round-trip through `as_u8`/`from_u8` and produce a
    /// phase-accurate queue-timeout message for each phase. This guards the
    /// attribution against regression with no live build (plan
    /// 2026-06-13-spawn-test-queue-timeout-attribution Verification).
    #[test]
    fn build_phase_round_trips_and_maps_to_message() {
        for phase in [
            BuildPhase::AwaitingSlot,
            BuildPhase::AwaitingNpmLock,
            BuildPhase::BuildingFrontend,
            BuildPhase::Compiling,
        ] {
            // u8 round-trip.
            assert_eq!(BuildPhase::from_u8(phase.as_u8()), phase);
        }

        // AwaitingSlot is the only phase whose message names a cargo build slot
        // (the genuine slot wait). Permit count is irrelevant here.
        let slot_msg = BuildPhase::AwaitingSlot.timeout_message(30, 3);
        assert!(slot_msg.contains("cargo build slot"), "{slot_msg}");
        assert!(slot_msg.contains("30s"), "{slot_msg}");

        // AwaitingNpmLock attributes the wait to the frontend lock and reports
        // the free cargo permits — the exact mis-attributed starvation case.
        let npm_msg = BuildPhase::AwaitingNpmLock.timeout_message(30, 3);
        assert!(npm_msg.contains("frontend (pnpm) lock"), "{npm_msg}");
        assert!(npm_msg.contains("3 cargo permits free"), "{npm_msg}");
        assert!(
            !npm_msg.contains("build slot"),
            "npm-lock message must NOT claim a slot wait: {npm_msg}"
        );

        // BuildingFrontend names the frontend build, still reporting free permits.
        let fe_msg = BuildPhase::BuildingFrontend.timeout_message(45, 2);
        assert!(fe_msg.contains("frontend (pnpm) build"), "{fe_msg}");
        assert!(fe_msg.contains("2 cargo permits free"), "{fe_msg}");
        assert!(!fe_msg.contains("build slot"), "{fe_msg}");

        // Compiling makes clear the slot was already held (not a slot wait).
        let compile_msg = BuildPhase::Compiling.timeout_message(60, 0);
        assert!(compile_msg.contains("compiling (cargo)"), "{compile_msg}");
        assert!(
            !compile_msg.contains("for a cargo build slot"),
            "{compile_msg}"
        );
    }

    /// An out-of-range marker value decodes to the conservative `AwaitingSlot`
    /// default (an attempt that never advanced the marker reads as the initial
    /// slot wait, not a panic).
    #[test]
    fn build_phase_from_u8_out_of_range_defaults_to_awaiting_slot() {
        assert_eq!(BuildPhase::from_u8(7), BuildPhase::AwaitingSlot);
        assert_eq!(BuildPhase::from_u8(255), BuildPhase::AwaitingSlot);
    }

    /// `git init` a real repo at `dir` with one commit, returning its HEAD SHA.
    /// Mirrors the temp-repo fixture pattern in `spawn_worktree.rs` tests.
    fn init_git_repo_one_commit(dir: &std::path::Path, seed_name: &str) -> String {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        fs::write(dir.join(seed_name), seed_name.as_bytes()).expect("seed");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "initial"]);
        let head = run(&["rev-parse", "HEAD"]);
        String::from_utf8_lossy(&head.stdout).trim().to_string()
    }

    /// `provenance_tree_root` selects `.parent()` of the live `project_dir`
    /// when there's no override. (Source classification is no longer the
    /// function's job — it comes from `BuildSourceKind`.)
    #[test]
    fn provenance_tree_root_live_tree() {
        let project_dir = std::path::Path::new("/ws/qontinui-runner/src-tauri");
        let root = provenance_tree_root(project_dir, None);
        assert_eq!(root, std::path::Path::new("/ws/qontinui-runner"));
    }

    /// `provenance_tree_root` selects `.parent()` of the OVERRIDE src-tauri,
    /// ignoring `project_dir` entirely.
    #[test]
    fn provenance_tree_root_override() {
        let project_dir = std::path::Path::new("/ws/qontinui-runner/src-tauri");
        let over = std::path::Path::new("/ws/.spawn-feat/qontinui-runner/src-tauri");
        let root = provenance_tree_root(project_dir, Some(over));
        assert_eq!(
            root,
            std::path::Path::new("/ws/.spawn-feat/qontinui-runner")
        );
    }

    /// The motivating-incident guard: with two distinct git repos (the "live"
    /// tree and an "override" worktree at a DIFFERENT HEAD), the SHA probed for
    /// an override build is the OVERRIDE tree's HEAD, not the live tree's.
    #[tokio::test]
    async fn override_build_probes_override_tree_sha_not_live() {
        let base = TempDir::new().expect("tempdir");

        // Live tree: <base>/live/qontinui-runner with src-tauri.
        let live_root = base.path().join("live").join("qontinui-runner");
        let live_src_tauri = live_root.join("src-tauri");
        fs::create_dir_all(&live_src_tauri).expect("mkdir live");
        let live_sha = init_git_repo_one_commit(&live_root, "live-seed");

        // Override tree: <base>/override/qontinui-runner with src-tauri, a
        // DIFFERENT repo with a different HEAD.
        let over_root = base.path().join("override").join("qontinui-runner");
        let over_src_tauri = over_root.join("src-tauri");
        fs::create_dir_all(&over_src_tauri).expect("mkdir override");
        let over_sha = init_git_repo_one_commit(&over_root, "override-seed");

        assert_ne!(live_sha, over_sha, "fixture must produce distinct HEADs");

        // Live-tree selection probes the live tree's HEAD.
        let live_probe_root = provenance_tree_root(&live_src_tauri, None);
        assert_eq!(
            rev_parse_head(&live_probe_root).await,
            Some(live_sha.clone())
        );

        // Override selection probes the OVERRIDE tree's HEAD — the bug fix.
        let over_probe_root = provenance_tree_root(&live_src_tauri, Some(over_src_tauri.as_path()));
        assert_eq!(
            rev_parse_head(&over_probe_root).await,
            Some(over_sha.clone()),
            "override build must record the override tree's sha, not the live tree's"
        );
        assert_ne!(
            rev_parse_head(&over_probe_root).await,
            Some(live_sha),
            "override probe must NOT return the live tree's sha"
        );
    }

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

    // -----------------------------------------------------------------------
    // S1 — dep-install FRESHNESS gate (lockfile-hash sidecar)
    //
    // Regression class: a reused `.spawn-<ref>` container force-reset to a new
    // ref keeps its `node_modules/` from the PREVIOUS ref. The old
    // presence-gated check ("does node_modules/.bin/ui-bridge-build-ir exist?")
    // said "skip install", so the new ref's TypeScript compiled against the old
    // ref's dependency tree and produced a phantom `TS2339` that looked exactly
    // like a red origin/main.
    // -----------------------------------------------------------------------

    /// Populate `wt_root` as an "already installed" worktree: the pnpm bin
    /// marker + a lockfile + a package.json with the given contents.
    fn seed_installed_worktree(wt_root: &std::path::Path, lockfile: &str, package_json: &str) {
        let bin_dir = wt_root.join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).expect("mkdir node_modules/.bin");
        fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");
        fs::write(wt_root.join("pnpm-lock.yaml"), lockfile).expect("write lockfile");
        fs::write(wt_root.join("package.json"), package_json).expect("write package.json");
    }

    /// Simulate what a successful `pnpm install` leaves behind: the sidecar
    /// recording the dep-manifest hash AT THAT MOMENT. (The async
    /// `write_dep_hash_sidecar` needs a live `SharedState`; the file it writes
    /// is exactly this, so the pure gate is driven directly.)
    fn stamp_dep_hash_sidecar(wt_root: &std::path::Path) {
        let hash = dep_manifest_hash(wt_root).expect("fixture has manifests");
        fs::write(dep_hash_sidecar_path(wt_root), hash).expect("write sidecar");
    }

    #[test]
    fn dep_install_reason_none_when_lockfile_unchanged() {
        // THE skip case: same container, same ref, deps untouched since the
        // last successful install. Must NOT re-pay the ~30s install.
        let tmp = TempDir::new().expect("tempdir");
        seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
        stamp_dep_hash_sidecar(tmp.path());

        assert!(
            dep_install_reason(tmp.path()).is_none(),
            "unchanged lockfile + present marker + matching sidecar must SKIP install"
        );
    }

    #[test]
    fn dep_install_reason_some_when_lockfile_changed() {
        // THE regression: container reused across a ref whose deps moved
        // (`@qontinui/navigation` 0.1.5 → ^0.2.0). `node_modules` and its
        // marker are still there — only the lockfile content changed — so the
        // old presence gate skipped and built against stale deps.
        let tmp = TempDir::new().expect("tempdir");
        seed_installed_worktree(
            tmp.path(),
            "lockfileVersion: 9\n  '@qontinui/navigation': 0.1.5\n",
            r#"{"dependencies":{"@qontinui/navigation":"^0.1.0"}}"#,
        );
        stamp_dep_hash_sidecar(tmp.path());
        assert!(
            dep_install_reason(tmp.path()).is_none(),
            "precondition: the seeded state must be considered fresh"
        );

        // The container is force-reset to a ref with different deps. Only the
        // lockfile/package.json change on disk — node_modules is untouched.
        fs::write(
            tmp.path().join("pnpm-lock.yaml"),
            "lockfileVersion: 9\n  '@qontinui/navigation': 0.2.0\n",
        )
        .expect("rewrite lockfile");

        let reason = dep_install_reason(tmp.path())
            .expect("a changed lockfile MUST force a reinstall, not a stale-node_modules build");
        assert!(
            reason.contains("CHANGED") && reason.contains("STALE"),
            "reason must name the staleness so the log is self-explaining; got: {}",
            reason
        );
    }

    #[test]
    fn dep_install_reason_some_when_package_json_changed_but_lockfile_stale() {
        // A dep pin bumped in package.json without the lockfile regenerated
        // still disagrees with the installed tree (and `--frozen-lockfile`
        // would reject it). Hashing package.json too catches this.
        let tmp = TempDir::new().expect("tempdir");
        seed_installed_worktree(
            tmp.path(),
            "lockfileVersion: 9\n",
            r#"{"dependencies":{"@qontinui/navigation":"^0.1.0"}}"#,
        );
        stamp_dep_hash_sidecar(tmp.path());

        fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"@qontinui/navigation":"^0.2.0"}}"#,
        )
        .expect("rewrite package.json");

        assert!(
            dep_install_reason(tmp.path()).is_some(),
            "a package.json dep change must force a reinstall"
        );
    }

    #[test]
    fn dep_install_reason_some_when_sidecar_absent() {
        // Pre-existing containers (installed before this gate shipped) have
        // node_modules but no sidecar. Provenance unknown ⇒ must NOT be trusted.
        let tmp = TempDir::new().expect("tempdir");
        seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
        // deliberately no stamp

        let reason = dep_install_reason(tmp.path())
            .expect("absent sidecar means unknown node_modules provenance ⇒ reinstall");
        assert!(
            reason.contains("absent"),
            "reason must say the sidecar is absent; got: {}",
            reason
        );
    }

    #[test]
    fn dep_install_reason_some_when_marker_absent_even_if_sidecar_matches() {
        // Someone `rm -rf node_modules/.bin` (or a half-install). A surviving
        // sidecar must never vouch for a tree that isn't installed — the marker
        // check is checked FIRST for exactly this reason.
        let tmp = TempDir::new().expect("tempdir");
        seed_installed_worktree(tmp.path(), "lockfileVersion: 9\n", r#"{"name":"r"}"#);
        stamp_dep_hash_sidecar(tmp.path());
        fs::remove_file(
            tmp.path()
                .join("node_modules")
                .join(".bin")
                .join(ui_bridge_build_ir_bin()),
        )
        .expect("remove marker");

        let reason =
            dep_install_reason(tmp.path()).expect("missing install marker must force a reinstall");
        assert!(
            reason.contains("marker"),
            "reason must name the missing marker; got: {}",
            reason
        );
    }

    #[test]
    fn dep_install_reason_none_when_no_manifests_at_all() {
        // Not a JS project: nothing governs an install, and running one would
        // just fail. Degrade to the legacy marker-presence outcome.
        let tmp = TempDir::new().expect("tempdir");
        let bin_dir = tmp.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");
        fs::write(bin_dir.join(ui_bridge_build_ir_bin()), b"stub").expect("write bin stub");

        assert!(dep_manifest_hash(tmp.path()).is_none());
        assert!(
            dep_install_reason(tmp.path()).is_none(),
            "no manifests + marker present must preserve the legacy skip"
        );
    }

    #[test]
    fn dep_manifest_hash_is_stable_and_content_sensitive() {
        let tmp = TempDir::new().expect("tempdir");
        fs::write(tmp.path().join("pnpm-lock.yaml"), "a").expect("w");
        fs::write(tmp.path().join("package.json"), "b").expect("w");
        let h1 = dep_manifest_hash(tmp.path()).expect("hash");
        let h2 = dep_manifest_hash(tmp.path()).expect("hash");
        assert_eq!(h1, h2, "hash must be deterministic for identical bytes");

        // Removing a lockfile must change the hash (absence is hashed, so a
        // present→absent flip cannot be mistaken for "unchanged").
        fs::remove_file(tmp.path().join("pnpm-lock.yaml")).expect("rm");
        let h3 = dep_manifest_hash(tmp.path()).expect("hash");
        assert_ne!(h1, h3, "removing the lockfile must change the hash");

        // Swapping content between the two files must not collide (name +
        // length are mixed in ahead of the bytes).
        fs::write(tmp.path().join("pnpm-lock.yaml"), "b").expect("w");
        fs::write(tmp.path().join("package.json"), "a").expect("w");
        let h4 = dep_manifest_hash(tmp.path()).expect("hash");
        assert_ne!(h1, h4, "content swap across manifests must not collide");
    }

    // -----------------------------------------------------------------------
    // S2 — a frontend failure must carry the COMPILER error (tsc writes to
    // stdout; the legacy capture read stderr only and came back empty).
    // -----------------------------------------------------------------------

    /// Build a fake finished-process Output with the given streams.
    fn fake_output(stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output {
            // Status is irrelevant to the merge; take a default failure-ish one.
            status: Default::default(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn merge_process_output_keeps_tsc_errors_from_stdout() {
        // The exact shape of the P0: tsc puts `error TS2339` on stdout and
        // leaves stderr EMPTY. Reading stderr alone yields "" — a failed build
        // with no visible reason.
        let out = fake_output(
            "src/nav.tsx(12,7): error TS2339: Property 'hasOwnPage' does not exist on type 'NavigationItem'.\n",
            "",
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).is_empty(),
            "fixture premise: stderr is empty"
        );

        let merged = merge_process_output(&out);
        assert!(
            merged.contains("error TS2339"),
            "merged output MUST carry the tsc error from stdout; got: {:?}",
            merged
        );
        assert!(merged.contains("--- stdout ---"));
        assert!(
            !merged.contains("--- stderr ---"),
            "an empty stream must not add an empty labelled section"
        );
    }

    #[test]
    fn merge_process_output_keeps_both_streams() {
        let merged = merge_process_output(&fake_output("OUT-LINE", "ERR-LINE"));
        assert!(merged.contains("OUT-LINE") && merged.contains("ERR-LINE"));
        assert!(merged.contains("--- stdout ---") && merged.contains("--- stderr ---"));
    }

    #[test]
    fn merge_process_output_empty_when_both_streams_empty() {
        assert!(merge_process_output(&fake_output("", "")).is_empty());
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

    // =====================================================================
    // Phase 2: LKG promotion gate — `update_lkg_after_success` must promote
    // live-tree builds (recording sha + source in lkg.json) and SKIP override
    // builds entirely (exe + sidecar untouched). Root fix for the 2026-06-05
    // incident where a branch build was promoted to LKG and deployed.
    // =====================================================================

    /// Build a `SharedState` whose runner workspace root is a tempdir, so the
    /// LKG dir (`<root>/target-pool/lkg/`) and slot exe
    /// (`<root>/target-pool/slot-0/debug/qontinui-runner.exe`) land under a
    /// throwaway path. `project_dir` is `<root>/src-tauri` because
    /// `runner_npm_dir()` takes its parent. Returns the state plus the
    /// canonicalized workspace root (canonicalized to match `runner_npm_dir`'s
    /// own `canonicalize()`, so the test's path expectations line up).
    fn lkg_test_state(workspace_root: &std::path::Path) -> SharedState {
        let project_dir = workspace_root.join("src-tauri");
        fs::create_dir_all(&project_dir).expect("mkdir src-tauri");
        let config = SupervisorConfig {
            project_dir,
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: 9875,
            dev_logs_dir: workspace_root.join(".dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 8081,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
        };
        Arc::new(SupervisorState::new(config))
    }

    /// Stage a fake slot-0 exe with known bytes so the copy step has something
    /// to promote. Returns the exe path.
    fn stage_slot0_exe(state: &SharedState, bytes: &[u8]) -> std::path::PathBuf {
        let exe = state.config.runner_exe_path_for_slot(0);
        fs::create_dir_all(exe.parent().unwrap()).expect("mkdir slot debug");
        fs::write(&exe, bytes).expect("write slot exe");
        exe
    }

    fn live_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
        BuildProvenance {
            sha: sha.map(str::to_string),
            source: BuildSource::LiveTree,
            built_from: built_from.to_string(),
            built_at: "2026-06-05T00:00:00Z".to_string(),
        }
    }

    fn override_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
        BuildProvenance {
            sha: sha.map(str::to_string),
            source: BuildSource::Override,
            built_from: built_from.to_string(),
            built_at: "2026-06-05T00:00:00Z".to_string(),
        }
    }

    fn origin_main_provenance(sha: Option<&str>, built_from: &str) -> BuildProvenance {
        BuildProvenance {
            sha: sha.map(str::to_string),
            source: BuildSource::OriginMain,
            built_from: built_from.to_string(),
            built_at: "2026-06-07T00:00:00Z".to_string(),
        }
    }

    /// Live-tree build promotes: the LKG exe is written with the slot's bytes
    /// and `lkg.json` records `sha` + `"source":"live_tree"`. Also asserts the
    /// in-memory `last_known_good` lock is populated from the same provenance.
    #[tokio::test]
    async fn live_tree_build_promotes_and_records_provenance() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canon root");
        let state = lkg_test_state(&root);
        stage_slot0_exe(&state, b"fresh-live-tree-bytes");

        let slot = state.build_pool.slots[0].clone();
        let prov = live_provenance(Some("abc123def456"), "/ws/qontinui-runner");

        update_lkg_after_success(&state, &slot, &prov)
            .await
            .expect("live-tree build must promote to LKG");

        // Exe promoted with the slot's bytes.
        let lkg_exe = state.config.lkg_exe_path();
        assert_eq!(
            fs::read(&lkg_exe).expect("read lkg exe"),
            b"fresh-live-tree-bytes",
            "LKG exe must carry the promoted slot bytes"
        );

        // Sidecar carries sha + source from provenance.
        let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
        assert_eq!(meta["sha"], "abc123def456", "lkg.json must record sha");
        assert_eq!(
            meta["source"], "live_tree",
            "lkg.json must record source=live_tree, got {meta_raw}"
        );
        assert_eq!(meta["source_slot"], 0);

        // In-memory lock hydrated from the same provenance.
        let lkg = state.build_pool.last_known_good.read().await.clone();
        let lkg = lkg.expect("last_known_good must be populated after live-tree promote");
        assert_eq!(lkg.sha.as_deref(), Some("abc123def456"));
        assert_eq!(lkg.source, BuildSource::LiveTree);
    }

    /// Live-tree build with a failed git probe (`sha: None`) still promotes;
    /// `lkg.json`'s `sha` serializes as JSON null (honest "unknown SHA"),
    /// `source` is still `live_tree`.
    #[tokio::test]
    async fn live_tree_build_with_null_sha_promotes_with_null_in_sidecar() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canon root");
        let state = lkg_test_state(&root);
        stage_slot0_exe(&state, b"live-no-sha");

        let slot = state.build_pool.slots[0].clone();
        let prov = live_provenance(None, "/ws/qontinui-runner");

        update_lkg_after_success(&state, &slot, &prov)
            .await
            .expect("live-tree build must promote even when sha probe failed");

        let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
        assert!(
            meta["sha"].is_null(),
            "null sha must serialize as JSON null"
        );
        assert_eq!(meta["source"], "live_tree");
    }

    /// Override build does NOT promote: a PRE-EXISTING LKG exe + sidecar are
    /// left byte-for-byte untouched, the in-memory lock is unchanged, and the
    /// call still returns `Ok` (skip is not an error). This is the gate.
    #[tokio::test]
    async fn override_build_does_not_touch_lkg() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canon root");
        let state = lkg_test_state(&root);

        // Pre-seed a prior good LKG (exe + sidecar) so we can prove the
        // override build leaves it intact rather than there simply being
        // nothing to write.
        let lkg_dir = state.config.lkg_dir();
        fs::create_dir_all(&lkg_dir).expect("mkdir lkg");
        let lkg_exe = state.config.lkg_exe_path();
        fs::write(&lkg_exe, b"prior-good-lkg-bytes").expect("seed lkg exe");
        let meta_path = state.config.lkg_metadata_path();
        let prior_meta = r#"{"built_at":"2026-06-01T00:00:00Z","source_slot":2,"exe_size":20,"sha":"prior0000000","source":"live_tree"}"#;
        fs::write(&meta_path, prior_meta).expect("seed lkg.json");

        // Stage a DIFFERENT slot exe that would be promoted if the gate failed.
        stage_slot0_exe(&state, b"foreign-override-bytes");

        let slot = state.build_pool.slots[0].clone();
        let prov = override_provenance(Some("feedface0000"), "/ws/.spawn-feat/qontinui-runner");

        update_lkg_after_success(&state, &slot, &prov)
            .await
            .expect("override build must return Ok (skip, not error)");

        // Exe untouched — still the prior good bytes, NOT the foreign slot exe.
        assert_eq!(
            fs::read(&lkg_exe).expect("read lkg exe"),
            b"prior-good-lkg-bytes",
            "override build must NOT overwrite the LKG exe"
        );
        // Sidecar untouched — byte-for-byte the prior content.
        assert_eq!(
            fs::read_to_string(&meta_path).expect("read lkg.json"),
            prior_meta,
            "override build must NOT rewrite lkg.json"
        );
        // In-memory lock unchanged (still None — we never set it on the prior
        // seed; the gate must not populate it from an override build).
        assert!(
            state.build_pool.last_known_good.read().await.is_none(),
            "override build must NOT populate the last_known_good lock"
        );
    }

    /// Phase B: an `origin_main` build IS promoted to LKG (it is vouched, unlike
    /// `override`). The LKG exe carries the slot bytes and `lkg.json` records
    /// `"source":"origin_main"` + the resolved sha. This is the fix for the
    /// stale-LKG symptom: an origin/main primary build SHOULD advance LKG.
    #[tokio::test]
    async fn origin_main_build_promotes_to_lkg() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canon root");
        let state = lkg_test_state(&root);
        stage_slot0_exe(&state, b"fresh-origin-main-bytes");

        let slot = state.build_pool.slots[0].clone();
        let prov = origin_main_provenance(
            Some("0a1b2c3d4e5f"),
            "/ws/.spawn-origin-main/qontinui-runner",
        );

        update_lkg_after_success(&state, &slot, &prov)
            .await
            .expect("origin/main build must promote to LKG");

        // Exe promoted with the slot's bytes.
        let lkg_exe = state.config.lkg_exe_path();
        assert_eq!(
            fs::read(&lkg_exe).expect("read lkg exe"),
            b"fresh-origin-main-bytes",
            "LKG exe must carry the promoted origin/main slot bytes"
        );

        // Sidecar records sha + source=origin_main from provenance.
        let meta_raw = fs::read_to_string(state.config.lkg_metadata_path()).expect("read lkg.json");
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("parse lkg.json");
        assert_eq!(meta["sha"], "0a1b2c3d4e5f", "lkg.json must record sha");
        assert_eq!(
            meta["source"], "origin_main",
            "lkg.json must record source=origin_main, got {meta_raw}"
        );

        // In-memory lock hydrated from the same provenance.
        let lkg = state.build_pool.last_known_good.read().await.clone();
        let lkg = lkg.expect("last_known_good must be populated after origin/main promote");
        assert_eq!(lkg.sha.as_deref(), Some("0a1b2c3d4e5f"));
        assert_eq!(lkg.source, BuildSource::OriginMain);
    }

    /// The `from_working_tree` flag selects the build's source classification:
    /// the working-tree path uses `BuildSourceKind::LiveTree` ⇒ `live_tree`;
    /// the default origin/main path uses
    /// `BuildSourceKind::OriginMain { resolved_sha }` ⇒ `origin_main`. This is
    /// the pure classification seam the primary rebuild path threads — the kind
    /// alone decides the recorded `BuildSource`, disambiguating an origin/main
    /// primary build from a spawn-test override (both `Some(src_tauri)`).
    #[test]
    fn build_source_kind_classifies_working_tree_vs_origin_main() {
        // from_working_tree:true → live tree.
        assert_eq!(
            BuildSourceKind::LiveTree.build_source(),
            BuildSource::LiveTree
        );
        // from_working_tree:false (default) → origin/main.
        assert_eq!(
            BuildSourceKind::OriginMain {
                resolved_sha: "deadbeef".to_string(),
            }
            .build_source(),
            BuildSource::OriginMain
        );
        // spawn-test foreign override stays Override (unchanged).
        assert_eq!(
            BuildSourceKind::Override.build_source(),
            BuildSource::Override
        );

        // The vouched predicate: working-tree + origin/main promote; override
        // does not.
        assert!(BuildSourceKind::LiveTree.build_source().is_vouched());
        assert!(BuildSourceKind::OriginMain {
            resolved_sha: "x".to_string()
        }
        .build_source()
        .is_vouched());
        assert!(!BuildSourceKind::Override.build_source().is_vouched());
    }

    // ---------- Issue 3: stderr classifier + submission tail ----------

    /// A stderr carrying a real `error[E####]` diagnostic code classifies as a
    /// compiler diagnostic — the user's code is broken, no poisoned-slot retry.
    #[test]
    fn classify_compiler_error_code_is_diagnostic() {
        let stderr = "   Compiling qontinui-runner v0.1.0\n\
             error[E0432]: unresolved import `crate::does_not_exist`\n\
              --> src/main.rs:3:5\n\
             error: aborting due to previous error\n";
        assert_eq!(
            classify_build_stderr(stderr),
            StderrClass::CompilerDiagnostic
        );
    }

    /// Cargo's terminal `could not compile` summary also classifies as a
    /// compiler diagnostic even if the `error[E####]` line was truncated off
    /// the captured tail.
    #[test]
    fn classify_could_not_compile_is_diagnostic() {
        let stderr = "   Compiling qontinui-runner v0.1.0\n\
             error: could not compile `qontinui-runner` (bin \"qontinui-runner\") due to 1 previous error\n";
        assert_eq!(
            classify_build_stderr(stderr),
            StderrClass::CompilerDiagnostic
        );
    }

    /// A failure with ONLY "Compiling …" progress noise + a linker/fingerprint
    /// error and NO compiler diagnostic classifies as environmental — the
    /// poisoned-slot self-heal should fire and retry in a cleaned slot. This is
    /// the exact 2 KB-tail surface the user saw (`Compiling qontinui-runner …`).
    #[test]
    fn classify_environmental_noise_is_environmental() {
        let stderr =
            "   Compiling qontinui-runner v0.1.0 (D:\\qontinui-root\\qontinui-runner\\src-tauri)\n\
             error: linking with `link.exe` failed: exit code: 1104\n\
             LINK : fatal error LNK1104: cannot open file 'qontinui_runner.exe'\n";
        assert_eq!(classify_build_stderr(stderr), StderrClass::Environmental);
    }

    /// A bare `error:` line (no `error[E####]`, no `could not compile`) — e.g. a
    /// "could not find Cargo.toml" environmental failure — must NOT be misread
    /// as a compiler diagnostic, or the self-heal would never fire.
    #[test]
    fn classify_bare_error_line_is_environmental() {
        let stderr = "error: could not find `Cargo.toml` in `/tmp/x` or any parent directory\n";
        assert_eq!(classify_build_stderr(stderr), StderrClass::Environmental);
    }

    /// The submission tail returns the input unchanged when it's under the cap,
    /// and a boundary-safe tail (≤ cap bytes, preserving the END where cargo's
    /// real error lives) when it's over.
    #[test]
    fn stderr_submission_tail_caps_and_keeps_tail() {
        let small = "error[E0277]: trait bound not satisfied";
        assert_eq!(stderr_submission_tail(small), small);

        let big =
            "x".repeat(LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES * 2) + "\nerror[E0599]: tail marker";
        let tail = stderr_submission_tail(&big);
        assert!(tail.len() <= LAST_BUILD_STDERR_SUBMISSION_TAIL_BYTES);
        assert!(
            tail.ends_with("error[E0599]: tail marker"),
            "tail must preserve the END of the stderr where the real error lives"
        );
    }
}

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::config::{RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS, RUNNER_GRACEFUL_STOP_TIMEOUT_MS};
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::env_forwarders;
use crate::process::port::wait_for_port_free;
#[cfg(target_os = "windows")]
use crate::process::windows::{
    kill_by_pid, kill_by_port, remove_runner_app_data_dirs, remove_webview2_user_data_folder,
    webview2_user_data_folder,
};
use crate::state::{ManagedRunner, SharedState};

// =============================================================================
// Runner Category Helpers
// =============================================================================

/// Classify a runner from its supervisor-assigned id.
///
/// Single source of truth for the prefix scheme — see
/// [`qontinui_types::wire::runner_kind::RunnerKind`] for the full mapping
/// and `routes::runners` for where the ids are constructed.
///
/// Note: this drops back to [`RunnerKind::from_id`] verbatim and exists
/// primarily to give callers a stable supervisor-side import path. For
/// classification that needs the user-friendly display name, prefer
/// [`RunnerConfig::kind`] which can mirror it from `RunnerConfig.name`.
#[allow(dead_code)] // Item 2: helper exposed for follow-up migration of `is_primary` checks.
pub fn runner_kind(runner_id: &str) -> qontinui_types::wire::runner_kind::RunnerKind {
    qontinui_types::wire::runner_kind::RunnerKind::from_id(runner_id)
}

/// Returns true if this runner is a temp/test runner managed by the supervisor.
/// Only temp runners can be started, stopped, or restarted by the supervisor.
/// All other runners (primary, user-opened) are observe-only.
///
/// Thin wrapper over [`runner_kind`] — kept as a standalone helper because
/// the boolean form is the most common predicate in the supervisor and
/// avoids a `match` ceremony at every call site. Migrating call sites to
/// `match runner_kind(id) { RunnerKind::Temp { .. } => ... }` is a
/// follow-up; out of scope for Item 2.
pub fn is_temp_runner(runner_id: &str) -> bool {
    runner_kind(runner_id).is_temp()
}

/// Binary metadata for diagnostics — lets callers detect stale binaries.
#[derive(Clone, serde::Serialize)]
pub struct BinaryMeta {
    pub binary_mtime: String,
    pub binary_size_bytes: u64,
}

/// Read mtime + size of a binary file.
pub fn binary_meta(path: &std::path::Path) -> Option<BinaryMeta> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
    let mtime_str = dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Some(BinaryMeta {
        binary_mtime: mtime_str,
        binary_size_bytes: meta.len(),
    })
}

// =============================================================================
// Stale-binary detection (Phase 2c — Item 9)
// =============================================================================

/// Minimum `slot_mtime - running_mtime` gap (in seconds) before we surface a
/// `stale_binary` entry. Tuned to absorb filesystem mtime resolution jitter
/// and near-simultaneous builds that racily complete around a running-runner
/// start. Anything finer than ~30s is not actionable ("rebuild now to pick it
/// up") because a user-issued restart at t=0 routinely reads a slot binary
/// stamped t+2s from the same cargo invocation. 30s keeps the badge meaningful.
pub const STALE_BINARY_THRESHOLD_SECS: i64 = 30;

/// Per-runner "newer build available" summary surfaced on `/runners` and
/// `/runners/{id}/logs`. `None` is the normal case (running binary is newer
/// than or equal to the newest slot, within the 30s jitter threshold).
#[derive(Clone, serde::Serialize)]
pub struct StaleBinary {
    /// Unix millis of the copy the supervisor made at start time
    /// (`target/debug/qontinui-runner-<id>.exe`).
    pub running_mtime_ms: i64,
    /// Unix millis of the newest `target-pool/slot-*/debug/qontinui-runner.exe`.
    pub slot_mtime_ms: i64,
    /// Which slot holds the newer build.
    pub slot_id: u8,
    /// `slot_mtime - running_mtime` in whole seconds. Always positive when
    /// surfaced — the field is `None` when the running binary is newer.
    pub age_delta_secs: i64,
}

/// Stat the supervisor's per-runner exe copy and return its mtime.
///
/// Returns `None` when the copy does not exist yet (runner never started under
/// this supervisor, or the path resolver failed to copy). The live path is
/// `target/debug/qontinui-runner-<id>.exe` — the same path set up by
/// `start_exe_mode_for_runner` before spawning the child.
pub fn running_binary_mtime(state: &SharedState, runner_id: &str) -> Option<std::time::SystemTime> {
    let path = state.config.runner_exe_copy_path(runner_id);
    std::fs::metadata(&path).ok()?.modified().ok()
}

/// Scan every `target-pool/slot-*/debug/qontinui-runner.exe` and return the
/// `(slot_id, mtime)` of the newest. Returns `None` when the pool has never
/// produced a binary yet.
pub async fn newest_slot_binary_mtime(state: &SharedState) -> Option<(u8, std::time::SystemTime)> {
    let mut best: Option<(u8, std::time::SystemTime)> = None;
    for slot in &state.build_pool.slots {
        let path = slot.target_dir.join("debug").join("qontinui-runner.exe");
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let slot_id_u8: u8 = slot.id.min(u8::MAX as usize) as u8;
        best = match best {
            Some((_, current)) if current >= mtime => best,
            _ => Some((slot_id_u8, mtime)),
        };
    }
    best
}

/// Convert a `SystemTime` to unix millis, saturating at i64 bounds. Values
/// predating the epoch return a negative ms count (shouldn't happen for
/// filesystem mtimes on sane clocks, but defined for test-fixture ergonomics).
fn system_time_to_unix_millis(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Compute a `StaleBinary` record from the raw mtimes. Pure function — the
/// actual `SystemTime` lookups live in `running_binary_mtime` /
/// `newest_slot_binary_mtime` so this is trivially testable.
///
/// Returns `Some` only when the newest slot binary is strictly newer than the
/// running copy by more than `STALE_BINARY_THRESHOLD_SECS`. Equal or
/// within-threshold deltas yield `None` (normal state — restart is a no-op
/// from a binary-freshness perspective).
pub fn compute_stale_binary(
    running: Option<std::time::SystemTime>,
    newest_slot: Option<(u8, std::time::SystemTime)>,
) -> Option<StaleBinary> {
    let running = running?;
    let (slot_id, slot_mtime) = newest_slot?;
    // Compute the delta in whole seconds. `duration_since` errors when the
    // left side predates the right (i.e. running > slot) — that's the "not
    // stale" case. Ignore it and return `None`.
    let delta_secs = slot_mtime.duration_since(running).ok()?.as_secs() as i64;
    if delta_secs <= STALE_BINARY_THRESHOLD_SECS {
        return None;
    }
    Some(StaleBinary {
        running_mtime_ms: system_time_to_unix_millis(running),
        slot_mtime_ms: system_time_to_unix_millis(slot_mtime),
        slot_id,
        age_delta_secs: delta_secs,
    })
}

/// Convenience wrapper: look up the runner's running copy + newest slot and
/// call `compute_stale_binary`. Returns `None` on any I/O miss — callers
/// treat the field as strictly informational.
pub async fn stale_binary_for_runner(state: &SharedState, runner_id: &str) -> Option<StaleBinary> {
    let running = running_binary_mtime(state, runner_id);
    let newest_slot = newest_slot_binary_mtime(state).await;
    compute_stale_binary(running, newest_slot)
}

/// Resolve the last-known-good runner exe path.
///
/// Returns the path only when both the on-disk LKG exe AND in-memory
/// `LkgInfo` are present — callers that pin a runner to LKG need the
/// metadata (notably `built_at`) to make their staleness decision, so a
/// dangling exe with no sidecar is treated as absent.
///
/// If the on-disk exe has gone missing while the in-memory `LkgInfo` is
/// still populated (e.g. the user wiped `target-pool/lkg/` between builds,
/// or a subsequent rename never landed), the stale `LkgInfo` is cleared
/// before returning the error so `/health.build.lkg` no longer reports
/// metadata for an exe that doesn't exist.
pub async fn resolve_lkg_exe(state: &SharedState) -> Result<std::path::PathBuf, SupervisorError> {
    let info_present = state.build_pool.last_known_good.read().await.is_some();
    if !info_present {
        return Err(SupervisorError::Process(
            "No last-known-good runner binary recorded yet. Run a build that succeeds first."
                .to_string(),
        ));
    }
    let p = state.config.lkg_exe_path();
    if !p.exists() {
        // Drop the stale in-memory entry so /health and /builds stop
        // reporting metadata for an exe that's no longer on disk.
        let mut guard = state.build_pool.last_known_good.write().await;
        *guard = None;
        return Err(SupervisorError::Process(format!(
            "LKG metadata is set but exe is missing at {:?}. The LKG dir may have been wiped; rebuild to repopulate.",
            p
        )));
    }
    Ok(p)
}

/// Locate the most recent successfully-built runner exe across the build pool.
///
/// Preference order:
/// 1. The exe in the slot recorded as `last_successful_slot` (fresh build).
/// 2. Any slot whose exe exists on disk (e.g. after a supervisor restart).
/// 3. The legacy `runner_exe_path()` (default `target/debug/`) for builds
///    that predate the build pool.
pub async fn resolve_source_exe(
    state: &SharedState,
) -> Result<std::path::PathBuf, SupervisorError> {
    // Preference 1: last successful slot.
    if let Some(slot_id) = *state.build_pool.last_successful_slot.read().await {
        let p = state.config.runner_exe_path_for_slot(slot_id);
        if p.exists() {
            return Ok(p);
        }
    }

    // Preference 2: scan all slots for any existing exe (supervisor restart case).
    for slot in &state.build_pool.slots {
        let p = slot.target_dir.join("debug").join("qontinui-runner.exe");
        if p.exists() {
            return Ok(p);
        }
    }

    // Preference 3: legacy path for pre-pool builds.
    let legacy = state.config.runner_exe_path();
    if legacy.exists() {
        return Ok(legacy);
    }

    Err(SupervisorError::Process(format!(
        "Runner exe not found in any build slot or at legacy path {:?}. Run a build first.",
        legacy
    )))
}

// =============================================================================
// Startup Cleanup
// =============================================================================

/// Kill any orphaned temp runner processes AND remove stale registry entries
/// from previous supervisor sessions.
/// Only cleans up temp runner ports — user runners are never touched.
pub async fn cleanup_orphaned_runners(state: &SharedState) {
    let runners = state.get_all_runners().await;

    // Collect temp runner ports (to kill processes) and stale IDs (to remove from registry).
    let mut ports: Vec<u16> = Vec::new();
    let mut stale_ids: Vec<String> = Vec::new();
    for r in &runners {
        if is_temp_runner(&r.config.id) {
            ports.push(r.config.port);
            // On startup, ALL pre-existing test runners are stale — they're
            // leftovers from a previous supervisor session.  Remove them from
            // the registry after killing their processes.
            stale_ids.push(r.config.id.clone());
        } else {
            // Mark non-temp runners as running if the HTTP /health endpoint
            // responds, so the supervisor tracks their health without managing
            // them. We probe HTTP rather than just TCP here because a stale
            // socket left behind by a just-killed runner can make the TCP
            // check return true for several seconds — that false positive
            // used to leave the primary stuck as `running=true, pid=null`
            // and prevented manual restart from being triggered on boot.
            if crate::process::port::is_runner_responding(r.config.port).await {
                info!(
                    "Runner '{}' (port {}) already running — tracking health only",
                    r.config.name, r.config.port
                );
                let mut runner = r.runner.write().await;
                runner.running = true;
            } else if crate::process::port::is_port_in_use(r.config.port) {
                warn!(
                    "Runner '{}' port {} is occupied but /health is not responding — \
                     treating as offline (likely a stale socket from a just-killed process)",
                    r.config.name, r.config.port
                );
            }
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut killed_any = false;
    #[cfg(target_os = "windows")]
    for &port in &ports {
        if let Ok(true) = kill_by_port(port).await {
            info!("Killed orphaned temp runner on port {}", port);
            killed_any = true;
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = ports;

    // Remove stale test runner entries from the in-memory registry.
    if !stale_ids.is_empty() {
        let mut runners_map = state.runners.write().await;
        for id in &stale_ids {
            runners_map.remove(id);
        }
        info!(
            "Purged {} stale test runner entries from registry on startup",
            stale_ids.len()
        );
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
// Periodic Stale Test Runner Reaper
// =============================================================================

/// Background task that periodically detects and removes stopped/crashed test
/// runners from the in-memory registry. Runs every 5 minutes.
///
/// A test runner is considered stale if:
///   - Its `running` flag is false, OR
///   - Its `running` flag is true but nothing is listening on its port (crash).
pub async fn reap_stale_test_runners(state: SharedState) {
    const INTERVAL: Duration = Duration::from_secs(5 * 60);
    // Wait a bit on startup to let normal init complete
    tokio::time::sleep(Duration::from_secs(30)).await;

    loop {
        tokio::time::sleep(INTERVAL).await;
        let runners = state.get_all_runners().await;
        let mut reaped = 0u32;

        for managed in &runners {
            if !is_temp_runner(&managed.config.id) {
                continue;
            }
            // Skip runners created less than 2 minutes ago — they may still
            // be in the build+start pipeline (spawn_test inserts a placeholder
            // with running=false before the build completes).
            if managed.created_at.elapsed() < Duration::from_secs(120) {
                continue;
            }
            let is_running = {
                let runner = managed.runner.read().await;
                runner.running
            };
            if is_running {
                if crate::process::port::is_port_in_use(managed.config.port) {
                    continue; // genuinely alive
                }
                // Port free but state says running — crashed
                {
                    let mut runner = managed.runner.write().await;
                    runner.running = false;
                    runner.pid = None;
                }
            }

            let id = managed.config.id.clone();
            let name = managed.config.name.clone();
            let port = managed.config.port;

            #[cfg(target_os = "windows")]
            let _ = kill_by_port(port).await;

            // Preserve the runner's logs in the stopped-runners cache before
            // dropping its ManagedRunner so post-mortem debugging still works
            // via `GET /runners/{id}/logs?include_stopped=true`.
            let snapshot = crate::process::stopped_cache::snapshot_from_managed(
                managed,
                None,
                crate::process::stopped_cache::StopReason::Reaped,
            )
            .await;
            {
                let mut cache = state.stopped_runners.write().await;
                crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
            }

            {
                let mut runners_map = state.runners.write().await;
                runners_map.remove(&id);
            }

            #[cfg(windows)]
            {
                let _ = remove_webview2_user_data_folder(&id, false).await;
                let _ = remove_runner_app_data_dirs(&name, false).await;
            }

            info!(
                "reaper: removed stale test runner '{}' (port {})",
                name, port
            );
            reaped += 1;
        }

        if reaped > 0 {
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Info,
                    format!("Reaper: purged {} stale test runner(s)", reaped),
                )
                .await;
        }
    }
}

// =============================================================================
// Per-Runner Process Management (multi-runner)
// =============================================================================

// Per-runner env forwarders moved to `process::env_forwarders`. See
// [`crate::process::env_forwarders::EnvForwarder`] and
// [`crate::process::env_forwarders::default_env_forwarders`]. Every spawned
// runner runs the same registered list once in `start_exe_mode_for_runner`,
// replacing the previous five hand-written `forward_*_env` functions and
// the duplicated cfg(windows) / cfg(not(windows)) call-site chains.

/// Start a specific runner by ID.
///
/// Thin wrapper around [`start_managed_runner`] that first resolves the id in
/// the registry. Prefer `start_managed_runner` when the caller already holds
/// an `Arc<ManagedRunner>` — that path is race-free, whereas id-based lookup
/// can fail if a concurrent remove (reaper, stop, failed probe) fires between
/// insertion and start.
pub async fn start_runner_by_id(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;
    start_managed_runner(state, &managed).await
}

/// Start a runner given a direct `Arc<ManagedRunner>` reference.
///
/// Used by `spawn_test` / `spawn_named` to avoid a re-lookup race: the
/// registry insertion and the start must use the same ManagedRunner, even if
/// another task concurrently removes the id from the map. If the id is
/// missing from the registry when we start (which shouldn't normally happen,
/// but has been observed as a transient 404 under load), we re-insert the Arc
/// so downstream health / monitoring can find it by id.
pub async fn start_managed_runner(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
) -> Result<(), SupervisorError> {
    // With the parallel build pool, a concurrent build on one slot does not
    // prevent us from starting a runner from a previously-built exe in
    // another slot (or the legacy target path). `resolve_source_exe` inside
    // `start_exe_mode_for_runner` returns an explicit error if no binary is
    // available anywhere. No coarse `build_in_progress` check here.

    let runner_id = managed.config.id.clone();

    // Defensive re-insertion: if something removed our id between placeholder
    // insertion and start, put it back. This fixes the ~1-in-10 spawn-test
    // 404 "Runner not found" race observed in smoke tests. Using `entry` +
    // `or_insert` instead of unconditional insert preserves any other Arc
    // that may have replaced ours (so we don't clobber a different managed
    // runner sharing the same id, which would itself indicate a bug).
    {
        let mut runners = state.runners.write().await;
        runners
            .entry(runner_id.clone())
            .or_insert_with(|| managed.clone());
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
            format!("Starting runner '{}' (port {})", runner_name, port),
        )
        .await;

    let SpawnResult {
        mut child,
        panic_log_dir,
    } = start_exe_mode_for_runner(state, managed).await?;

    let pid = child.id();
    info!(
        "Runner '{}' started with PID {:?} on port {}",
        runner_name, pid, port
    );

    // Assign the spawned process to the supervisor's kill-on-exit JobObject.
    // When the supervisor process dies (graceful exit, panic, force-kill, or
    // BSOD), the kernel closes the last handle to the job and terminates
    // every assigned process per `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    // WebView2 children of the runner are transitively in the job too —
    // Windows assigns child processes of a job-tracked process to the same
    // job by default.
    //
    // We assign AFTER `cmd.spawn()` because the runner is not started with
    // `CREATE_SUSPENDED`, so it's already executing. That's fine for
    // KILL_ON_JOB_CLOSE — the only correctness trap with post-spawn
    // assignment is BREAKAWAY_OK interactions where a child could escape
    // before assignment, which we don't rely on here.
    //
    // Assignment failure is loud but non-fatal — the runner is functional;
    // it just won't be auto-killed if the supervisor dies abruptly.
    if let (Some(job), Some(pid_val)) = (state.runner_job.as_ref(), pid) {
        match job.assign(pid_val) {
            Ok(()) => {
                let msg = format!(
                    "Assigned runner '{}' (PID {}) to kill-on-exit JobObject",
                    runner_name, pid_val
                );
                info!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Info, msg)
                    .await;
            }
            Err(e) => {
                warn!(
                    "Failed to assign runner '{}' (PID {}) to kill-on-exit JobObject: {}. \
                     Supervisor exit will not terminate this runner.",
                    runner_name, pid_val, e
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Runner '{}' (PID {}) NOT assigned to kill-on-exit JobObject: {}. \
                             If the supervisor crashes, this runner may linger as an orphan.",
                            runner_name, pid_val, e
                        ),
                    )
                    .await;
            }
        }
    }

    // Remember the panic log dir so `monitor_runner_process_exit` can find
    // the file after a non-zero exit. Also clear any stale `recent_panic`
    // left over from a previous boot of this runner id — a clean start
    // should not continue surfacing an old panic in the runner list.
    {
        let mut slot = managed.panic_log_dir.write().await;
        *slot = panic_log_dir.clone();
    }
    {
        let mut slot = managed.recent_panic.write().await;
        *slot = None;
    }

    // Open the per-spawn early-death log file BEFORE attaching readers.
    // `spawn_stdout_reader` / `spawn_stderr_reader` snapshot the writer at
    // spawn time, so this must precede them. If the file can't be opened
    // (out of disk space, perms, etc.) the runner still starts — early-log
    // capture is strictly best-effort. Drops any path stored from a prior
    // start of this runner id.
    let early_log_path = crate::process::early_log::early_log_dir()
        .map(|dir| crate::process::early_log::early_log_path_for(&dir, &runner_id));
    if let Some(ref path) = early_log_path {
        match crate::process::early_log::EarlyLogWriter::open(path) {
            Some(writer) => {
                managed.logs.set_early_log_writer(Some(writer));
                let mut slot = managed.early_log_path.write().await;
                *slot = Some(path.clone());
                debug!(
                    "Early-log capture enabled for runner '{}' at {:?}",
                    runner_name, path
                );
            }
            None => {
                // Couldn't open the file — clear any prior path so we don't
                // surface a stale value via the API.
                managed.logs.set_early_log_writer(None);
                let mut slot = managed.early_log_path.write().await;
                *slot = None;
            }
        }
    } else {
        managed.logs.set_early_log_writer(None);
        let mut slot = managed.early_log_path.write().await;
        *slot = None;
    }

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
    tokio::spawn(async move {
        monitor_runner_process_exit(state_clone, managed_clone, runner_id).await;
    });

    // Spawn the first-healthy watchdog so a child that spawns but never
    // binds its HTTP API is killed instead of lingering as a zombie.
    if let Some(pid_val) = pid {
        let state_clone = state.clone();
        let managed_clone = managed.clone();
        tokio::spawn(async move {
            watch_first_healthy(state_clone, managed_clone, pid_val).await;
        });
    }

    Ok(())
}

/// Result of spawning a runner in exe mode.
struct SpawnResult {
    child: tokio::process::Child,
    /// Directory the supervisor told the runner to write its panic log to
    /// via `QONTINUI_RUNNER_LOG_DIR`. `None` when the supervisor deferred
    /// to the runner's default path (no `--log-dir` configured).
    panic_log_dir: Option<std::path::PathBuf>,
}

/// Start exe mode for a specific runner with port/name env vars.
async fn start_exe_mode_for_runner(
    state: &SharedState,
    managed: &ManagedRunner,
) -> Result<SpawnResult, SupervisorError> {
    // Locate the source exe. The per-runner `source_exe_override` takes
    // precedence — set by `spawn_test` when the caller passes `use_lkg: true`
    // so the runner is pinned to the last-known-good binary regardless of
    // current slot state. With no override, fall back to the parallel build
    // pool: each slot builds into its own `target-pool/slot-{k}/debug/`.
    // Prefer the slot that produced the most recent successful build; then
    // any slot with an exe on disk; then the legacy single-target path for
    // cases where no parallel build has run yet (e.g. pre-pool-era builds
    // or manual `cargo build` invocations).
    let source_exe = {
        let override_path = managed.source_exe_override.read().await.clone();
        match override_path {
            Some(p) if p.exists() => {
                info!(
                    "Runner '{}' pinned to source exe override {:?}",
                    managed.config.name, p
                );
                p
            }
            Some(p) => {
                // Hard-fail: the caller explicitly pinned this runner to a
                // specific binary (typically the LKG via spawn-test
                // {use_lkg: true}). Silently falling back to slot resolution
                // would launch a different binary while the response keeps
                // claiming `used_lkg: true`, which is exactly the kind of
                // staleness the LKG path is meant to *prevent*.
                return Err(SupervisorError::Process(format!(
                    "Runner '{}' was pinned to source exe override {:?} but the file is missing. The LKG dir may have been wiped between the spawn-time check and process start; rebuild to repopulate.",
                    managed.config.name, p
                )));
            }
            None => resolve_source_exe(state).await?,
        }
    };

    // All runners use a copy of the exe to avoid locking the build artifact.
    // This allows cargo build to succeed while any runner is running.
    //
    // The first copy can fail when a previous instance of this runner died
    // with the supervisor losing its PID — Windows will hold the prior copy
    // open until the OS releases the handle. Try to remove the stale copy
    // and retry once. If that still fails, fail the spawn rather than fall
    // back to running directly from `source_exe` (the slot binary).
    //
    // Why we never fall back to source_exe: it leaves a process running
    // out of `target-pool/slot-{k}/debug/qontinui-runner.exe`, locking the
    // slot for every future cargo build. If the supervisor then loses the
    // PID, the slot becomes permanently unbuildable until the OS process
    // is killed externally — exactly the deadlock this code is meant to
    // prevent. A clean failure here surfaces the underlying problem
    // (locked previous copy, disk full, AV) instead of silently producing
    // a worse failure mode later.
    let exe_path = {
        let copy_path = state.config.runner_exe_copy_path(&managed.config.id);
        match std::fs::copy(&source_exe, &copy_path) {
            Ok(_) => {
                info!(
                    "Copied runner exe for '{}' to {:?}",
                    managed.config.name, copy_path
                );
                copy_path
            }
            Err(first_err) => {
                warn!(
                    "Initial copy of runner exe for '{}' failed: {} — \
                     attempting to remove stale copy and retry",
                    managed.config.name, first_err
                );
                let _ = std::fs::remove_file(&copy_path);
                match std::fs::copy(&source_exe, &copy_path) {
                    Ok(_) => {
                        info!(
                            "Copied runner exe for '{}' to {:?} on retry",
                            managed.config.name, copy_path
                        );
                        copy_path
                    }
                    Err(retry_err) => {
                        return Err(SupervisorError::Process(format!(
                            "Failed to copy runner exe for '{}' from {:?} to {:?}: \
                             initial error: {}; retry error: {}. \
                             Refusing to run directly from the build slot — that \
                             would lock the slot for future builds. Resolve the \
                             copy-target lock (likely a prior runner instance the \
                             supervisor lost track of) and retry.",
                            managed.config.name, source_exe, copy_path, first_err, retry_err
                        )));
                    }
                }
            }
        }
    };

    info!(
        "Starting runner '{}' in exe mode from {:?} on port {}",
        managed.config.name, exe_path, managed.config.port
    );

    let mut cmd = Command::new(&exe_path);
    cmd.current_dir(&state.config.project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE")
        .env("QONTINUI_PORT", managed.config.port.to_string())
        .env("QONTINUI_API_URL", "http://127.0.0.1:8000");

    // Windows-only creation flags: detach from console (no flash window) +
    // own process group (so the supervisor can send Ctrl-Break for graceful
    // shutdown without killing siblings).
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // Inline non-forwarder env vars. Test-auto-login credentials are pulled
    // by `TestAutoLoginEnv` below and apply to every supervisor-spawned
    // runner — primary included — for the rationale documented on the
    // forwarder type.
    //
    // Non-primary runners additionally get `QONTINUI_INSTANCE_NAME` to skip
    // the scheduler and `QONTINUI_PRIMARY_PORT` so they can proxy process
    // commands to the primary.
    if !managed.config.is_primary {
        cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
        // Find the primary runner's port for process log proxying.
        //
        // The user-started primary isn't in the supervisor's runners
        // registry (the supervisor only tracks runners IT spawned),
        // so `state.get_primary()` returns None on most setups. Fall
        // back to the conventional default port — the runner's
        // `process_capture::primary_proxy::is_secondary()` requires
        // BOTH env vars to be set, so leaving the port unset would
        // cause every secondary to silently behave as a primary
        // (re-introducing the wrappers-dir contention this var was
        // added to fix).
        let primary_port = state
            .get_primary()
            .await
            .map(|p| p.config.port)
            .unwrap_or(crate::config::DEFAULT_RUNNER_API_PORT);
        cmd.env("QONTINUI_PRIMARY_PORT", primary_port.to_string());

        // Per-runner WebView2 data dir — non-primary runners get isolated
        // localStorage, IndexedDB, cookies, and caches. Primary keeps the
        // default path so its existing state (auth, terminal layouts, etc.)
        // is preserved. This prevents state bleed-over when spawning temp
        // test runners and eliminates the "216 restored terminals" problem
        // where one runner's persisted UI state floods every other runner.
        // On non-Windows the variable is ignored by other webview backends,
        // so this is harmless but keeps behavior consistent.
        #[cfg(target_os = "windows")]
        if let Some(webview_dir) = webview2_user_data_folder(&managed.config.id, false) {
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

    // Apply the registered env forwarders. Order is load-bearing — see
    // `process::env_forwarders` for the per-forwarder rationale. Adding a
    // new forwarder is one struct + one registration line in
    // `default_env_forwarders`, replacing the previous five-place edit
    // (forwarder fn + two cfg-gated call sites + state.rs storage).
    for forwarder in env_forwarders::default_env_forwarders() {
        debug!(
            "applying env forwarder '{}' for runner '{}'",
            forwarder.name(),
            managed.config.name
        );
        forwarder.apply(&mut cmd, state, managed).await;
    }

    // `PanicLogEnv` stashed the resolved per-runner panic-log path on
    // `managed.panic_log_dir` while applying — read it back so
    // `monitor_runner_process_exit` can find `runner-panic.log` after a
    // non-zero exit. Cloning out keeps the lock held for the minimum span.
    let panic_log_dir = managed.panic_log_dir.read().await.clone();

    let child = cmd
        .spawn()
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?;

    Ok(SpawnResult {
        child,
        panic_log_dir,
    })
}

/// Default deadline for a newly-spawned runner to bind its HTTP API
/// before the supervisor declares the spawn a failure and kills the PID.
/// Override via `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS`.
const DEFAULT_FIRST_HEALTHY_TIMEOUT_SECS: u64 = 90;
const FIRST_HEALTHY_POLL_INTERVAL_SECS: u64 = 3;

fn first_healthy_timeout_secs() -> u64 {
    std::env::var("QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_FIRST_HEALTHY_TIMEOUT_SECS)
}

/// Outcome of one poll tick of the first-healthy watchdog. Extracted as a
/// pure decision so the priority rules can be asserted by unit tests
/// without spinning up a process, HTTP server, or SharedState.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstHealthyDecision {
    /// Exit quietly — the exit monitor already reaped the process.
    Abandon,
    /// HTTP /health responded; record success and exit.
    Healthy,
    /// Deadline passed and still no /health response; kill the PID.
    Kill,
    /// None of the above; sleep one poll interval and retry.
    Wait,
}

/// Decide what the watchdog should do this tick. Priority is intentional:
///   1. Abandon — process is gone, nothing we should do to it.
///   2. Healthy — responding wins even if the deadline just passed
///      (avoids a pointless kill on a runner that made it just in time).
///   3. Kill — deadline passed and still unresponsive.
///   4. Wait — not yet past deadline, keep polling.
fn decide_first_healthy(
    still_tracked: bool,
    api_responding: bool,
    deadline_passed: bool,
) -> FirstHealthyDecision {
    if !still_tracked {
        return FirstHealthyDecision::Abandon;
    }
    if api_responding {
        return FirstHealthyDecision::Healthy;
    }
    if deadline_passed {
        return FirstHealthyDecision::Kill;
    }
    FirstHealthyDecision::Wait
}

/// Watchdog for a newly-spawned runner. If its HTTP API doesn't respond
/// within `first_healthy_timeout_secs()`, the process is considered
/// wedged (alive but hung during startup — e.g. stuck on a DDL, on
/// WebView2 init, or inside a subprocess spawn) and the PID is killed.
/// `monitor_runner_process_exit` observes the resulting exit and cleans
/// up runner state naturally.
///
/// Scope: runs once per supervisor-initiated start. Does not auto-restart
/// and does not touch runners started outside the supervisor.
async fn watch_first_healthy(state: SharedState, managed: Arc<ManagedRunner>, pid: u32) {
    let timeout_secs = first_healthy_timeout_secs();
    let runner_name = managed.config.name.clone();
    let port = managed.config.port;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let poll = Duration::from_secs(FIRST_HEALTHY_POLL_INTERVAL_SECS);

    loop {
        let still_tracked = {
            let runner = managed.runner.read().await;
            runner.pid == Some(pid) && runner.running
        };
        let api_responding = if still_tracked {
            crate::process::port::is_runner_responding(port).await
        } else {
            false
        };
        let deadline_passed = tokio::time::Instant::now() >= deadline;

        match decide_first_healthy(still_tracked, api_responding, deadline_passed) {
            FirstHealthyDecision::Abandon => {
                debug!(
                    "First-healthy watchdog for runner '{}' (PID {}) exiting — process no longer tracked",
                    runner_name, pid
                );
                return;
            }
            FirstHealthyDecision::Healthy => {
                info!(
                    "Runner '{}' (PID {}) HTTP API responsive — first-healthy watchdog clear",
                    runner_name, pid
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "Runner '{}' healthy within first-healthy budget",
                            runner_name
                        ),
                    )
                    .await;
                return;
            }
            FirstHealthyDecision::Kill => {
                let msg = format!(
                    "Runner '{}' (PID {}) did not bind HTTP API within {}s — killing wedged process",
                    runner_name, pid, timeout_secs
                );
                error!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Error, msg)
                    .await;

                #[cfg(target_os = "windows")]
                {
                    match crate::process::windows::kill_by_pid(pid).await {
                        Ok(true) => info!(
                            "First-healthy watchdog killed wedged runner '{}' PID {}",
                            runner_name, pid
                        ),
                        Ok(false) => warn!(
                            "First-healthy watchdog: PID {} for runner '{}' no longer present",
                            pid, runner_name
                        ),
                        Err(e) => error!(
                            "First-healthy watchdog: failed to kill PID {} for runner '{}': {}",
                            pid, runner_name, e
                        ),
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    warn!(
                        "First-healthy watchdog: kill_by_pid not implemented on non-Windows; \
                         leaving wedged runner '{}' PID {} in place",
                        runner_name, pid
                    );
                }
                return;
            }
            FirstHealthyDecision::Wait => {
                tokio::time::sleep(poll).await;
            }
        }
    }
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

        // If the process died non-zero, look for a startup-panic log. A
        // panic that fires during early init (DB connect, Tauri builder,
        // axum router construction) doesn't flow through stderr in a
        // shape our buffered reader can latch onto, so this file is the
        // only place the panic payload actually lives.
        if !status.success() {
            check_and_record_panic_log(&state, &managed, &runner_name).await;
        }
    } else {
        let msg = format!("Runner '{}' process terminated unexpectedly", runner_name);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, &msg)
            .await;
        warn!("{}", msg);
        // Also check on unexpected termination — the child is gone either
        // way, and if a panic log exists within the freshness window it's
        // almost certainly the cause.
        check_and_record_panic_log(&state, &managed, &runner_name).await;
    }
}

/// Look for `<panic_log_dir>/runner-panic.log`; if it exists and its
/// timestamp is within [`PANIC_LOG_FRESHNESS_SECS`] of now, parse it and
/// stash it on the managed runner so `GET /runners` can surface it. Also
/// emit a tagged `[runner-panic]` ERROR into the supervisor log buffer.
///
/// All errors are swallowed at debug level — panic telemetry is strictly
/// best-effort and must never interfere with normal process-exit handling.
async fn check_and_record_panic_log(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    runner_name: &str,
) {
    let dir_opt = managed.panic_log_dir.read().await.clone();
    let path = crate::process::panic_log::resolve_panic_log_path(dir_opt.as_deref());

    let Some(parsed) = crate::process::panic_log::parse_panic_file(&path) else {
        debug!(
            "No panic log found for runner '{}' at {:?}",
            runner_name, path
        );
        return;
    };

    // Freshness gate — a stale file from a previous boot shouldn't be
    // attributed to the exit we just observed.
    let now = chrono::Utc::now();
    if !crate::process::panic_log::is_fresh(&parsed, now) {
        debug!(
            "Panic log at {:?} is stale (timestamp {} vs now {}) — ignoring",
            path, parsed.timestamp, now
        );
        return;
    }

    let location_str = parsed.location.as_deref().unwrap_or("<unknown>");
    let payload_preview: String = parsed.payload.chars().take(500).collect();
    let backtrace_preview = parsed.backtrace_preview.as_deref().unwrap_or("");
    let msg = format!(
        "[runner-panic] Runner '{}' panicked during startup at {}:\n{}\n{}",
        runner_name, location_str, payload_preview, backtrace_preview,
    );
    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Error, msg.clone())
        .await;
    error!("{}", msg);

    // Stash on the managed runner for JSON surfacing. The reaper may
    // later drop the runner from the registry — that's fine, callers
    // passing `?include_stopped=true` to the logs endpoint see the
    // panic via the stopped-cache snapshot once we extend that path.
    let mut slot = managed.recent_panic.write().await;
    *slot = Some(parsed);
}

/// POST to the runner's UI Bridge close-request endpoint so that
/// Tauri's WindowEvent::CloseRequested fires on the runner side and its
/// graceful teardown hooks run (e.g. UsbTransport::release_all, which
/// removes adb forwards). Best-effort: any error — including a hung
/// endpoint — is swallowed at debug level, and the caller falls through
/// to child.kill() after the wait window elapses.
async fn request_graceful_stop(state: &SharedState, port: u16, runner_name: &str) {
    let url = format!(
        "http://127.0.0.1:{}/ui-bridge/control/page/close-request",
        port
    );
    let result = state
        .http_client
        .post(&url)
        .timeout(Duration::from_millis(
            RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS,
        ))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {
            let msg = format!(
                "Requested graceful stop for runner '{}' via close-request (port {})",
                runner_name, port
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
        }
        Ok(resp) => {
            let msg = format!(
                "Graceful close-request for runner '{}' (port {}) returned {} — falling through to kill",
                runner_name,
                port,
                resp.status()
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
        Err(e) => {
            let msg = format!(
                "Graceful close-request for runner '{}' (port {}) failed: {} — falling through to kill",
                runner_name, port, e
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
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

    // The Child handle is owned by the `monitor_runner_process_exit` task that
    // was spawned when the runner started — it calls `runner.process.take()`
    // immediately so it can await `child.wait()` without holding the lock. So
    // by the time we get here, `managed.runner.process` is always None, and
    // we have to work via (a) the graceful HTTP close endpoint, (b) the stored
    // PID, and (c) the `running` flag that the monitor task flips to false
    // when the process exits.
    let pid_to_kill = {
        let runner = managed.runner.read().await;
        runner.pid
    };

    // Orphan-PID recovery: if the registry lost track of the PID (None) but
    // the runner's configured port is in use by some process, that process is
    // the de-facto runner — likely a zombie from a prior supervisor instance
    // that the current supervisor adopted partially. Kill it up-front so the
    // graceful path below has a free port to verify, instead of returning
    // success while the OS process keeps running.
    #[cfg(target_os = "windows")]
    if pid_to_kill.is_none() {
        if let Some(orphan_pid) = crate::process::windows::find_pid_on_port(port).await {
            let msg = format!(
                "Recovered orphan PID {} on port {} for runner '{}'; killing",
                orphan_pid, port, runner_name
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
            let _ = kill_by_pid(orphan_pid).await;
            // Be explicit about the resulting registry state. The PID was
            // already None, but flip running=false so callers that race a
            // health probe see the post-kill view immediately rather than the
            // stale "running=true, pid=None" tuple.
            {
                let mut runner = managed.runner.write().await;
                runner.pid = None;
                runner.running = false;
            }
        }
    }

    // 1. Graceful-first: ask the runner to close itself via the same endpoint
    //    the UI uses, so WindowEvent::CloseRequested fires and its teardown
    //    hooks run (notably UsbTransport::release_all, which removes adb
    //    forwards — see qontinui-runner §1.6a). Best-effort: on any failure
    //    we fall through to the PID kill below.
    request_graceful_stop(state, port, &runner_name).await;

    // 2. Poll the monitor's `running` flag for up to
    //    RUNNER_GRACEFUL_STOP_TIMEOUT_MS. When the runner exits, the monitor
    //    task sets running=false — that's our signal that graceful worked.
    let graceful_deadline =
        std::time::Instant::now() + Duration::from_millis(RUNNER_GRACEFUL_STOP_TIMEOUT_MS);
    let mut exited_gracefully = false;
    while std::time::Instant::now() < graceful_deadline {
        if !managed.runner.read().await.running {
            let msg = format!(
                "Runner '{}' exited gracefully after close-request",
                runner_name
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
            exited_gracefully = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !exited_gracefully {
        let msg = format!(
            "Graceful stop timed out for runner '{}' after {}ms, falling through to taskkill",
            runner_name, RUNNER_GRACEFUL_STOP_TIMEOUT_MS
        );
        info!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, msg)
            .await;
    }

    // 3. Kill by PID. This is a no-op if the process already exited gracefully
    //    (taskkill reports "PID not found" at debug level) and the primary
    //    mechanism otherwise.
    if let Some(pid) = pid_to_kill {
        #[cfg(target_os = "windows")]
        let _ = kill_by_pid(pid).await;
        #[cfg(not(target_os = "windows"))]
        let _ = pid;
    }

    // 3. Clean up the runner's port
    let port_free = wait_for_port_free(port, 5).await;
    if !port_free {
        warn!(
            "Port {} still in use after stopping runner '{}', force-killing",
            port, runner_name
        );
        #[cfg(target_os = "windows")]
        let _ = kill_by_port(port).await;
    }

    // Snapshot the runner's state for post-mortem cache BEFORE clearing
    // `started_at` below. The crash-summary endpoint reports
    // `duration_alive_ms` computed from `started_at`/`stopped_at`, so the
    // snapshot must capture the value before the per-runner reset wipes it.
    // (For non-test runners we still capture so future post-mortem queries
    // can see the most recent stop event; the cache is bounded so this is
    // cheap.)
    let runner_id = managed.config.id.clone();
    let pre_clear_snapshot = if runner_id.starts_with("test-") {
        Some(
            crate::process::stopped_cache::snapshot_from_managed(
                managed.as_ref(),
                None,
                crate::process::stopped_cache::StopReason::GracefulStop,
            )
            .await,
        )
    } else {
        None
    };

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
    if runner_id.starts_with("test-") {
        if let Some(snapshot) = pre_clear_snapshot {
            let mut cache = state.stopped_runners.write().await;
            crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
        }

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
            // And the per-instance app data dirs (dev-logs, restate journal,
            // macros, prompts, playwright, contexts) — keyed off the config
            // name because that's what the runner received as
            // QONTINUI_INSTANCE_NAME.
            if let Err(e) = remove_runner_app_data_dirs(&runner_name, false).await {
                warn!(
                    "Failed to remove per-instance app data for test runner '{}': {}",
                    runner_name, e
                );
            }
        }

        // Clean up the per-runner exe copy to prevent disk bloat.
        // Each copy is ~200MB + ~1.3GB PDB; without cleanup, orphaned copies
        // accumulated to ~200GB in a recent audit.
        let exe_copy = state.config.runner_exe_copy_path(&runner_id);
        if exe_copy.exists() {
            if let Err(e) = std::fs::remove_file(&exe_copy) {
                warn!("Failed to remove runner exe copy {:?}: {}", exe_copy, e);
            } else {
                info!("Removed runner exe copy {:?}", exe_copy);
            }
        }
        // Also try to remove the PDB file (same name but .pdb extension)
        let pdb_copy = exe_copy.with_extension("pdb");
        if pdb_copy.exists() {
            let _ = std::fs::remove_file(&pdb_copy);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// A slot freshly built 5 minutes after the running copy is "stale".
    #[test]
    fn stale_binary_detection_slot_much_newer() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(300); // +5 min
        let out = compute_stale_binary(Some(running), Some((0, slot)))
            .expect("5-minute gap should be surfaced");
        assert_eq!(out.slot_id, 0);
        assert_eq!(out.age_delta_secs, 300);
        assert_eq!(out.running_mtime_ms, 1_700_000_000 * 1000);
        assert_eq!(out.slot_mtime_ms, (1_700_000_000 + 300) * 1000);
    }

    /// A slot 10 seconds newer is within jitter — no badge.
    #[test]
    fn stale_binary_detection_within_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(10);
        assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
    }

    /// A slot exactly at the threshold (30s) does not trigger — strict `>`.
    #[test]
    fn stale_binary_detection_at_exact_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64);
        assert!(
            compute_stale_binary(Some(running), Some((0, slot))).is_none(),
            "delta == threshold must not surface a stale_binary entry"
        );
    }

    /// One second over the threshold DOES trigger.
    #[test]
    fn stale_binary_detection_just_over_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64 + 1);
        let out = compute_stale_binary(Some(running), Some((0, slot)))
            .expect("threshold + 1s should surface a stale_binary entry");
        assert_eq!(out.age_delta_secs, STALE_BINARY_THRESHOLD_SECS + 1);
    }

    /// A slot older than the running copy means the running copy is the
    /// freshest binary on disk — normal state, no badge.
    #[test]
    fn stale_binary_detection_running_newer() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running - Duration::from_secs(120);
        assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
    }

    /// Identical mtimes — no divergence, no badge.
    #[test]
    fn stale_binary_detection_equal() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(Some(running), Some((1, running))).is_none());
    }

    /// Missing running-copy mtime (first start, fs stat failed, etc.) — the
    /// feature silently skips.
    #[test]
    fn stale_binary_detection_missing_running_mtime() {
        let slot = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(None, Some((0, slot))).is_none());
    }

    /// No slot has ever produced a binary — nothing to compare against.
    #[test]
    fn stale_binary_detection_no_slot_binary() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(Some(running), None).is_none());
    }

    /// Slot id is preserved through the struct (not always 0).
    #[test]
    fn stale_binary_detection_preserves_slot_id() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(600);
        let out = compute_stale_binary(Some(running), Some((2, slot))).expect("stale");
        assert_eq!(out.slot_id, 2);
    }

    // =========================================================================
    // First-healthy watchdog decision tests
    // =========================================================================

    /// Process gone — exit quietly regardless of other flags.
    #[test]
    fn first_healthy_abandon_when_untracked() {
        assert_eq!(
            decide_first_healthy(false, false, false),
            FirstHealthyDecision::Abandon
        );
        // Even if the port is "responding" and the deadline passed, an
        // untracked PID is not ours to act on.
        assert_eq!(
            decide_first_healthy(false, true, true),
            FirstHealthyDecision::Abandon
        );
    }

    /// HTTP /health responded — healthy outcome, even if the deadline just
    /// elapsed on the same tick.
    #[test]
    fn first_healthy_healthy_wins_over_kill() {
        assert_eq!(
            decide_first_healthy(true, true, false),
            FirstHealthyDecision::Healthy
        );
        // Edge case the priority rule exists for: responsive AND past
        // deadline on the same poll. We do NOT kill — the runner made it.
        assert_eq!(
            decide_first_healthy(true, true, true),
            FirstHealthyDecision::Healthy
        );
    }

    /// Tracked, not responding, deadline passed — kill path.
    #[test]
    fn first_healthy_kill_when_deadline_passed_and_unresponsive() {
        assert_eq!(
            decide_first_healthy(true, false, true),
            FirstHealthyDecision::Kill
        );
    }

    /// Tracked, not responding, still within budget — keep waiting.
    #[test]
    fn first_healthy_wait_while_within_budget() {
        assert_eq!(
            decide_first_healthy(true, false, false),
            FirstHealthyDecision::Wait
        );
    }
}

//! Layer 3 of the orphan-runner safety net: a startup scan that finds
//! `qontinui-runner.exe` processes left over from a prior supervisor
//! instance and either adopts them back into the registry (when a
//! registered runner config claims the port they're listening on) or
//! kills them so the next build can replace the slot binary.
//!
//! The scan is Windows-specific (PID enumeration + file-lock semantics).
//! On other platforms `scan_orphans_at_startup` is a no-op stub and every
//! helper / type below is genuinely unused — gate the dead-code lint
//! accordingly so non-Windows CI doesn't trip on unreachable items.
#![cfg_attr(not(target_os = "windows"), allow(unused))]
//!
//! Why this exists when Layer 2 (the kill-on-exit JobObject) already kills
//! children at supervisor shutdown:
//!
//! - The previous supervisor binary may pre-date Layer 2 — the JobObject
//!   only takes effect on processes spawned by a supervisor that *has*
//!   the JobObject code. A force-kill of a JobObject-aware supervisor
//!   does run the kernel-side `KILL_ON_JOB_CLOSE`, but only because the
//!   handle goes away with the process; if the previous supervisor was
//!   built without the job at all, none of its children are in any job.
//! - Manually-spawned `cargo build` runners have no supervisor parent at
//!   all, so no JobObject covers them.
//! - JobObject creation can fail at startup (logged as a warning, but the
//!   supervisor still runs without the safety net).
//!
//! In any of these cases, a fresh supervisor inherits the prior session's
//! runners as orphans holding slot binaries. The next build fails with
//! "Access is denied." This scan resolves that on every cold start.
//!
//! Algorithm — see `scan_orphans_at_startup` for the concrete
//! implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use tracing::{info, warn};

use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::is_temp_runner;
#[cfg(target_os = "windows")]
use crate::process::windows::{find_pid_on_port, find_runner_processes, kill_by_pid};
use crate::state::{ManagedRunner, SharedState};

/// Tolerance window absorbing filesystem mtime resolution jitter and the
/// multi-second gap between "build completes" and "supervisor records the
/// timestamps." If an orphan binary is older than the freshest available
/// source by **more than** this gap, the orphan is considered stale and
/// killed instead of adopted. Matches the existing
/// `STALE_BINARY_THRESHOLD_SECS` policy in `process::manager` so the
/// "newer build available" badge and the orphan-staleness decision use
/// the same threshold.
const STALENESS_GAP_SECS: i64 = 30;

/// Decision returned by `should_adopt_or_kill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptDecision {
    Adopt,
    KillStale,
}

/// Pure comparison: should the orphan be adopted, or killed because it's
/// running a stale binary that a fresh spawn would replace?
///
/// Rules (mirrors the algorithm documented in `scan_orphans_at_startup`):
/// - `orphan_mtime is None` → adopt (can't tell, conservative).
/// - `freshest_source_mtime is None` → adopt (orphan is the only thing on
///   disk anyway; killing it would just leave the user with nothing).
/// - `orphan_mtime + STALENESS_GAP < freshest_source_mtime` → kill.
///   The freshest binary is meaningfully newer than what the orphan is
///   running, so the user's `/runner/restart` flow would launch a newer
///   exe than what's running today.
/// - Otherwise → adopt (orphan is current enough; preserve the session).
fn should_adopt_or_kill(
    orphan_mtime: Option<DateTime<Utc>>,
    freshest_source_mtime: Option<DateTime<Utc>>,
    gap_secs: i64,
) -> AdoptDecision {
    let Some(orphan) = orphan_mtime else {
        return AdoptDecision::Adopt;
    };
    let Some(fresh) = freshest_source_mtime else {
        return AdoptDecision::Adopt;
    };
    let gap = chrono::Duration::seconds(gap_secs);
    if orphan + gap < fresh {
        AdoptDecision::KillStale
    } else {
        AdoptDecision::Adopt
    }
}

/// Read mtime of a path as `DateTime<Utc>`. Returns `None` when the file
/// is missing or its metadata is unreadable — caller treats `None` as
/// "couldn't tell, fall back to safe behavior."
fn path_mtime_utc(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.into())
}

/// Freshest mtime across every existing
/// `target-pool/slot-*/debug/qontinui-runner.exe` plus
/// `target-pool/lkg/qontinui-runner.exe`. This is the upper bound on what
/// `start_managed_runner` would launch on the user's next `/runner/restart`.
///
/// Returns `None` only when no source binary exists anywhere — at that
/// point the orphan is the only artifact on disk and adoption is the
/// safe choice. Slot scans use the same `state.build_pool.slots` list
/// that the build pool itself walks, so we never go out of sync with
/// supervisor config.
async fn freshest_source_mtime(state: &SharedState) -> Option<DateTime<Utc>> {
    let mut best: Option<DateTime<Utc>> = None;
    for slot in &state.build_pool.slots {
        let p = slot.target_dir.join("debug").join("qontinui-runner.exe");
        if let Some(mt) = path_mtime_utc(&p) {
            best = Some(match best {
                Some(b) if b >= mt => b,
                _ => mt,
            });
        }
    }
    let lkg = state.config.lkg_exe_path();
    if let Some(mt) = path_mtime_utc(&lkg) {
        best = Some(match best {
            Some(b) if b >= mt => b,
            _ => mt,
        });
    }
    best
}

/// Result of resolving a single orphan PID — used to log the summary.
#[derive(Debug, Default)]
struct ScanSummary {
    found: usize,
    adopted: usize,
    killed: usize,
}

/// Top-level entry point. Enumerates every live `qontinui-runner.exe`
/// process, filters to those whose image lives under the supervisor's
/// `target-pool/` or `target/debug/` (so we don't touch unrelated
/// runners), and either adopts each PID into a registered runner or
/// kills it.
///
/// Runs once at supervisor startup AFTER `SupervisorState::new` returns
/// and BEFORE the HTTP server starts taking traffic and BEFORE
/// `prewarm_build_slots` spawns. Awaiting here serializes with the rest
/// of startup — we don't want a prewarm running on a slot whose binary
/// is still locked by an orphan we haven't killed yet.
///
/// Always logs a single summary line at info level, even when the scan
/// finds nothing. Per-PID actions (adopt or kill) emit their own log
/// entries so an operator can audit what happened.
#[cfg(not(target_os = "windows"))]
pub async fn scan_orphans_at_startup(_state: &SharedState) {
    // Orphan scanning is Windows-specific (handles `qontinui-runner.exe`
    // file locks via Win32 process enumeration). On other platforms the
    // supervisor is not supported, so this is a no-op.
}

#[cfg(target_os = "windows")]
pub async fn scan_orphans_at_startup(state: &SharedState) {
    info!("Startup orphan scan: enumerating qontinui-runner.exe processes...");

    // Resolve the two roots we recognize as "ours". `runner_npm_dir()`
    // returns the canonicalized absolute path of the runner workspace —
    // same helper the build pool uses, so the prefix check matches what
    // ends up on disk after a build copies a binary into a slot.
    let runner_root = state.config.runner_npm_dir();
    let target_pool = runner_root.join("target-pool");
    let target_debug = runner_root.join("target").join("debug");

    let target_pool_canon = canonicalize_or_keep(&target_pool);
    let target_debug_canon = canonicalize_or_keep(&target_debug);

    let our_pid = std::process::id();
    let processes = find_runner_processes().await;

    // Build a (port -> Arc<ManagedRunner>) map once so we can look up
    // adoption candidates by port without re-walking the registry per
    // orphan. Bounded by the number of registered runners (~5) so this
    // is cheap.
    let runners = state.get_all_runners().await;
    let mut by_port: HashMap<u16, Arc<ManagedRunner>> = HashMap::new();
    for r in &runners {
        by_port.insert(r.config.port, r.clone());
    }

    let mut summary = ScanSummary::default();

    for (pid, exe_path) in processes {
        // Defense against the pathological case where this scan ever runs
        // inside the supervisor process (it shouldn't — supervisor is
        // qontinui-supervisor.exe — but the PID check is cheap).
        if pid == our_pid {
            continue;
        }

        // Only consider runners running from our build outputs. A runner
        // launched from somewhere else (a developer's hand-built install,
        // a different checkout, etc.) is not our concern.
        if !path_is_under(&exe_path, &target_pool_canon)
            && !path_is_under(&exe_path, &target_debug_canon)
        {
            continue;
        }

        summary.found += 1;

        // If the supervisor's registry already claims this PID, the
        // health-cache rehydration path (which calls `find_pid_on_port`
        // before our scan ran in steady state, and ran during our scan
        // in the spawn case) has already adopted it. Skip — adopting a
        // second time would be a no-op but produce a misleading log.
        if registered_pid(&runners, pid).await {
            continue;
        }

        // Try adoption: find the listening port for this PID by probing
        // every registered runner's port and inverting the match. We
        // probe registered ports rather than `netstat | findstr <pid>`
        // because we only ever care about ports that map to a registered
        // runner config — orphans bound to other ports are by definition
        // unadoptable and will fall through to the kill branch.
        let port = listening_port_for_pid(pid, &runners).await;
        if let Some(port) = port {
            if let Some(managed) = by_port.get(&port).cloned() {
                // Refuse to adopt PIDs whose runner id is a temp runner.
                // Temp runners are ephemeral by design; the next
                // `cleanup_orphaned_runners` pass will purge their
                // registry entry, so adopting just to immediately reap
                // would churn state for nothing. Kill instead.
                if is_temp_runner(&managed.config.id) {
                    warn!(
                        "Killing orphan qontinui-runner.exe PID {} from {:?} at startup \
                         (port {} maps to temp runner '{}', not adopting ephemerals)",
                        pid, exe_path, port, managed.config.id
                    );
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!(
                                "Killing orphan qontinui-runner.exe PID {} (temp runner '{}', port {})",
                                pid, managed.config.id, port
                            ),
                        )
                        .await;
                    if kill_by_pid(pid).await.unwrap_or(false) {
                        summary.killed += 1;
                    }
                    continue;
                }

                // Stale-binary check: refuse to adopt orphans whose exe
                // is meaningfully older than the freshest available
                // source binary. After a `/runner/restart {rebuild: true}`
                // the user expects the runner to be running the freshly
                // built code; adopting an old orphan would silently
                // produce a stale runner. The check trades the user's
                // session continuity for build-correctness — but only
                // when there's actually a newer binary on disk waiting
                // to be launched.
                let orphan_mtime = path_mtime_utc(&exe_path);
                let fresh_mtime = freshest_source_mtime(state).await;
                if should_adopt_or_kill(orphan_mtime, fresh_mtime, STALENESS_GAP_SECS)
                    == AdoptDecision::KillStale
                {
                    let orphan_iso = orphan_mtime
                        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
                        .unwrap_or_else(|| "unknown".to_string());
                    let fresh_iso = fresh_mtime
                        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
                        .unwrap_or_else(|| "unknown".to_string());
                    let gap_secs = match (orphan_mtime, fresh_mtime) {
                        (Some(o), Some(f)) => (f - o).num_seconds(),
                        _ => 0,
                    };
                    warn!(
                        "Killing stale-adopted-orphan runner '{}' PID {} — \
                         orphan binary mtime {} is {}s older than freshest source mtime {}; \
                         user's session will be lost but adoption would have produced a stale runner",
                        managed.config.id, pid, orphan_iso, gap_secs, fresh_iso
                    );
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!(
                                "Killing stale-adopted-orphan runner '{}' PID {} — \
                                 orphan binary mtime {} is {}s older than freshest source mtime {}",
                                managed.config.id, pid, orphan_iso, gap_secs, fresh_iso
                            ),
                        )
                        .await;
                    if kill_by_pid(pid).await.unwrap_or(false) {
                        summary.killed += 1;
                    }
                    continue;
                }

                // Non-temp registered runner. Adopt: stuff this PID into
                // the runner's RunnerState so the rest of the supervisor
                // sees it as healthy. `process: Option<Child>` stays None
                // — we never had a `tokio::process::Child` handle for an
                // orphan, and downstream code that polls health via HTTP
                // is unaffected.
                if adopt_pid_into_registry(&managed, pid).await {
                    info!(
                        "Adopted orphan runner '{}' (PID {}, port {}) into registry at startup",
                        managed.config.id, pid, port
                    );
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Info,
                            format!(
                                "Adopted orphan runner '{}' (PID {}, port {}) into registry at startup",
                                managed.config.id, pid, port
                            ),
                        )
                        .await;
                    summary.adopted += 1;
                } else {
                    // Adoption was raced by another path that filled in
                    // the PID first — treat as a no-op success.
                    info!(
                        "Adoption of PID {} for runner '{}' skipped — registry already populated",
                        pid, managed.config.id
                    );
                }
                continue;
            }
        }

        // No registry match by port. This is a true orphan holding a
        // slot/copy binary the next build needs. Kill it.
        warn!(
            "Killing orphan qontinui-runner.exe PID {} from {:?} at startup \
             (no registered runner claims it)",
            pid, exe_path
        );
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Warn,
                format!(
                    "Killing orphan qontinui-runner.exe PID {} from {:?} at startup",
                    pid, exe_path
                ),
            )
            .await;
        if kill_by_pid(pid).await.unwrap_or(false) {
            summary.killed += 1;
        }
    }

    info!(
        "Startup orphan scan: {} runner(s) found, {} adopted, {} killed",
        summary.found, summary.adopted, summary.killed
    );
}

/// Return true if any registered `ManagedRunner` already records `pid` in
/// its `RunnerState.pid` field. Used to skip orphans the health-cache
/// rehydration already adopted via `find_pid_on_port`.
async fn registered_pid(runners: &[Arc<ManagedRunner>], pid: u32) -> bool {
    for r in runners {
        let state = r.runner.read().await;
        if state.pid == Some(pid) {
            return true;
        }
    }
    false
}

/// Probe every registered runner's port for the listening PID. When we
/// find a port whose listener is `pid`, return that port. Returns `None`
/// when no registered port is bound by `pid`.
///
/// This is deliberately bounded by the registered-runner count (~5)
/// rather than running a global `netstat | findstr <pid>` because the
/// only ports we can adopt against are ports that already have a runner
/// config — anything else is unadoptable by definition.
///
/// Defensive against the impossible-but-defined case where two PIDs both
/// claim the same port: `find_pid_on_port` returns the first listener
/// netstat reports, which is good enough for adoption — the other PID
/// either gets caught by a subsequent registered-port probe or falls
/// through to the kill branch.
#[cfg(target_os = "windows")]
async fn listening_port_for_pid(pid: u32, runners: &[Arc<ManagedRunner>]) -> Option<u16> {
    for r in runners {
        let port = r.config.port;
        if let Some(p) = find_pid_on_port(port).await {
            if p == pid {
                return Some(port);
            }
        }
    }
    None
}

/// Stuff `pid` into `managed.runner` so the rest of the supervisor sees
/// it as a healthy running runner. Returns true when the adoption
/// actually wrote new state, false when `pid` was already populated by
/// another path (race-safe no-op).
///
/// `started_at` is set to `Utc::now()` — we don't have the orphan's
/// real start time and the field is only used for human-readable
/// "uptime" displays. Marking "now" is the clearest "we just adopted
/// this" signal.
///
/// Note: `process: Option<Child>` is intentionally left as `None`. We
/// don't have the `tokio::process::Child` handle from
/// `tokio::process::Command::spawn` because we didn't spawn this child
/// — an orphan, by definition, is a process whose parent (the previous
/// supervisor) is gone. Any code path that calls `child.wait()` on the
/// adopted runner will not work; the rest of the codebase polls health
/// via HTTP (which is unaffected) and only the spawn flow holds onto
/// `Child` directly.
async fn adopt_pid_into_registry(managed: &ManagedRunner, pid: u32) -> bool {
    let mut runner = managed.runner.write().await;
    if runner.pid.is_some() && runner.running {
        return false;
    }
    runner.running = true;
    runner.pid = Some(pid);
    if runner.started_at.is_none() {
        runner.started_at = Some(Utc::now());
    }
    runner.stop_requested = false;
    runner.restart_requested = false;
    true
}

/// Test whether `child` lives under `parent`. Tries canonical-path
/// containment first (handles symlinks, short-name forms, etc.) and
/// falls back to a case-insensitive lowercase string-prefix match if
/// either canonicalize call fails — which happens on Windows when an
/// executable file has been deleted while the process is still running
/// (yes, this is possible on Windows, despite the file lock).
fn path_is_under(child: &Path, parent: &Path) -> bool {
    let child_canon = canonicalize_or_keep(child);
    let parent_canon = canonicalize_or_keep(parent);
    if child_canon.starts_with(&parent_canon) {
        return true;
    }
    let child_str = child.to_string_lossy().to_ascii_lowercase();
    let parent_str = parent.to_string_lossy().to_ascii_lowercase();
    child_str.starts_with(&parent_str)
}

/// `Path::canonicalize` but returns the original path on error. Used as
/// a best-effort step before prefix-matching — if canonicalization
/// fails, the caller will fall back to a string-prefix compare.
fn canonicalize_or_keep(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::Path;

    #[test]
    fn path_is_under_basic_match() {
        let parent = Path::new("/tmp/qontinui-runner/target-pool");
        let child = Path::new("/tmp/qontinui-runner/target-pool/slot-0/debug/qontinui-runner.exe");
        assert!(path_is_under(child, parent));
    }

    #[test]
    fn path_is_under_no_match() {
        let parent = Path::new("/tmp/qontinui-runner/target-pool");
        let child = Path::new("/tmp/some-other-checkout/target/debug/qontinui-runner.exe");
        assert!(!path_is_under(child, parent));
    }

    #[cfg(windows)]
    #[test]
    fn path_is_under_case_insensitive() {
        let parent = Path::new(r"C:\Qontinui\Target-Pool");
        let child = Path::new(r"c:\qontinui\target-pool\slot-0\debug\qontinui-runner.exe");
        assert!(path_is_under(child, parent));
    }

    /// Helper for constructing UTC timestamps in tests.
    fn ts(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn should_adopt_or_kill_unknown_orphan_mtime_adopts() {
        // Conservative: missing orphan mtime → adopt (don't kill what we
        // can't classify).
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            should_adopt_or_kill(None, fresh, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_no_source_on_disk_adopts() {
        // No source binary anywhere → orphan is the only artifact, killing
        // it would leave the user with nothing.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            should_adopt_or_kill(orphan, None, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_newer_than_source_adopts() {
        // Orphan was built after the freshest source on disk (e.g. cleared
        // slot dirs since spawn). Definitely adopt.
        let orphan = Some(ts(2026, 4, 27, 12, 5, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_equal_to_source_adopts() {
        // Same mtime — orphan is the freshest binary. Adopt.
        let t = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            should_adopt_or_kill(t, t, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_within_tolerance_adopts() {
        // Orphan is 30s older than source — exactly at the tolerance
        // boundary. The check is `orphan + GAP < fresh`, so 30s gap is
        // NOT strictly less than (orphan + 30s) and we adopt.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 30));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_just_inside_tolerance_adopts() {
        // 29s older — well within the 30s jitter window. Adopt.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 29));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh, STALENESS_GAP_SECS),
            AdoptDecision::Adopt
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_60s_older_kills() {
        // Orphan is 60s older than freshest source — well past the 30s
        // tolerance. The user just rebuilt; adopting would silently run
        // the stale binary. Kill.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 1, 0));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh, STALENESS_GAP_SECS),
            AdoptDecision::KillStale
        );
    }

    #[test]
    fn should_adopt_or_kill_orphan_one_hour_older_kills() {
        // Pathological "user restarted supervisor an hour after rebuild"
        // case — definitely kill.
        let orphan = Some(ts(2026, 4, 27, 11, 0, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh, STALENESS_GAP_SECS),
            AdoptDecision::KillStale
        );
    }

    #[test]
    fn should_adopt_or_kill_zero_gap_strict_compare() {
        // gap=0 reduces to strict-less-than — equal mtimes adopt, any
        // older orphan kills. Sanity check that the comparison is
        // strict and the gap_secs parameter actually plumbs through.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        let fresh_same = Some(ts(2026, 4, 27, 12, 0, 0));
        let fresh_1s = Some(ts(2026, 4, 27, 12, 0, 1));
        assert_eq!(
            should_adopt_or_kill(orphan, fresh_same, 0),
            AdoptDecision::Adopt
        );
        assert_eq!(
            should_adopt_or_kill(orphan, fresh_1s, 0),
            AdoptDecision::KillStale
        );
    }
}

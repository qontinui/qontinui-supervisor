//! Layer 3 of the orphan-runner safety net: a startup scan that finds
//! `qontinui-runner.exe` processes the supervisor does not (yet) track and
//! either adopts them back into the registry (when a registered runner
//! config claims them — by image path or by the port they're listening on)
//! or kills them so the next build can replace the slot binary.
//!
//! **A live primary at startup is the expected steady state, not an
//! anomaly.** Since the kill-on-exit JobObject holds only supervisor-owned
//! *temp* runners (`process::job::should_assign_to_ephemeral_job`), the
//! operator's primary and named runners deliberately survive every
//! supervisor exit path. The very next supervisor start therefore finds them
//! here — untracked, because this process never spawned them — and adopting
//! them back is the normal path, not a recovery from a crash. Only two
//! classes are killed: ephemeral leftovers, which are supervisor-owned by
//! definition, and processes no registered runner config claims at all.
//!
//! **Ownership is decided by IMAGE PATH first, port probe second.** Every
//! registered runner launches from its own
//! [`crate::config::Config::runner_exe_copy_path`]
//! (`target/debug/qontinui-runner-primary.exe`,
//! `qontinui-runner-test-<port>.exe`, …), so the orphan's `exe_path` alone
//! identifies which registered runner it is — no live process, no netstat, no
//! listening socket required. The netstat port probe is only a secondary
//! signal, and a *failed* probe is UNKNOWN rather than "unclaimed"
//! ([`PortProbe`]). Both halves matter: a primary that is alive but not yet
//! LISTENING (mid-boot, e.g. stuck behind PG bootstrap locks) and a probe that
//! could not run at all used to land in the unconditional kill branch below —
//! taking the operator's terminal panes and `claude.exe` sessions with them.
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
//! *temp* runners as orphans holding slot binaries. The next build fails
//! with "Access is denied." This scan resolves that on every cold start.
//!
//! **Known gap for adopted runners:** `adopt_pid_into_registry` leaves
//! `process: Option<Child>` as `None` — we never had a
//! `tokio::process::Child` handle for a process we did not spawn, and one
//! cannot be synthesized. So the crash-only watchdog's *exit observation*
//! does not cover an adopted (inherited) primary until this supervisor
//! starts it itself. HTTP health polling — the surface the operator actually
//! watches — is unaffected.
//!
//! Algorithm — see `scan_orphans_at_startup` for the concrete
//! implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use tracing::{info, warn};

use crate::log_capture::{LogLevel, LogSource};
#[cfg(target_os = "windows")]
use crate::process::windows::{find_pid_on_port, find_runner_processes, kill_by_pid};
use crate::state::{ManagedRunner, SharedState};

/// Tolerance window absorbing filesystem mtime resolution jitter and the
/// multi-second gap between "build completes" and "supervisor records the
/// timestamps." An orphan binary older than the freshest available source
/// by **more than** this gap is reported as stale (a `Warn` on adoption —
/// never a kill). Matches the existing `STALE_BINARY_THRESHOLD_SECS`
/// policy in `process::manager` so the "newer build available" badge and
/// this warning use the same threshold.
const STALENESS_GAP_SECS: i64 = 30;

/// What to do with an orphan PID whose port matched a registered runner
/// config. Returned by [`resolve_registered_orphan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanAction {
    /// Supervisor-owned ephemeral (`test-*`): kill it. Temps are ephemeral
    /// by design and the next `cleanup_orphaned_runners` pass purges the
    /// registry entry, so adopting just to immediately reap would churn
    /// state for nothing.
    KillEphemeral,
    /// User-owned runner (primary / named / external): adopt it back.
    /// `stale_gap_secs` is `Some(n)` when its exe is `n` seconds older than
    /// the freshest source binary by more than the tolerance — surfaced as
    /// a `Warn`, never acted on.
    Adopt { stale_gap_secs: Option<i64> },
}

/// Pure decision for an orphan that a registered runner config claims.
///
/// **Adoption is unconditional for user-owned runners.** Until 2026-07-28
/// this branch also killed an adoptable orphan whose exe was more than
/// `tolerance_secs` older than the freshest `target-pool/slot-*` or
/// `target-pool/lkg` binary — logging *"user's session will be lost"* as it
/// did so. That rule made the whole ownership split pointless: a primary
/// saved from the kernel at supervisor *exit* was killed here at supervisor
/// *start*, ~5s later. It was also a race the primary loses by simply having
/// been up for a while — the mtimes compared are "when the primary last
/// started" vs "when ANY peer last built the runner", and on this fleet
/// `POST /runners/spawn-test {rebuild:true}` is the routine way every agent
/// tests a change. Staleness is now reported, not enforced; the operator
/// decides (`POST /runner/restart {"rebuild": true}`).
///
/// Rules:
/// - temp runner → [`OrphanAction::KillEphemeral`], regardless of mtime.
/// - otherwise → [`OrphanAction::Adopt`], with `stale_gap_secs` set only
///   when both mtimes are known and `orphan + tolerance < freshest`.
fn resolve_registered_orphan(
    is_temp: bool,
    orphan_mtime: Option<DateTime<Utc>>,
    freshest_source_mtime: Option<DateTime<Utc>>,
    tolerance_secs: i64,
) -> OrphanAction {
    if is_temp {
        return OrphanAction::KillEphemeral;
    }
    let stale_gap_secs = match (orphan_mtime, freshest_source_mtime) {
        (Some(orphan), Some(fresh))
            if orphan + chrono::Duration::seconds(tolerance_secs) < fresh =>
        {
            Some((fresh - orphan).num_seconds())
        }
        _ => None,
    };
    OrphanAction::Adopt { stale_gap_secs }
}

/// Outcome of probing the registered runner ports for an orphan PID.
///
/// The three states are deliberately distinct. Collapsing `Unknown` into
/// `NoMatch` is the "silent-empty treated as NO" bug: a single failed
/// `cmd.exe` spawn makes every registered port look unclaimed, and the scan
/// reads "unclaimed" as "true orphan, kill it" — one transient failure
/// reaping every runner under `target/` in a single pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortProbe {
    /// The probe ran and this PID is LISTENING on the named registered port.
    Matched(u16),
    /// The probe ran against every registered port and none of them is held
    /// by this PID. A real answer: no registered runner claims it *by port*.
    NoMatch,
    /// At least one port probe could not RUN. Ownership by port is UNKNOWN,
    /// which is never sufficient grounds for a kill.
    Unknown,
}

/// One registered runner's deterministic identity: the image path
/// `start_managed_runner` launches it from, plus whether it is
/// supervisor-owned. Built from the registry by [`registered_exe_table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredExe {
    runner_id: String,
    is_temp: bool,
    exe_copy_path: PathBuf,
}

/// Snapshot the registry's image-path identity table.
///
/// Shared by the startup orphan scan and by `build_monitor::free_slot_exe`,
/// which face the same question ("is this PID one of the operator's runners
/// or a true orphan?") and must not answer it two different ways.
pub async fn registered_exe_table(state: &SharedState) -> Vec<RegisteredExe> {
    let runners = state.get_all_runners().await;
    exe_table_from_runners(state, &runners)
}

/// [`registered_exe_table`] over a registry snapshot the caller already holds,
/// so the scan classifies against exactly the runners it enumerated.
fn exe_table_from_runners(
    state: &SharedState,
    runners: &[Arc<ManagedRunner>],
) -> Vec<RegisteredExe> {
    runners
        .iter()
        .map(|r| RegisteredExe {
            runner_id: r.config.id.clone(),
            is_temp: r.config.kind().is_temp(),
            exe_copy_path: state.config.runner_exe_copy_path(&r.config),
        })
        .collect()
}

/// Who owns an orphan's image path, decided from the registry alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExeOwner {
    /// The image path IS a registered runner's exe copy.
    Registered { runner_id: String, is_temp: bool },
    /// No registered runner's exe copy matches this image path — a genuinely
    /// unowned process (e.g. one running straight out of
    /// `target-pool/slot-1/debug/qontinui-runner.exe`).
    Unowned,
}

/// Pure seam: does `exe_path` belong to a registered runner?
///
/// Every runner start copies its resolved source exe to a **unique** path
/// (`runner_exe_copy_path`: id-keyed for `Primary`/`External`, port-keyed for
/// `Temp`/`Named`), so the image path is a deterministic identity that needs
/// neither a live socket nor a working netstat. This is what lets the scan
/// recognize the operator's primary while it is still booting.
///
/// First match wins; the copy paths are unique by construction.
pub fn classify_exe_owner(exe_path: &Path, registered: &[RegisteredExe]) -> ExeOwner {
    for entry in registered {
        if paths_equal_ignore_case(exe_path, &entry.exe_copy_path) {
            return ExeOwner::Registered {
                runner_id: entry.runner_id.clone(),
                is_temp: entry.is_temp,
            };
        }
    }
    ExeOwner::Unowned
}

/// What to do with an orphan whose port probe did NOT tie it to a registered
/// runner — either because no registered port is bound by it, or because the
/// probe could not run. Returned by [`decide_unmatched_orphan`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnmatchedOrphanAction {
    /// The image path is a registered **non-temp** runner's exe copy: this is
    /// the operator's runner, identified without any probe. Adopt it.
    AdoptByExePath { runner_id: String },
    /// The image path is a registered **temp** runner's exe copy. Temps are
    /// supervisor-owned; kill (today's behavior, now on deterministic
    /// evidence rather than on a port match).
    KillEphemeral { runner_id: String },
    /// No registered runner's image path matches: a true unowned orphan
    /// holding a slot/copy binary the next build needs. Kill it.
    KillUnowned,
    /// The port probe failed AND the image path matches nothing we know.
    /// Both signals are absent, so there is no evidence of ownership either
    /// way — do nothing and log. `free_slot_exe` still frees a genuinely
    /// stuck slot binary at the next build.
    SkipProbeUnknown,
}

/// Pure decision for an orphan the port probe did not claim.
///
/// **Deterministic identity outranks the probe.** A registered non-temp image
/// path is adopted even when the probe failed, because the image path alone
/// proves whose process it is. Conversely, an `Unknown` probe can never
/// produce a kill: with the probe down we cannot distinguish "port idle" from
/// "could not ask", so the only safe unowned-orphan verdict is "leave it".
fn decide_unmatched_orphan(probe: PortProbe, owner: &ExeOwner) -> UnmatchedOrphanAction {
    match owner {
        ExeOwner::Registered {
            runner_id,
            is_temp: false,
        } => UnmatchedOrphanAction::AdoptByExePath {
            runner_id: runner_id.clone(),
        },
        _ if probe == PortProbe::Unknown => UnmatchedOrphanAction::SkipProbeUnknown,
        ExeOwner::Registered { runner_id, .. } => UnmatchedOrphanAction::KillEphemeral {
            runner_id: runner_id.clone(),
        },
        ExeOwner::Unowned => UnmatchedOrphanAction::KillUnowned,
    }
}

/// Case-insensitive path equality that tolerates either side being absent
/// from disk.
///
/// Canonicalization is attempted first (resolves `..`, 8.3 short names and
/// symlinks), but on Windows a running process's image can be deleted while
/// the process lives, and a registered runner's exe copy may not exist yet —
/// so a canonicalize failure falls back to a lowercased string compare rather
/// than declaring the two paths different.
fn paths_equal_ignore_case(a: &Path, b: &Path) -> bool {
    let lower = |p: &Path| p.to_string_lossy().to_ascii_lowercase();
    lower(&canonicalize_or_keep(a)) == lower(&canonicalize_or_keep(b)) || lower(a) == lower(b)
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
/// point the orphan is the only artifact on disk and there is no staleness
/// to report. Slot scans use the same `state.build_pool.slots` list
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
    /// Orphans left running because the port probe could not run and their
    /// image path matched no registered runner. Counted separately so a
    /// broken probe is visible in the summary line instead of looking like a
    /// quiet scan.
    skipped_unknown: usize,
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

    // Build (port -> runner) and (id -> runner) maps once so we can look up
    // adoption candidates without re-walking the registry per orphan, plus
    // the image-path identity table the deterministic ownership check uses.
    // All bounded by the number of registered runners (~5), so this is cheap.
    let runners = state.get_all_runners().await;
    let mut by_port: HashMap<u16, Arc<ManagedRunner>> = HashMap::new();
    let mut by_id: HashMap<String, Arc<ManagedRunner>> = HashMap::new();
    for r in &runners {
        by_port.insert(r.config.port, r.clone());
        by_id.insert(r.config.id.clone(), r.clone());
    }
    let registered_exes = exe_table_from_runners(state, &runners);

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
        // unadoptable by port and fall through to the image-path check.
        let probe = listening_port_for_pid(pid, &runners).await;
        if let PortProbe::Matched(port) = probe {
            if let Some(managed) = by_port.get(&port).cloned() {
                apply_registered_orphan(state, &managed, pid, &exe_path, Some(port), &mut summary)
                    .await;
                continue;
            }
        }

        // The port probe did not tie this PID to a registered runner. Fall
        // back to DETERMINISTIC IDENTITY: every registered runner runs from
        // its own `runner_exe_copy_path`, so the image path names the owner
        // with no probe involved. This is what keeps a primary that is alive
        // but not yet LISTENING — and every runner at all when the probe
        // infrastructure itself fails — out of the kill branch.
        let owner = classify_exe_owner(&exe_path, &registered_exes);
        match decide_unmatched_orphan(probe, &owner) {
            UnmatchedOrphanAction::AdoptByExePath { runner_id } => {
                let Some(managed) = by_id.get(&runner_id).cloned() else {
                    // Registry changed under us between building the tables
                    // and here. No owner to adopt into and no evidence this
                    // is unowned — do nothing.
                    warn!(
                        "Orphan PID {} from {:?} matched registered runner '{}' by image path, \
                         but that runner is no longer in the registry — leaving it alone",
                        pid, exe_path, runner_id
                    );
                    continue;
                };
                info!(
                    "Orphan PID {} identified as user-owned runner '{}' by image path {:?} \
                     (port probe: {:?}) — adopting instead of killing",
                    pid, runner_id, exe_path, probe
                );
                apply_registered_orphan(state, &managed, pid, &exe_path, None, &mut summary).await;
            }
            UnmatchedOrphanAction::KillEphemeral { runner_id } => {
                warn!(
                    "Killing orphan qontinui-runner.exe PID {} from {:?} at startup \
                     (image path is temp runner '{}'; ephemerals are supervisor-owned)",
                    pid, exe_path, runner_id
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Killing orphan qontinui-runner.exe PID {} (temp runner '{}')",
                            pid, runner_id
                        ),
                    )
                    .await;
                if kill_by_pid(pid).await.unwrap_or(false) {
                    summary.killed += 1;
                }
            }
            UnmatchedOrphanAction::KillUnowned => {
                // No registered runner claims this PID by port OR by image
                // path. This is a true orphan holding a slot/copy binary the
                // next build needs. Kill it.
                warn!(
                    "Killing orphan qontinui-runner.exe PID {} from {:?} at startup \
                     (no registered runner claims it by port or image path)",
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
            UnmatchedOrphanAction::SkipProbeUnknown => {
                summary.skipped_unknown += 1;
                let msg = format!(
                    "Orphan qontinui-runner.exe PID {} from {:?}: the netstat listener probe \
                     could NOT RUN, and its image path matches no registered runner. Ownership \
                     is UNKNOWN, so it is left running — a failed probe must never read as \
                     'unclaimed'. A genuinely stuck slot binary is still freed by the next \
                     build (free_slot_exe).",
                    pid, exe_path
                );
                warn!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                    .await;
            }
        }
    }

    info!(
        "Startup orphan scan: {} runner(s) found, {} adopted, {} killed, {} left alone \
         (ownership unknown)",
        summary.found, summary.adopted, summary.killed, summary.skipped_unknown
    );
}

/// Resolve one orphan PID that a registered runner claims — by port
/// (`port = Some(..)`) or by image path (`port = None`) — and apply the
/// resulting action.
///
/// Both discovery paths funnel through here so the adopt/kill decision, the
/// staleness reporting and the log vocabulary cannot drift apart.
#[cfg(target_os = "windows")]
async fn apply_registered_orphan(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    pid: u32,
    exe_path: &Path,
    port: Option<u16>,
    summary: &mut ScanSummary,
) {
    // `via` labels how we identified the owner, so the log says which
    // evidence was used rather than implying a port match that never happened.
    let via = match port {
        Some(p) => format!("port {p}"),
        None => format!("image path {exe_path:?}"),
    };

    // Both mtimes are gathered up-front so the adopt/kill call is a pure,
    // unit-testable seam. They are only a handful of `stat`s over the slot
    // dirs + LKG, once per orphan.
    let is_temp = managed.config.kind().is_temp();
    let orphan_mtime = path_mtime_utc(exe_path);
    let fresh_mtime = freshest_source_mtime(state).await;
    let stale_gap_secs =
        match resolve_registered_orphan(is_temp, orphan_mtime, fresh_mtime, STALENESS_GAP_SECS) {
            OrphanAction::KillEphemeral => {
                warn!(
                    "Killing orphan qontinui-runner.exe PID {} from {:?} at startup \
                     ({} maps to temp runner '{}', not adopting ephemerals)",
                    pid, exe_path, via, managed.config.id
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Killing orphan qontinui-runner.exe PID {} (temp runner '{}', {})",
                            pid, managed.config.id, via
                        ),
                    )
                    .await;
                if kill_by_pid(pid).await.unwrap_or(false) {
                    summary.killed += 1;
                }
                return;
            }
            OrphanAction::Adopt { stale_gap_secs } => stale_gap_secs,
        };

    // User-owned registered runner. Adopt: stuff this PID into the runner's
    // RunnerState so the rest of the supervisor sees it as healthy.
    // `process: Option<Child>` stays None — we never had a
    // `tokio::process::Child` handle for an orphan, and downstream code that
    // polls health via HTTP is unaffected.
    if adopt_pid_into_registry(managed, pid).await {
        let msg = format!(
            "Adopted orphan runner '{}' (PID {}, matched by {}) into registry at startup",
            managed.config.id, pid, via
        );
        info!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, msg)
            .await;
        summary.adopted += 1;

        // Staleness is REPORTED, never enforced. The operator decides
        // whether losing the session is worth the newer binary; the
        // supervisor does not decide for them by killing it. (The same
        // information is also on `GET /runners` as `stale_binary` plus the
        // strictly better commit-based `build_sha` / `build_source` /
        // `build_source_root` / `build_built_at`.)
        if let Some(gap) = stale_gap_secs {
            let orphan_iso = orphan_mtime
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "unknown".to_string());
            let fresh_iso = fresh_mtime
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "unknown".to_string());
            let msg = format!(
                "Adopted runner '{}' (PID {}) is running a STALE binary — \
                 its exe mtime {} is {}s older than the freshest source binary {}. \
                 Adopted anyway (the session is yours to keep). To pick up the \
                 newer build: POST /runner/restart {{\"rebuild\": true}}",
                managed.config.id, pid, orphan_iso, gap, fresh_iso
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
    } else {
        // Adoption was raced by another path that filled in the PID first —
        // treat as a no-op success.
        info!(
            "Adoption of PID {} for runner '{}' skipped — registry already populated",
            pid, managed.config.id
        );
    }
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

/// Probe every registered runner's port for the listening PID.
///
/// This is deliberately bounded by the registered-runner count (~5)
/// rather than running a global `netstat | findstr <pid>` because the
/// only ports we can adopt against are ports that already have a runner
/// config — anything else is unadoptable by definition.
///
/// Returns [`PortProbe::Unknown`] as soon as any single probe fails to run.
/// A failed probe means "could not ask", which is NOT "port idle": once one
/// probe has failed we can no longer claim that *no* registered port is bound
/// by this PID, so the whole answer degrades to UNKNOWN and the caller must
/// not kill on it. (`kill_by_pid` on an unclaimed-looking PID is how one
/// failed `cmd.exe` spawn used to reap every runner in a single pass.)
///
/// Defensive against the impossible-but-defined case where two PIDs both
/// claim the same port: `find_pid_on_port` returns the first listener
/// netstat reports, which is good enough for adoption — the other PID
/// either gets caught by a subsequent registered-port probe or falls
/// through to the image-path ownership check.
#[cfg(target_os = "windows")]
async fn listening_port_for_pid(pid: u32, runners: &[Arc<ManagedRunner>]) -> PortProbe {
    for r in runners {
        let port = r.config.port;
        match find_pid_on_port(port).await {
            Ok(Some(p)) if p == pid => return PortProbe::Matched(port),
            Ok(_) => {}
            Err(e) => {
                warn!(
                    "Orphan scan: listener probe for port {} could not run ({}) — treating \
                     port ownership for PID {} as UNKNOWN (never a kill)",
                    port, e, pid
                );
                return PortProbe::Unknown;
            }
        }
    }
    PortProbe::NoMatch
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

    /// **The incident-encoding test.** A user-owned runner whose exe is an
    /// hour older than the freshest slot/LKG binary — the routine case on
    /// this fleet, since any peer's `spawn-test {rebuild:true}` stamps a
    /// newer slot exe — must be ADOPTED, with the staleness reported. Before
    /// 2026-07-28 this branch killed it ("user's session will be lost"),
    /// which undid the JobObject ownership split five seconds after
    /// supervisor start.
    #[test]
    fn stale_durable_orphan_is_adopted_not_killed() {
        let orphan = Some(ts(2026, 4, 27, 11, 0, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(false, orphan, fresh, STALENESS_GAP_SECS),
            OrphanAction::Adopt {
                stale_gap_secs: Some(3600)
            }
        );
    }

    #[test]
    fn temp_orphan_is_always_killed_regardless_of_mtime() {
        // Ephemerals are supervisor-owned: killed whether their binary is
        // current, stale, or unclassifiable.
        let t = Some(ts(2026, 4, 27, 12, 0, 0));
        let older = Some(ts(2026, 4, 27, 11, 0, 0));
        assert_eq!(
            resolve_registered_orphan(true, t, t, STALENESS_GAP_SECS),
            OrphanAction::KillEphemeral
        );
        assert_eq!(
            resolve_registered_orphan(true, older, t, STALENESS_GAP_SECS),
            OrphanAction::KillEphemeral
        );
        assert_eq!(
            resolve_registered_orphan(true, None, None, STALENESS_GAP_SECS),
            OrphanAction::KillEphemeral
        );
    }

    #[test]
    fn unknown_orphan_mtime_adopts_without_a_staleness_claim() {
        // Missing orphan mtime → adopt, and don't claim staleness we can't
        // substantiate.
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(false, None, fresh, STALENESS_GAP_SECS),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
    }

    #[test]
    fn no_source_on_disk_adopts_without_a_staleness_claim() {
        // No source binary anywhere → the orphan is the only artifact and
        // there is nothing to compare against.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(false, orphan, None, STALENESS_GAP_SECS),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
    }

    #[test]
    fn orphan_newer_than_source_is_not_stale() {
        // Orphan was built after the freshest source on disk (e.g. cleared
        // slot dirs since spawn).
        let orphan = Some(ts(2026, 4, 27, 12, 5, 0));
        let fresh = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(false, orphan, fresh, STALENESS_GAP_SECS),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
    }

    #[test]
    fn orphan_at_tolerance_boundary_is_not_stale() {
        // Exactly 30s older — the check is `orphan + GAP < fresh`, so the
        // boundary itself is inside the jitter window. 29s likewise.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(
                false,
                orphan,
                Some(ts(2026, 4, 27, 12, 0, 30)),
                STALENESS_GAP_SECS
            ),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
        assert_eq!(
            resolve_registered_orphan(
                false,
                orphan,
                Some(ts(2026, 4, 27, 12, 0, 29)),
                STALENESS_GAP_SECS
            ),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
    }

    /// Build the image-path identity table the scan derives from the
    /// registry, using the real `runner_exe_copy_path` shapes:
    /// id-keyed for `Primary`/`External`, port-keyed for `Temp`/`Named`.
    fn registry_fixture() -> Vec<RegisteredExe> {
        vec![
            RegisteredExe {
                runner_id: "primary".to_string(),
                is_temp: false,
                exe_copy_path: PathBuf::from(
                    r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-primary.exe",
                ),
            },
            RegisteredExe {
                runner_id: "named-9880-abc".to_string(),
                is_temp: false,
                exe_copy_path: PathBuf::from(
                    r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-named-9880.exe",
                ),
            },
            RegisteredExe {
                runner_id: "test-abc123".to_string(),
                is_temp: true,
                exe_copy_path: PathBuf::from(
                    r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-test-9877.exe",
                ),
            },
        ]
    }

    /// **The Finding-1 test.** The operator's primary is identified from its
    /// exe copy path alone — no live socket, no netstat, no port match. This
    /// is what protects a primary that is alive but not yet LISTENING (e.g.
    /// stuck in PG bootstrap) from the fallthrough kill.
    #[test]
    fn primary_copy_path_is_owned_and_non_temp() {
        let owner = classify_exe_owner(
            Path::new(r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-primary.exe"),
            &registry_fixture(),
        );
        assert_eq!(
            owner,
            ExeOwner::Registered {
                runner_id: "primary".to_string(),
                is_temp: false
            }
        );
    }

    /// A named runner's port-keyed copy path is likewise owned and non-temp.
    #[test]
    fn named_copy_path_is_owned_and_non_temp() {
        let owner = classify_exe_owner(
            Path::new(
                r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-named-9880.exe",
            ),
            &registry_fixture(),
        );
        assert_eq!(
            owner,
            ExeOwner::Registered {
                runner_id: "named-9880-abc".to_string(),
                is_temp: false
            }
        );
    }

    /// A temp runner's copy path is owned, and flagged temp so it keeps
    /// getting reaped — the ownership split must not save ephemerals.
    #[test]
    fn temp_copy_path_is_owned_and_temp() {
        let owner = classify_exe_owner(
            Path::new(
                r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-test-9877.exe",
            ),
            &registry_fixture(),
        );
        assert_eq!(
            owner,
            ExeOwner::Registered {
                runner_id: "test-abc123".to_string(),
                is_temp: true
            }
        );
    }

    /// A process running straight out of a build-pool slot matches no
    /// registered runner's copy path — a genuine unowned orphan holding a
    /// binary the next build needs.
    #[test]
    fn slot_pool_exe_is_unowned() {
        let owner = classify_exe_owner(
            Path::new(
                r"D:\qontinui-root\qontinui-runner\target-pool\slot-1\debug\qontinui-runner.exe",
            ),
            &registry_fixture(),
        );
        assert_eq!(owner, ExeOwner::Unowned);
    }

    /// Windows paths are case-insensitive, and neither path needs to exist on
    /// disk (a running process's image can be deleted out from under it).
    #[test]
    fn exe_owner_match_is_case_insensitive_for_missing_files() {
        let owner = classify_exe_owner(
            Path::new(r"d:\QONTINUI-ROOT\Qontinui-Runner\Target\Debug\QONTINUI-RUNNER-PRIMARY.EXE"),
            &registry_fixture(),
        );
        assert_eq!(
            owner,
            ExeOwner::Registered {
                runner_id: "primary".to_string(),
                is_temp: false
            }
        );
    }

    /// **The Finding-1(b) test.** An UNKNOWN port probe can never produce a
    /// kill, for ANY image-path ownership. "Could not ask" is not "nobody
    /// claims it" — one failed `cmd.exe` spawn used to make every registered
    /// port look unclaimed and reap the whole fleet in a single pass.
    #[test]
    fn unknown_probe_never_yields_a_kill() {
        for owner in [
            ExeOwner::Unowned,
            ExeOwner::Registered {
                runner_id: "test-abc123".to_string(),
                is_temp: true,
            },
            ExeOwner::Registered {
                runner_id: "primary".to_string(),
                is_temp: false,
            },
        ] {
            let action = decide_unmatched_orphan(PortProbe::Unknown, &owner);
            assert!(
                !matches!(
                    action,
                    UnmatchedOrphanAction::KillEphemeral { .. }
                        | UnmatchedOrphanAction::KillUnowned
                ),
                "UNKNOWN probe must never decide a kill, got {action:?} for {owner:?}"
            );
        }
    }

    /// Deterministic identity OUTRANKS the probe: a registered non-temp image
    /// path is adopted even when the probe could not run at all.
    #[test]
    fn non_temp_image_path_is_adopted_even_when_probe_is_unknown() {
        let owner = ExeOwner::Registered {
            runner_id: "primary".to_string(),
            is_temp: false,
        };
        for probe in [PortProbe::Unknown, PortProbe::NoMatch] {
            assert_eq!(
                decide_unmatched_orphan(probe, &owner),
                UnmatchedOrphanAction::AdoptByExePath {
                    runner_id: "primary".to_string()
                },
                "probe {probe:?}"
            );
        }
    }

    /// With a WORKING probe that found no registered listener, the two kill
    /// classes are preserved: temps by image path, and true unowned orphans.
    #[test]
    fn working_probe_still_kills_temps_and_unowned_orphans() {
        assert_eq!(
            decide_unmatched_orphan(
                PortProbe::NoMatch,
                &ExeOwner::Registered {
                    runner_id: "test-abc123".to_string(),
                    is_temp: true
                }
            ),
            UnmatchedOrphanAction::KillEphemeral {
                runner_id: "test-abc123".to_string()
            }
        );
        assert_eq!(
            decide_unmatched_orphan(PortProbe::NoMatch, &ExeOwner::Unowned),
            UnmatchedOrphanAction::KillUnowned
        );
    }

    #[test]
    fn tolerance_parameter_plumbs_through() {
        // tolerance=0 reduces to strict-less-than: equal mtimes are current,
        // 1s older is reported stale. Sanity check that the parameter is
        // actually used and the reported gap is the real one.
        let orphan = Some(ts(2026, 4, 27, 12, 0, 0));
        assert_eq!(
            resolve_registered_orphan(false, orphan, orphan, 0),
            OrphanAction::Adopt {
                stale_gap_secs: None
            }
        );
        assert_eq!(
            resolve_registered_orphan(false, orphan, Some(ts(2026, 4, 27, 12, 0, 1)), 0),
            OrphanAction::Adopt {
                stale_gap_secs: Some(1)
            }
        );
    }
}

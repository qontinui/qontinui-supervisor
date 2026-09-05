//! Durable record of the temp-runner processes **this supervisor lineage
//! actually spawned**, and the pure decision the reconcile sweep makes from it.
//!
//! # Why this exists
//!
//! [`crate::process::manager::reconcile_orphaned_temp_runners`] is the safety
//! net for the D7 orphan leak: it frees a temp-runner port whose registry
//! record was dropped without a kill. Its ownership test was *"listening on
//! 9877-9899, **not in my registry**, and a `qontinui-runner` image"* — three
//! conditions a **hand-launched** runner satisfies by construction. So the
//! sweep reaped runners it had never owned, silently, on a five-minute timer.
//!
//! That cost a day of misdiagnosis on 2026-08-19 (plan
//! `2026-08-19-session-info-dropdown-mount-gaps-remediation`, D3): a runner
//! being driven through the UI Bridge died three times in ten minutes with no
//! panic, no shutdown log and no Windows error event, and the investigation
//! went looking for a webview teardown bug that did not exist.
//!
//! "Not in my registry" is the wrong question. The right one is **"did I spawn
//! it?"**, and answering that across a supervisor restart needs a record that
//! outlives the process — hence a file rather than an in-memory set. The
//! safety net keeps working: a temp runner this supervisor spawned is in the
//! ledger whether or not its registry record survived, which is exactly the
//! D7 case. What stops working is reaping strangers.
//!
//! # Fail-closed, in the direction of NOT killing
//!
//! Every uncertainty here resolves to [`SweepVerdict::LeaveUnowned`]. An
//! unreadable ledger, a missing record, a PID that does not match the recorded
//! one, or a start time that says the PID was recycled all mean *leave it
//! alone and say why*. The cost of a false negative is a leaked port that the
//! next sweep re-examines; the cost of a false positive is killing someone
//! else's live work with no log line naming the victim. Those are not
//! symmetric.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One temp runner this supervisor spawned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRecord {
    pub pid: u32,
    pub port: u16,
    /// The OS-reported process start time, in seconds. `None` when sysinfo
    /// could not resolve the process at record time (a spawn that died
    /// instantly, or a platform that declines to answer) — recorded as absent
    /// rather than as zero, because zero would silently match a later
    /// recycled PID whose start time also failed to resolve.
    pub start_time_secs: Option<u64>,
    /// When the supervisor wrote this row. Diagnostic only; never a predicate.
    pub recorded_at_secs: u64,
}

/// What the sweep should do with one listener it found on a temp port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepVerdict {
    /// This supervisor spawned it and it is genuinely orphaned — free the port.
    Kill { reason: String },
    /// Not ours, or not provably ours. Leave it running and report why.
    LeaveUnowned { reason: String },
}

impl SweepVerdict {
    pub fn is_kill(&self) -> bool {
        matches!(self, SweepVerdict::Kill { .. })
    }

    pub fn reason(&self) -> &str {
        match self {
            SweepVerdict::Kill { reason } | SweepVerdict::LeaveUnowned { reason } => reason,
        }
    }
}

/// The whole decision, as a pure function of what was observed and what was
/// recorded — so every branch is unit-testable with no process, no port and no
/// filesystem.
///
/// `observed_start_secs` is the live process's start time, `None` when it could
/// not be read. A `None` on **either** side skips the PID-reuse check rather
/// than failing it: the check exists to catch a *proven* mismatch, and an
/// unanswerable probe proves nothing. The `(port, pid)` match still has to
/// hold, so an unknown start time never widens the kill set beyond PIDs this
/// supervisor wrote down.
pub fn classify_listener(
    pid: u32,
    port: u16,
    observed_start_secs: Option<u64>,
    records: &[SpawnRecord],
) -> SweepVerdict {
    let Some(record) = records.iter().find(|r| r.port == port) else {
        return SweepVerdict::LeaveUnowned {
            reason: format!(
                "no spawn record for port {port} — this supervisor lineage never spawned a \
                 temp runner here (a hand-launched runner looks exactly like this)"
            ),
        };
    };

    if record.pid != pid {
        return SweepVerdict::LeaveUnowned {
            reason: format!(
                "port {port} was spawned by PID {} but the live listener is PID {pid} — \
                 something this supervisor did not spawn is holding the port",
                record.pid
            ),
        };
    }

    if let (Some(recorded), Some(observed)) = (record.start_time_secs, observed_start_secs) {
        if recorded != observed {
            return SweepVerdict::LeaveUnowned {
                reason: format!(
                    "PID {pid} on port {port} started at {observed}, but the runner this \
                     supervisor spawned there started at {recorded} — the PID has been \
                     recycled and this is a different process"
                ),
            };
        }
    }

    SweepVerdict::Kill {
        reason: format!(
            "PID {pid} on port {port} is a temp runner this supervisor spawned \
             (recorded at {}) whose registry record is gone",
            record.recorded_at_secs
        ),
    }
}

/// The OS-reported start time of a live process, in seconds. `None` when the
/// process is gone or sysinfo declines to answer.
pub fn process_start_time_secs(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
    let system =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    system.process(Pid::from_u32(pid)).map(|p| p.start_time())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the ledger lives. `None` when no per-user data directory resolves —
/// in which case ownership is UNKNOWN for every listener and the sweep leaves
/// them all alone, which is the correct fail-closed direction.
pub fn ledger_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| {
        d.join("com.qontinui.supervisor")
            .join("spawned-temp-runners.json")
    })
}

/// Read the ledger. An absent or unreadable file is an empty ledger — which,
/// via [`classify_listener`], means "own nothing, kill nothing".
pub fn load() -> Vec<SpawnRecord> {
    let Some(path) = ledger_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!(
            "temp-runner ledger at {} is unreadable ({}); treating it as empty, so the \
             reconcile sweep will kill nothing this pass",
            path.display(),
            e
        );
        Vec::new()
    })
}

fn store(records: &[SpawnRecord]) {
    let Some(path) = ledger_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(records) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(
                    "could not write the temp-runner ledger at {}: {} — the reconcile sweep \
                     will not recognise this spawn as ours",
                    path.display(),
                    e
                );
            }
        }
        Err(e) => tracing::warn!("could not serialize the temp-runner ledger: {}", e),
    }
}

/// Pure upsert: one record per port, newest wins. Extracted so the
/// replace-don't-append rule is testable without touching a file.
pub fn upsert(records: &mut Vec<SpawnRecord>, record: SpawnRecord) {
    records.retain(|r| r.port != record.port);
    records.push(record);
}

/// Record that this supervisor spawned a temp runner. Called at the one spawn
/// site, for temp runners only — a named or primary runner is outside the
/// swept port range and has no business in this file.
pub fn record_spawn(pid: u32, port: u16) {
    let record = SpawnRecord {
        pid,
        port,
        start_time_secs: process_start_time_secs(pid),
        recorded_at_secs: now_secs(),
    };
    let mut records = load();
    upsert(&mut records, record);
    store(&records);
}

/// Drop the record for a port once its process is confirmed gone. Best-effort:
/// a stale row is harmless because [`classify_listener`] re-checks the live
/// PID and start time before killing anything.
pub fn forget_port(port: u16) {
    let mut records = load();
    let before = records.len();
    records.retain(|r| r.port != port);
    if records.len() != before {
        store(&records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, port: u16, start: Option<u64>) -> SpawnRecord {
        SpawnRecord {
            pid,
            port,
            start_time_secs: start,
            recorded_at_secs: 1_700_000_000,
        }
    }

    #[test]
    fn a_hand_launched_runner_is_never_killed() {
        // The 2026-08-19 incident, as a test: a runner nobody spawned through
        // the supervisor is listening on a temp port. The old predicate killed
        // it. The new one must not.
        let v = classify_listener(4242, 9899, Some(500), &[]);
        assert!(!v.is_kill(), "an unrecorded listener must survive: {v:?}");
        assert!(v.reason().contains("no spawn record for port 9899"));
        assert!(
            v.reason().contains("hand-launched"),
            "the reason must name the case an operator will actually hit: {}",
            v.reason()
        );
    }

    #[test]
    fn a_spawned_runner_whose_record_is_gone_is_killed() {
        // This is the D7 orphan the sweep exists for, and it must keep working.
        let v = classify_listener(4242, 9899, Some(500), &[rec(4242, 9899, Some(500))]);
        assert!(v.is_kill(), "{v:?}");
        assert!(v.reason().contains("registry record is gone"));
    }

    #[test]
    fn a_different_pid_on_a_recorded_port_is_left_alone() {
        let v = classify_listener(7777, 9899, Some(500), &[rec(4242, 9899, Some(500))]);
        assert!(!v.is_kill(), "{v:?}");
        assert!(v.reason().contains("spawned by PID 4242"));
        assert!(v.reason().contains("PID 7777"));
    }

    #[test]
    fn a_recycled_pid_is_left_alone_and_says_so() {
        let v = classify_listener(4242, 9899, Some(900), &[rec(4242, 9899, Some(500))]);
        assert!(!v.is_kill(), "{v:?}");
        assert!(v.reason().contains("recycled"));
        assert!(v.reason().contains("900"));
        assert!(v.reason().contains("500"));
    }

    #[test]
    fn an_unknown_start_time_on_either_side_does_not_block_the_kill() {
        // The reuse check catches a PROVEN mismatch. An unanswerable probe
        // proves nothing, and the (port, pid) match still had to hold.
        assert!(classify_listener(4242, 9899, None, &[rec(4242, 9899, Some(500))]).is_kill());
        assert!(classify_listener(4242, 9899, Some(500), &[rec(4242, 9899, None)]).is_kill());
        assert!(classify_listener(4242, 9899, None, &[rec(4242, 9899, None)]).is_kill());
    }

    #[test]
    fn an_unknown_start_time_never_widens_the_kill_set_to_an_unrecorded_pid() {
        assert!(!classify_listener(9999, 9899, None, &[rec(4242, 9899, None)]).is_kill());
    }

    #[test]
    fn a_record_for_another_port_does_not_authorize_this_one() {
        let v = classify_listener(4242, 9899, Some(500), &[rec(4242, 9877, Some(500))]);
        assert!(!v.is_kill(), "{v:?}");
        assert!(v.reason().contains("no spawn record for port 9899"));
    }

    #[test]
    fn upsert_keeps_one_row_per_port_newest_wins() {
        let mut records = vec![rec(1, 9877, Some(10)), rec(2, 9878, Some(20))];
        upsert(&mut records, rec(3, 9877, Some(30)));
        assert_eq!(records.len(), 3 - 1);
        let row = records.iter().find(|r| r.port == 9877).unwrap();
        assert_eq!(row.pid, 3);
        assert_eq!(row.start_time_secs, Some(30));
        // The unrelated port survives untouched.
        assert_eq!(records.iter().find(|r| r.port == 9878).unwrap().pid, 2);
    }

    #[test]
    fn a_record_round_trips_through_json() {
        // The ledger outlives the process, so its shape is a wire contract.
        let record = rec(4242, 9899, Some(500));
        let json = serde_json::to_string(&record).unwrap();
        let back: SpawnRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }
}

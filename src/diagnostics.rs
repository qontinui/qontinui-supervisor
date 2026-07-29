use crate::process::slot_territory::{KillMethod, SlotMatch};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;

/// Capacity of the in-memory diagnostics ring.
///
/// Raised 200 → 500 because ONE build-cleanup pass can be large: a crashed pool
/// build leaves 1 cargo plus up to `-j N` rustc children, and `N` is 24-32 on
/// the operator's box. At 200 a single recovering slot consumed a sixth of the
/// ring and three slots recovering together wiped all build/restart history —
/// including the verification harness's own evidence. The ring is in-memory and
/// each event is small (a few hundred bytes), so 500 is cheap. The per-pass
/// emission cap in `build_monitor` is the other half of that fix.
///
/// Public so `routes::diagnostics` can clamp `?limit=` to the same number
/// instead of a second hard-coded constant that can drift out of step.
pub const DIAGNOSTICS_BUFFER_SIZE: usize = 500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartSource {
    Manual,
    Watchdog,
}

impl RestartSource {
    /// Returns true if this is a manual (user-initiated) restart.
    pub fn is_manual(&self) -> bool {
        matches!(self, Self::Manual)
    }
}

impl std::fmt::Display for RestartSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => write!(f, "manual request"),
            Self::Watchdog => write!(f, "watchdog"),
        }
    }
}

/// Why the build pool killed the process holding a slot's runner exe open.
///
/// Lives here beside [`RestartSource`] — the established home for small
/// event-payload enums — rather than in `process`, because it describes an
/// event, not a process fact.
// Constructed only from the Windows-gated `free_slot_exe` — same platform
// asymmetry (and same justification) as `DiagnosticEventKind` below.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExeLockKillReason {
    /// No registered runner claimed the PID, by registry entry or image path.
    Orphan,
    /// A registered TEMP runner held the lock and the graceful stop failed, so
    /// the build escalated to a direct kill.
    GracefulStopFailed,
}

// JUSTIFIED ALLOW — the five build-kill variants below (`BuildProcessKilled`,
// `BuildKillFailed`, `ExeLockHolderKilled`, `ExeLockKillFailed`,
// `BuildCleanupSummary`) are emitted
// ONLY from `#[cfg(target_os = "windows")]` code in `build_monitor`, so on Linux
// they are never constructed. `dead_code` reports an unconstructed variant even
// when the enum itself is used and derives `Serialize` (derive-generated matches
// do not count as construction), and in the BINARY crate `pub` does not exempt
// it — so `cargo clippy -- -D warnings` on `ubuntu-latest` would fail on a file
// that is otherwise perfectly cross-platform. Scoped to this enum rather than
// the module so real dead code elsewhere in `diagnostics` is still reported, and
// the serde-shape tests that pin the wire contract still run on Linux.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DiagnosticEventKind {
    // Runner restarts
    RestartStarted {
        source: RestartSource,
        rebuild: bool,
    },
    RestartCompleted {
        source: RestartSource,
        rebuild: bool,
        duration_secs: f64,
        build_duration_secs: Option<f64>,
    },
    RestartFailed {
        source: RestartSource,
        error: String,
    },

    // Build outcomes
    BuildStarted,
    BuildCompleted {
        duration_secs: f64,
        success: bool,
        error: Option<String>,
    },

    // Build-pool kill telemetry. Every process the pool terminates on an
    // operator's box is now an addressable event rather than a log line that
    // scrolls out of the 500-entry ring buffer.
    BuildProcessKilled {
        pid: u32,
        process_name: String,
        cmd_snippet: String,
        slot_id: usize,
        territory: String,
        matched_by: SlotMatch,
        method: KillMethod,
    },
    /// The holder of a slot's `qontinui-runner.exe` that the build killed to
    /// free the artifact.
    ///
    /// Deliberately carries no `process_name`: at both emit sites the holder is
    /// always the slot's `qontinui-runner.exe`, derivable from `exe_path`.
    /// `runner_id` (present only for the registered-temp escalation) and
    /// `reason` carry what actually differs between the two sites.
    ExeLockHolderKilled {
        pid: u32,
        slot_id: usize,
        exe_path: String,
        runner_id: Option<String>,
        reason: ExeLockKillReason,
    },
    /// The holder of a slot's `qontinui-runner.exe` that the build TRIED to kill
    /// and could NOT — the exe-lock counterpart of [`Self::BuildKillFailed`].
    ///
    /// Same shape as [`Self::ExeLockHolderKilled`] plus a `detail`, deliberately
    /// a sibling variant rather than a reuse of `BuildKillFailed`: that one
    /// carries `matched_by` / `territory`, which are sweep concepts with no
    /// meaning for a process that is simply holding a file open.
    ///
    /// Why it must exist at all: `free_slot_exe`'s kill arms treated
    /// `kill_by_pid(..) == Ok(false)` (taskkill RAN and was refused — the
    /// "Access is denied" case) as a no-op, logging only at `debug!`. The holder
    /// then kept the lock, the build failed on the locked artifact, and the
    /// operator got no `warn!` and nothing queryable — the exact diagnosis-blind
    /// shape the sweep path already fixed with `BuildKillFailed`. The two kill
    /// paths must be symmetric in what an operator can query.
    ExeLockKillFailed {
        pid: u32,
        slot_id: usize,
        exe_path: String,
        runner_id: Option<String>,
        /// Which emit site tried the kill — mirrors the successful event, so a
        /// reader can pair a failure with the kill it would have been.
        reason: ExeLockKillReason,
        /// Which path refused and what it said.
        detail: String,
    },
    /// A process the cleanup attributed to this slot, tried to kill, and failed
    /// to kill on EVERY path (native sysinfo kill refused, then taskkill refused
    /// or could not run).
    ///
    /// Higher signal than a successful kill: it is the state in which the build
    /// is about to fail on a locked artifact. It used to produce nothing at all
    /// — the process fell out of the report, `is_empty()` stayed true, and the
    /// pass emitted neither a log line nor an event.
    BuildKillFailed {
        pid: u32,
        process_name: String,
        slot_id: usize,
        territory: String,
        matched_by: SlotMatch,
        /// Which path refused and what it said.
        detail: String,
    },
    /// One per non-quiet cleanup pass, including passes that killed nothing but
    /// spared out-of-territory builds, and passes whose only content was a
    /// failed kill.
    ///
    /// `killed` / `spared` / `failed` are the TRUE counts. `killed_pids` /
    /// `spared_pids` are capped samples (same cap the summary log line uses) —
    /// they exist because a COUNT is not attributable evidence: "the foreign
    /// build was spared" is satisfied by an unrelated sibling-slot rustc, so a
    /// count-only assertion passes even when the specific process under test was
    /// reaped. `unclassifiable` is the subset of `spared` whose ownership could
    /// not be decided at all (safe-degrade), which is what an operator greps for
    /// when a build keeps failing on a lock nobody appears to hold.
    BuildCleanupSummary {
        slot_id: usize,
        territory: String,
        killed: usize,
        spared: usize,
        unclassifiable: usize,
        failed: usize,
        killed_pids: Vec<u32>,
        spared_pids: Vec<u32>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticEvent {
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: DiagnosticEventKind,
}

pub struct DiagnosticsState {
    events: VecDeque<DiagnosticEvent>,
}

impl DiagnosticsState {
    pub fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(DIAGNOSTICS_BUFFER_SIZE),
        }
    }

    pub fn emit(&mut self, kind: DiagnosticEventKind) {
        if self.events.len() >= DIAGNOSTICS_BUFFER_SIZE {
            self.events.pop_front();
        }
        self.events.push_back(DiagnosticEvent {
            timestamp: Utc::now(),
            kind,
        });
    }

    pub fn events(&self, limit: usize, filter: Option<&[String]>) -> Vec<DiagnosticEvent> {
        let iter = self.events.iter().rev();
        if let Some(filters) = filter {
            iter.filter(|e| {
                let category = e.filter_category();
                filters.iter().any(|f| category == f)
            })
            .take(limit)
            .cloned()
            .collect()
        } else {
            iter.take(limit).cloned().collect()
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }
}

impl Default for DiagnosticsState {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticEvent {
    fn filter_category(&self) -> &'static str {
        match &self.kind {
            DiagnosticEventKind::RestartStarted { .. }
            | DiagnosticEventKind::RestartCompleted { .. }
            | DiagnosticEventKind::RestartFailed { .. } => "restart",

            DiagnosticEventKind::BuildStarted | DiagnosticEventKind::BuildCompleted { .. } => {
                "build"
            }

            DiagnosticEventKind::BuildProcessKilled { .. }
            | DiagnosticEventKind::BuildKillFailed { .. }
            | DiagnosticEventKind::ExeLockHolderKilled { .. }
            | DiagnosticEventKind::ExeLockKillFailed { .. }
            | DiagnosticEventKind::BuildCleanupSummary { .. } => "build_kill",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiagnosticEvent, DiagnosticEventKind, DiagnosticsState, ExeLockKillReason, KillMethod,
        SlotMatch, DIAGNOSTICS_BUFFER_SIZE,
    };
    use chrono::Utc;

    /// The exact envelope + field names the Phase 3 verification harness reads.
    ///
    /// `scripts/verify-scoped-cleanup.ps1` is Windows-only and CI is
    /// `ubuntu-latest`, so the harness can never run on the merge gate. It
    /// navigates `$event.kind` / `$event.data.<field>` and string-matches
    /// `slot_id` and `territory` to decide which slot the pool build claimed and
    /// whether a kill was correctly attributed. None of that is pinned by the
    /// `slot_territory` tests, which only cover `KilledProcess`'s own fields —
    /// so without this test a rename of `territory`, or a change to the
    /// `#[serde(tag = "kind", content = "data")]` / `#[serde(flatten)]`
    /// attributes, would leave every Rust test green while silently turning
    /// each harness assertion into an unfindable-pid INCONCLUSIVE.
    #[test]
    fn build_process_killed_serializes_with_the_envelope_the_harness_reads() {
        let event = DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::BuildProcessKilled {
                pid: 4242,
                process_name: "cargo.exe".to_string(),
                cmd_snippet: "cargo.exe check".to_string(),
                slot_id: 1,
                territory: r"D:\qontinui-root\qontinui-runner\target-pool\slot-1".to_string(),
                matched_by: SlotMatch::Env,
                method: KillMethod::Sysinfo,
            },
        };

        let value = serde_json::to_value(&event).unwrap();
        let obj = value.as_object().expect("DiagnosticEvent is a JSON object");

        // The flattened envelope: {timestamp, kind, data}.
        assert!(obj.contains_key("timestamp"), "{value}");
        assert_eq!(obj["kind"], serde_json::json!("build_process_killed"));

        let data = obj["data"].as_object().expect("`data` is a JSON object");
        for key in [
            "pid",
            "process_name",
            "cmd_snippet",
            "slot_id",
            "territory",
            "matched_by",
            "method",
        ] {
            assert!(data.contains_key(key), "missing `{key}` in {value}");
        }
        assert_eq!(data["pid"], serde_json::json!(4242));
        assert_eq!(data["slot_id"], serde_json::json!(1));
        assert_eq!(data["matched_by"], serde_json::json!("env"));
        assert_eq!(data["method"], serde_json::json!("sysinfo"));
        assert_eq!(
            data["territory"],
            serde_json::json!(r"D:\qontinui-root\qontinui-runner\target-pool\slot-1"),
            "the harness matches this on a `slot-<id>` suffix"
        );
    }

    /// `build_cleanup_summary` reports which slot a pool build claimed and — via
    /// `killed_pids` / `spared_pids` — WHICH processes it acted on. The pid
    /// vectors are the attributable evidence a count cannot provide: the V1
    /// assertion "the foreign build survives and is recorded as spared" is
    /// satisfied by any unrelated sibling-slot rustc if all the harness can read
    /// is `spared: 3`.
    #[test]
    fn build_cleanup_summary_serializes_counts_and_attributable_pids() {
        let event = DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::BuildCleanupSummary {
                slot_id: 2,
                territory: "/srv/qontinui-runner/target-pool/slot-2".to_string(),
                killed: 1,
                spared: 3,
                unclassifiable: 1,
                failed: 2,
                killed_pids: vec![11],
                spared_pids: vec![21, 22, 23],
            },
        };

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["kind"], serde_json::json!("build_cleanup_summary"));
        assert_eq!(value["data"]["slot_id"], serde_json::json!(2));
        assert_eq!(
            value["data"]["territory"],
            serde_json::json!("/srv/qontinui-runner/target-pool/slot-2")
        );
        assert_eq!(value["data"]["killed"], serde_json::json!(1));
        assert_eq!(value["data"]["spared"], serde_json::json!(3));
        assert_eq!(value["data"]["unclassifiable"], serde_json::json!(1));
        assert_eq!(value["data"]["failed"], serde_json::json!(2));
        assert_eq!(value["data"]["killed_pids"], serde_json::json!([11]));
        assert_eq!(
            value["data"]["spared_pids"],
            serde_json::json!([21, 22, 23])
        );
    }

    /// `build_kill_failed` is the event for a pass that TRIED and could not
    /// reap — the state in which the build is about to fail on a locked
    /// artifact. It must land in the same `build_kill` category and carry the
    /// detail that says which path refused.
    #[test]
    fn build_kill_failed_serializes_with_detail_and_attribution() {
        let event = DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::BuildKillFailed {
                pid: 4243,
                process_name: "rustc.exe".to_string(),
                slot_id: 1,
                territory: r"D:\qontinui-root\qontinui-runner\target-pool\slot-1".to_string(),
                matched_by: SlotMatch::Argv,
                detail: "taskkill fallback refused the kill".to_string(),
            },
        };

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["kind"], serde_json::json!("build_kill_failed"));
        let data = value["data"].as_object().expect("`data` is a JSON object");
        for key in [
            "pid",
            "process_name",
            "slot_id",
            "territory",
            "matched_by",
            "detail",
        ] {
            assert!(data.contains_key(key), "missing `{key}` in {value}");
        }
        assert_eq!(data["matched_by"], serde_json::json!("argv"));
    }

    /// The exe-lock kill reasons are snake_case on the wire, and `runner_id` is
    /// present-but-null for the orphan case rather than omitted — an operator
    /// reading the event should not have to infer which of the two emit sites
    /// produced it.
    #[test]
    fn exe_lock_holder_killed_serializes_reason_and_nullable_runner_id() {
        let orphan = serde_json::to_value(DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::ExeLockHolderKilled {
                pid: 7,
                slot_id: 0,
                exe_path: r"D:\slot-0\debug\qontinui-runner.exe".to_string(),
                runner_id: None,
                reason: ExeLockKillReason::Orphan,
            },
        })
        .unwrap();
        assert_eq!(orphan["kind"], serde_json::json!("exe_lock_holder_killed"));
        assert_eq!(orphan["data"]["reason"], serde_json::json!("orphan"));
        assert_eq!(orphan["data"]["runner_id"], serde_json::Value::Null);

        let escalated = serde_json::to_value(DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::ExeLockHolderKilled {
                pid: 8,
                slot_id: 0,
                exe_path: r"D:\slot-0\debug\qontinui-runner.exe".to_string(),
                runner_id: Some("test-abc".to_string()),
                reason: ExeLockKillReason::GracefulStopFailed,
            },
        })
        .unwrap();
        assert_eq!(
            escalated["data"]["reason"],
            serde_json::json!("graceful_stop_failed")
        );
        assert_eq!(
            escalated["data"]["runner_id"],
            serde_json::json!("test-abc")
        );
    }

    /// `exe_lock_kill_failed` is the exe-lock counterpart of
    /// `build_kill_failed`: the build tried to kill the holder of the slot's
    /// runner exe and taskkill refused. Without it that outcome produced no
    /// event and no `warn!` at all, and the operator saw only the downstream
    /// "build failed on a locked artifact".
    ///
    /// Pins the field names a harness reads AND the symmetry requirement: it
    /// carries the same attribution as the successful `exe_lock_holder_killed`
    /// (`pid` / `slot_id` / `exe_path` / `runner_id` / `reason`) plus `detail`.
    #[test]
    fn exe_lock_kill_failed_serializes_with_detail_and_attribution() {
        let orphan = serde_json::to_value(DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::ExeLockKillFailed {
                pid: 9,
                slot_id: 2,
                exe_path: r"D:\slot-2\debug\qontinui-runner.exe".to_string(),
                runner_id: None,
                reason: ExeLockKillReason::Orphan,
                detail: "taskkill ran and refused the kill".to_string(),
            },
        })
        .unwrap();
        assert_eq!(orphan["kind"], serde_json::json!("exe_lock_kill_failed"));
        let data = orphan["data"].as_object().expect("`data` is a JSON object");
        for key in [
            "pid",
            "slot_id",
            "exe_path",
            "runner_id",
            "reason",
            "detail",
        ] {
            assert!(data.contains_key(key), "missing `{key}` in {orphan}");
        }
        assert_eq!(data["reason"], serde_json::json!("orphan"));
        assert_eq!(data["runner_id"], serde_json::Value::Null);

        let escalated = serde_json::to_value(DiagnosticEvent {
            timestamp: Utc::now(),
            kind: DiagnosticEventKind::ExeLockKillFailed {
                pid: 10,
                slot_id: 2,
                exe_path: r"D:\slot-2\debug\qontinui-runner.exe".to_string(),
                runner_id: Some("test-abc".to_string()),
                reason: ExeLockKillReason::GracefulStopFailed,
                detail: "taskkill could not be run: spawn ENOENT".to_string(),
            },
        })
        .unwrap();
        assert_eq!(
            escalated["data"]["reason"],
            serde_json::json!("graceful_stop_failed")
        );
        assert_eq!(
            escalated["data"]["runner_id"],
            serde_json::json!("test-abc")
        );
    }

    /// All FIVE build-kill events share the `build_kill` filter category, and
    /// none of them leaks into `build` or `restart`.
    ///
    /// The harness fetches `GET /diagnostics?filter=build_kill`; a
    /// miscategorized variant would simply never be returned, and every
    /// assertion that reads it would silently degrade to "no event names that
    /// pid" — i.e. a false INCONCLUSIVE, or a false PASS on the negative
    /// assertions.
    #[test]
    fn build_kill_filter_returns_exactly_the_five_kill_events() {
        let mut state = DiagnosticsState::new();
        state.emit(DiagnosticEventKind::BuildStarted);
        state.emit(DiagnosticEventKind::BuildProcessKilled {
            pid: 1,
            process_name: "cargo.exe".to_string(),
            cmd_snippet: String::new(),
            slot_id: 0,
            territory: r"D:\pool\slot-0".to_string(),
            matched_by: SlotMatch::Env,
            method: KillMethod::Sysinfo,
        });
        state.emit(DiagnosticEventKind::BuildKillFailed {
            pid: 3,
            process_name: "rustc.exe".to_string(),
            slot_id: 0,
            territory: r"D:\pool\slot-0".to_string(),
            matched_by: SlotMatch::Env,
            detail: "access denied".to_string(),
        });
        state.emit(DiagnosticEventKind::ExeLockHolderKilled {
            pid: 2,
            slot_id: 0,
            exe_path: r"D:\pool\slot-0\debug\qontinui-runner.exe".to_string(),
            runner_id: None,
            reason: ExeLockKillReason::Orphan,
        });
        state.emit(DiagnosticEventKind::ExeLockKillFailed {
            pid: 6,
            slot_id: 0,
            exe_path: r"D:\pool\slot-0\debug\qontinui-runner.exe".to_string(),
            runner_id: None,
            reason: ExeLockKillReason::Orphan,
            detail: "taskkill ran and refused the kill".to_string(),
        });
        state.emit(DiagnosticEventKind::BuildCleanupSummary {
            slot_id: 0,
            territory: r"D:\pool\slot-0".to_string(),
            killed: 1,
            spared: 2,
            unclassifiable: 0,
            failed: 1,
            killed_pids: vec![1],
            spared_pids: vec![4, 5],
        });

        let events = state.events(200, Some(&["build_kill".to_string()]));
        assert_eq!(events.len(), 5, "BuildStarted must not be included");

        let build_only = state.events(200, Some(&["build".to_string()]));
        assert_eq!(
            build_only.len(),
            1,
            "the `build` category must not absorb the kill events"
        );
    }

    /// The ring holds 500 entries. One cleanup pass on a `-j 32` box can emit
    /// dozens of events, and at 200 three recovering slots wiped every build and
    /// restart record — including the evidence the verification harness reads.
    #[test]
    fn ring_retains_the_documented_capacity() {
        let mut state = DiagnosticsState::new();
        for _ in 0..(DIAGNOSTICS_BUFFER_SIZE + 25) {
            state.emit(DiagnosticEventKind::BuildStarted);
        }
        assert_eq!(
            state.events(usize::MAX, None).len(),
            DIAGNOSTICS_BUFFER_SIZE,
            "the ring must hold exactly DIAGNOSTICS_BUFFER_SIZE entries"
        );
        // Compile-time floor: a single `-j N` cleanup pass (up to ~33 events)
        // must never be able to dominate the ring. A `const` block because the
        // comparison is constant — clippy rejects a runtime `assert!` on it.
        const { assert!(DIAGNOSTICS_BUFFER_SIZE >= 500) };
    }
}

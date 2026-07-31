//! What a runner stop actually DID, and how it says so when it fails.
//!
//! **Cross-platform on purpose** (same rationale as [`super::slot_territory`]
//! and `super::netstat_parse`): the stop ladder itself is Windows-specific,
//! CI is `ubuntu-latest`, so the reporting logic lives here where the merge
//! gate can execute its tests.
//!
//! # The defect this module exists to prevent
//!
//! `stop_runner_by_id` used to end a failed stop with a fixed sentence:
//!
//! ```text
//! Runner 'Primary' stop could not be confirmed: port 9876 is still in use
//! after PID kill, tree-kill, kill-by-port, and a 4s backoff re-check
//! ```
//!
//! On 2026-07-31 that message was **false in every clause**. The registry held
//! `pid: null` for the adopted primary and the netstat listener probe was
//! locale-blind (see `super::netstat_parse`), so `pid_kill`, `tree_kill` and
//! `kill_by_port` each had **no target to run against** and quietly did
//! nothing. The message described a stubborn process resisting three kills; the
//! reality was a supervisor that never issued one. Diagnosis went looking for
//! an unkillable process — hours in the wrong direction.
//!
//! A failure report that cannot distinguish "we tried and were refused" from
//! "we had nothing to try" is worse than no report: it actively misdirects. So
//! the ladder records each rung as it goes and the message is *derived* from
//! that record, with the no-target case called out as a supervisor defect
//! rather than a stubborn process.

use std::fmt;

/// One rung of the stop ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStrategy {
    /// HTTP close-request to the runner's own endpoint.
    GracefulClose,
    /// `taskkill /F /PID <pid>`.
    PidKill,
    /// `taskkill /F /T /PID <pid>` — the PID and its whole child tree.
    TreeKill,
    /// Kill whatever is listening on the port, whoever it is.
    KillByPort,
}

impl fmt::Display for StopStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            StopStrategy::GracefulClose => "graceful-close",
            StopStrategy::PidKill => "pid-kill",
            StopStrategy::TreeKill => "tree-kill",
            StopStrategy::KillByPort => "kill-by-port",
        };
        f.write_str(s)
    }
}

/// What became of one rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopStrategyOutcome {
    /// The strategy ran against a concrete target. `killed` is what the OS
    /// reported — `false` means it ran and was REFUSED, which is a genuinely
    /// different fact from never having run.
    Ran { target: String, killed: bool },
    /// The strategy could not run: no target was known, or the probe that
    /// would have supplied one failed. `why` must name which.
    NoTarget { why: String },
}

/// Ordered record of every rung the stop ladder attempted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopLedger {
    entries: Vec<(StopStrategy, StopStrategyOutcome)>,
}

impl StopLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a rung that ran against `target` (a PID, a port — whatever it
    /// aimed at), with the OS's verdict.
    pub fn ran(&mut self, strategy: StopStrategy, target: impl Into<String>, killed: bool) {
        self.entries.push((
            strategy,
            StopStrategyOutcome::Ran {
                target: target.into(),
                killed,
            },
        ));
    }

    /// Record a rung that had nothing to aim at. `why` must explain the gap
    /// (no PID in the registry, probe failed, …) — this is the string that
    /// turns a misleading failure into a diagnosable one.
    pub fn no_target(&mut self, strategy: StopStrategy, why: impl Into<String>) {
        self.entries
            .push((strategy, StopStrategyOutcome::NoTarget { why: why.into() }));
    }

    /// True when at least one KILL strategy actually executed against a
    /// target. `GracefulClose` deliberately does not count: it is a request,
    /// not a kill, and a stop that only ever asked politely has not exhausted
    /// anything.
    pub fn any_kill_executed(&self) -> bool {
        self.entries.iter().any(|(s, o)| {
            !matches!(s, StopStrategy::GracefulClose)
                && matches!(o, StopStrategyOutcome::Ran { .. })
        })
    }

    /// Human-readable "what we tried" clause, one rung per item, in order.
    pub fn attempts_clause(&self) -> String {
        if self.entries.is_empty() {
            return "no strategies were attempted".to_string();
        }
        self.entries
            .iter()
            .map(|(strategy, outcome)| match outcome {
                // `GracefulClose` gets its own vocabulary: it is a REQUEST,
                // not a kill. Rendering a runner that simply did not exit
                // within the window as "REFUSED" asserts it actively declined,
                // which we cannot know — and this whole module exists to stop
                // the failure message asserting things it cannot know.
                StopStrategyOutcome::Ran { target, killed } => match (strategy, killed) {
                    (StopStrategy::GracefulClose, true) => {
                        format!("{strategy} against {target} (the runner exited)")
                    }
                    (StopStrategy::GracefulClose, false) => {
                        format!("{strategy} against {target} (no exit within the window)")
                    }
                    (_, true) => format!("{strategy} against {target} (reported success)"),
                    (_, false) => format!("{strategy} against {target} (REFUSED)"),
                },
                StopStrategyOutcome::NoTarget { why } => {
                    format!("{strategy} DID NOT RUN ({why})")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The operator-facing message for a stop that could not be confirmed.
    ///
    /// Branches on [`Self::any_kill_executed`] because the two cases need
    /// different remedies and used to be indistinguishable:
    ///
    /// * some kill ran and the port is still held → a genuinely stubborn
    ///   process (elevated, different user, wedged in kernel I/O);
    /// * nothing ran → the supervisor could not identify a target, which is a
    ///   supervisor defect and is where the reader must look first.
    pub fn failure_message(&self, runner_name: &str, port: u16, backoff_secs: u64) -> String {
        if self.any_kill_executed() {
            format!(
                "Runner '{runner_name}' stop could not be confirmed: port {port} is still in \
                 use after every kill escalation and a {backoff_secs}s backoff re-check — \
                 refusing to report success. Attempted: {}.",
                self.attempts_clause()
            )
        } else {
            format!(
                "Runner '{runner_name}' stop could not be confirmed: port {port} is still in \
                 use, and NO kill strategy ever ran against a target — the supervisor could \
                 not identify what to kill, so this is a supervisor-side identification \
                 failure, NOT a process that resisted being killed. Do not go looking for an \
                 unkillable process. Ladder: {}.",
                self.attempts_clause()
            )
        }
    }
}

/// Where a stop's target PID came from, so the log can say how it was
/// identified (or why it could not be).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidSource {
    /// The registry already tracked it.
    Registry,
    /// The netstat listener probe on the runner's port named it.
    ListenerProbe,
    /// Resolved by image path: the process is running the runner's own
    /// deterministic exe copy. Locale-independent and subprocess-free — the
    /// backstop for when the listener probe cannot answer.
    ExeIdentity,
}

impl fmt::Display for PidSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PidSource::Registry => "the registry",
            PidSource::ListenerProbe => "the netstat listener probe",
            PidSource::ExeIdentity => "image-path identity (sysinfo)",
        };
        f.write_str(s)
    }
}

/// Outcome of trying to identify the process a stop should target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTarget {
    /// A PID was identified, and by which route.
    Found { pid: u32, source: PidSource },
    /// No PID could be identified. `why` accumulates what each source said, so
    /// the failure names the blind spot instead of hiding it.
    NotFound { why: String },
}

/// Decide the stop target from the three identification sources, in
/// preference order, accumulating the reason each one came up empty.
///
/// Pure so the precedence and the "why" accounting are covered by CI. The
/// arguments mirror the three sources exactly:
///
/// * `registry_pid` — what the registry tracks (`None` for an ADOPTED runner
///   whose PID recovery never succeeded — the 2026-07-31 case);
/// * `listener_pid` — `Ok(Some)`/`Ok(None)`/`Err` from the port probe, where
///   `Err` is UNKNOWN and must never read as "nothing there";
/// * `exe_identity_pids` — PIDs whose image is the runner's exe copy.
///
/// The exe-identity source is deliberately last: it is the most robust
/// (no subprocess, no locale, no listening socket required) but the least
/// specific, since it cannot distinguish two processes running the same image.
/// When it names exactly one PID that is unambiguous; when it names several,
/// we refuse rather than guess which of the operator's processes to kill.
pub fn resolve_stop_target(
    registry_pid: Option<u32>,
    listener_pid: Result<Option<u32>, String>,
    exe_identity_pids: &[u32],
) -> StopTarget {
    let mut gaps: Vec<String> = Vec::new();

    if let Some(pid) = registry_pid {
        return StopTarget::Found {
            pid,
            source: PidSource::Registry,
        };
    }
    gaps.push("the registry tracks no PID for this runner".to_string());

    match listener_pid {
        Ok(Some(pid)) => {
            return StopTarget::Found {
                pid,
                source: PidSource::ListenerProbe,
            }
        }
        Ok(None) => gaps.push("the listener probe ran and found nothing on the port".to_string()),
        Err(e) => gaps.push(format!("the listener probe could not answer ({e})")),
    }

    match exe_identity_pids {
        [] => gaps.push("no live process is running this runner's exe copy".to_string()),
        [pid] => {
            return StopTarget::Found {
                pid: *pid,
                source: PidSource::ExeIdentity,
            }
        }
        many => gaps.push(format!(
            "{} live processes share this runner's exe copy ({many:?}) — refusing to guess \
             which one to kill",
            many.len()
        )),
    }

    StopTarget::NotFound {
        why: gaps.join("; "),
    }
}

/// Verdict of re-checking, immediately before the kill, that the identified
/// PID is still the runner we meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetVerification {
    /// Proceed with the kill.
    Proceed { note: Option<String> },
    /// Do NOT kill: the PID is demonstrably running a different program.
    Refuse { why: String },
}

/// Decide whether a resolved stop target may still be killed.
///
/// Two independent reasons this check exists, both raised in pre-PR review:
///
/// 1. **The listener probe names peerless sockets, not strictly listeners.**
///    A foreign process that merely `bind()`s the runner's port appears in
///    `netstat` with the same row shape as a listener (see
///    `super::netstat_parse`), so a probe-derived PID is a *candidate*, not an
///    established identity.
/// 2. **Identification happens up to ~34s before the kill** — the pre-stop
///    drain (30s cap), the close-request, and the graceful poll all run in
///    between. On a box that churns hundreds of `claude.exe` and PTY
///    processes, a PID freed in that window can be reused, and the thing we
///    would `taskkill /F` is then somebody else's process.
///
/// A `Registry` PID is exempt: it is backed by the spawn path this supervisor
/// itself drove, it was the pre-existing behaviour, and adding a new refusal
/// arm there could only take away a kill that already worked.
///
/// `observed_exe` of `None` means the image could not be read — almost always
/// because the process already exited, in which case the kill is a harmless
/// no-op. That is deliberately NOT a refusal: an unreadable image must not
/// cost us the ability to stop a runner.
pub fn verify_target_image(
    source: &PidSource,
    observed_exe: Option<&std::path::Path>,
    expected_exe: &std::path::Path,
) -> TargetVerification {
    if matches!(source, PidSource::Registry) {
        return TargetVerification::Proceed { note: None };
    }
    let Some(observed) = observed_exe else {
        return TargetVerification::Proceed {
            note: Some(
                "the target's image path could not be read (it has most likely already \
                 exited); proceeding — the kill is a no-op on a dead PID"
                    .to_string(),
            ),
        };
    };
    let same = observed.to_string_lossy().to_ascii_lowercase()
        == expected_exe.to_string_lossy().to_ascii_lowercase();
    if same {
        TargetVerification::Proceed { note: None }
    } else {
        TargetVerification::Refuse {
            why: format!(
                "the PID identified via {source} is now running {observed:?}, not this \
                 runner's own {expected_exe:?} — it is either an unrelated process that \
                 holds the port or a reused PID, and killing it would hit the wrong process"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn registry_pid_wins() {
        assert_eq!(
            resolve_stop_target(Some(42), Ok(Some(99)), &[7]),
            StopTarget::Found {
                pid: 42,
                source: PidSource::Registry
            }
        );
    }

    #[test]
    fn listener_probe_is_second() {
        assert_eq!(
            resolve_stop_target(None, Ok(Some(99)), &[7]),
            StopTarget::Found {
                pid: 99,
                source: PidSource::ListenerProbe
            }
        );
    }

    /// THE 2026-07-31 case: adopted primary with no registry PID, on a box
    /// where the listener probe is blind. Image-path identity must still find
    /// the process so the stop has something to kill.
    #[test]
    fn exe_identity_rescues_an_adopted_runner_with_a_blind_probe() {
        assert_eq!(
            resolve_stop_target(None, Err("locale-blind probe".into()), &[8872]),
            StopTarget::Found {
                pid: 8872,
                source: PidSource::ExeIdentity
            }
        );
    }

    /// Same rescue when the probe merely found nothing (a runner that is alive
    /// but not yet bound to its port).
    #[test]
    fn exe_identity_rescues_when_probe_is_empty() {
        assert_eq!(
            resolve_stop_target(None, Ok(None), &[8872]),
            StopTarget::Found {
                pid: 8872,
                source: PidSource::ExeIdentity
            }
        );
    }

    /// Ambiguity must NOT be resolved by guessing — two processes on the same
    /// image means we cannot say which is the operator's runner.
    #[test]
    fn ambiguous_exe_identity_refuses_to_guess() {
        let target = resolve_stop_target(None, Ok(None), &[100, 200]);
        match target {
            StopTarget::NotFound { why } => {
                assert!(why.contains("refusing to guess"), "got: {why}");
                assert!(
                    why.contains("100"),
                    "the ambiguous PIDs must be named: {why}"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Every gap must be named — the whole point is that the failure explains
    /// itself rather than asserting three kills that never happened.
    #[test]
    fn not_found_accumulates_every_gap() {
        let target = resolve_stop_target(None, Err("netstat exploded".into()), &[]);
        match target {
            StopTarget::NotFound { why } => {
                assert!(why.contains("registry tracks no PID"), "got: {why}");
                assert!(why.contains("netstat exploded"), "got: {why}");
                assert!(why.contains("exe copy"), "got: {why}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A probe ERROR and a probe EMPTY must produce different explanations —
    /// conflating them is the original sin this whole change is about.
    #[test]
    fn probe_error_and_probe_empty_read_differently() {
        let err = resolve_stop_target(None, Err("boom".into()), &[]);
        let empty = resolve_stop_target(None, Ok(None), &[]);
        assert_ne!(err, empty);
    }

    #[test]
    fn graceful_close_alone_is_not_a_kill() {
        let mut ledger = StopLedger::new();
        ledger.ran(StopStrategy::GracefulClose, "port 9876", true);
        assert!(
            !ledger.any_kill_executed(),
            "a close-request is not a kill; a stop that only asked politely has \
             exhausted nothing"
        );
    }

    #[test]
    fn a_refused_kill_still_counts_as_executed() {
        let mut ledger = StopLedger::new();
        ledger.ran(StopStrategy::PidKill, "PID 8872", false);
        assert!(ledger.any_kill_executed());
    }

    /// THE regression. When nothing ran, the message must say so and must not
    /// claim the three strategies were tried.
    #[test]
    fn no_target_message_names_the_supervisor_not_the_process() {
        let mut ledger = StopLedger::new();
        ledger.no_target(StopStrategy::PidKill, "no PID known");
        ledger.no_target(StopStrategy::TreeKill, "no PID known");
        ledger.no_target(StopStrategy::KillByPort, "listener probe could not answer");

        let msg = ledger.failure_message("Primary", 9876, 4);
        assert!(
            msg.contains("NO kill strategy ever ran"),
            "the no-target case must be explicit: {msg}"
        );
        assert!(
            msg.contains("supervisor-side identification failure"),
            "the message must point at the supervisor: {msg}"
        );
        assert!(
            msg.contains("listener probe could not answer"),
            "each rung's reason must survive into the message: {msg}"
        );
        assert!(
            !msg.contains("after PID kill, tree-kill, kill-by-port"),
            "must not reproduce the old false claim: {msg}"
        );
    }

    /// The stubborn-process case keeps its own wording, and names what was
    /// refused.
    #[test]
    fn executed_kills_message_reports_refusals() {
        let mut ledger = StopLedger::new();
        ledger.ran(StopStrategy::PidKill, "PID 8872", false);
        ledger.ran(StopStrategy::TreeKill, "PID 8872", false);
        ledger.ran(StopStrategy::KillByPort, "port 9876", false);

        let msg = ledger.failure_message("Primary", 9876, 4);
        assert!(msg.contains("after every kill escalation"), "{msg}");
        assert!(msg.contains("REFUSED"), "{msg}");
        assert!(
            !msg.contains("NO kill strategy ever ran"),
            "must not claim the supervisor found no target: {msg}"
        );
    }

    /// A probe-derived PID that is now running something else must NOT be
    /// killed — this is the `BOUND`-row false positive and the PID-reuse
    /// window, both closed by the same check.
    #[test]
    fn probe_derived_pid_running_a_foreign_image_is_refused() {
        let v = verify_target_image(
            &PidSource::ListenerProbe,
            Some(Path::new(r"C:\Windows\System32\svchost.exe")),
            Path::new(r"D:\qontinui-root\qontinui-runner\target\debug\qontinui-runner-primary.exe"),
        );
        match v {
            TargetVerification::Refuse { why } => {
                assert!(
                    why.contains("svchost.exe"),
                    "must name what it found: {why}"
                );
                assert!(why.contains("wrong process"), "{why}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    /// The matching case proceeds, and Windows path case-insensitivity must
    /// not cause a spurious refusal.
    #[test]
    fn matching_image_proceeds_case_insensitively() {
        let v = verify_target_image(
            &PidSource::ListenerProbe,
            Some(Path::new(
                r"D:\Qontinui-Root\Target\Debug\Qontinui-Runner-Primary.exe",
            )),
            Path::new(r"d:\qontinui-root\target\debug\qontinui-runner-primary.exe"),
        );
        assert_eq!(v, TargetVerification::Proceed { note: None });
    }

    /// A registry PID keeps the pre-existing behaviour unconditionally — the
    /// new check may only ever remove a kill that was already unsafe, never
    /// one that already worked.
    #[test]
    fn registry_pid_is_never_refused() {
        let v = verify_target_image(
            &PidSource::Registry,
            Some(Path::new(r"C:\Windows\System32\svchost.exe")),
            Path::new(r"D:\runner.exe"),
        );
        assert_eq!(v, TargetVerification::Proceed { note: None });
    }

    /// An unreadable image is almost always an already-exited process. It must
    /// NOT cost us the ability to stop a runner — but it must be said out loud.
    #[test]
    fn unreadable_image_proceeds_with_a_note() {
        let v = verify_target_image(&PidSource::ExeIdentity, None, Path::new(r"D:\runner.exe"));
        match v {
            TargetVerification::Proceed { note: Some(n) } => {
                assert!(n.contains("already"), "got: {n}");
            }
            other => panic!("expected Proceed with a note, got {other:?}"),
        }
    }

    #[test]
    fn empty_ledger_is_reported_as_such() {
        let ledger = StopLedger::new();
        assert_eq!(ledger.attempts_clause(), "no strategies were attempted");
        assert!(!ledger.any_kill_executed());
    }
}

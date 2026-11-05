//! The temp-runner sweep `shutdown_signal` runs at SIGNAL time, off Windows.
//!
//! On Windows every temp runner is assigned to the kill-on-close
//! [`crate::process::job::RunnerJob`], so the kernel reaps it on any
//! supervisor exit. Off Windows that job is a no-op stub
//! ([`crate::process::job::RunnerJob::REAPS_ON_SUPERVISOR_EXIT`] is `false`),
//! so a graceful shutdown has to stop its temp runners itself — and it has to
//! do it **at signal time, inside `shutdown_signal`**, not after the serve
//! future resolves: `shutdown_signal` arms a 10 s `std::process::exit(0)`, and
//! an in-flight long-poll (a synchronous `spawn-test`, `spawn-named
//! wait:true`) holds the serve future open, so the post-serve path is skipped
//! in exactly the case temp runners are being built (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1, resolved at vet).
//!
//! **Ownership authority is [`crate::process::job::should_assign_to_ephemeral_job`]
//! and nothing else** — an `is_temp()` allowlist, never `!is_primary()`, so a
//! new `RunnerKind` variant defaults to *not reaped*. The primary, named and
//! external runners are user-owned; served policy `runner-lifecycle` forbids
//! stopping them, and this sweep never selects them.
//!
//! The sweep is bounded ([`SHUTDOWN_SWEEP_BOUND`], 2 s, which fits inside the
//! post-serve `HARD_EXIT_DEADLINE_SECS = 3`); `unix_kill`'s SIGTERM grace is
//! 1.5 s, so a straggler is SIGKILLed inside the bound. It logs one line per
//! victim (id, pid, port), and one line for any victim the bound cut off —
//! which the supervisor's systemd unit (if any) or the next supervisor's
//! ledger-gated reconcile sweep then frees.

use std::time::Duration;

use qontinui_types::wire::runner_kind::RunnerKind;

/// The whole sweep — registry snapshot plus every kill — finishes inside this.
#[cfg_attr(windows, allow(dead_code))]
pub const SHUTDOWN_SWEEP_BOUND: Duration = Duration::from_secs(2);

/// One temp runner the sweep will stop.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepVictim {
    pub id: String,
    pub port: u16,
    pub pid: u32,
}

/// How long the sweep waits for a temp runner's lifecycle lock — a start or
/// stop of that runner in flight — before reading it anyway. A start that got
/// past the shutdown latch before it was set is still spawning under that lock;
/// waiting lets the pid it is about to record be seen. Every start that
/// arrives after the latch is refused (`SupervisorError::ShuttingDown`), so no
/// temp runner can appear after this wait.
#[cfg_attr(windows, allow(dead_code))]
pub const SWEEP_SETTLE_BOUND: Duration = Duration::from_millis(500);

/// One registry row as the sweep reads it: `(id, port, kind, pid, running)`.
#[cfg_attr(windows, allow(dead_code))]
pub type SweepRow = (String, u16, RunnerKind, Option<u32>, bool);

/// Pick the sweep's victims from a registry snapshot: every runner the
/// ownership predicate assigns to the ephemeral job, that is RUNNING and whose
/// pid is known. Pure.
///
/// `running` matters as much as the pid. The exit monitor holds a running
/// runner's un-reaped `Child`, so its pid cannot have been reused; once
/// `running` is false the registry pid may name a reaped process whose number
/// the OS has handed to something else. This is the same trust the stop path
/// gives a `Registry` pid (`stop_ledger::verify_target_image`).
#[cfg_attr(windows, allow(dead_code))]
pub fn temp_runners_to_sweep(runners: &[SweepRow]) -> Vec<SweepVictim> {
    runners
        .iter()
        .filter(|(_, _, kind, _, _)| crate::process::job::should_assign_to_ephemeral_job(kind))
        .filter(|(_, _, _, _, running)| *running)
        .filter_map(|(id, port, _, pid, _)| {
            pid.map(|pid| SweepVictim {
                id: id.clone(),
                port: *port,
                pid,
            })
        })
        .collect()
}

/// Stop every temp runner this supervisor tracks, concurrently, within
/// `bound` — ONCE per process. `shutdown_signal` calls it after
/// `state.signal_shutdown()` and before the drain-deadline exit is armed; the
/// self-restart endpoint calls it before its own hard exit. Whichever comes
/// second waits for the first sweep to finish rather than running another.
#[cfg(not(windows))]
pub async fn sweep_temp_runners_at_shutdown(state: &crate::state::SharedState, bound: Duration) {
    state
        .shutdown_sweep
        .get_or_init(|| sweep_once(state, bound))
        .await;
}

#[cfg(not(windows))]
async fn sweep_once(state: &crate::state::SharedState, bound: Duration) {
    use crate::log_capture::{LogLevel, LogSource};
    use tracing::{info, warn};

    let deadline = tokio::time::Instant::now() + bound;
    let settle_deadline = (tokio::time::Instant::now() + SWEEP_SETTLE_BOUND).min(deadline);

    async fn emit(state: &crate::state::SharedState, level: LogLevel, msg: String) {
        match level {
            LogLevel::Warn => warn!("{}", msg),
            _ => info!("{}", msg),
        }
        state.logs.emit(LogSource::Supervisor, level, msg).await;
    }

    let Ok(all) = tokio::time::timeout_at(deadline, state.get_all_runners()).await else {
        emit(
            state,
            LogLevel::Warn,
            format!(
                "Shutdown sweep: could not read the runner registry within {}s — NO temp runner \
                 was stopped; the supervisor's systemd unit (if any) or the next supervisor's \
                 ledger-gated reconcile sweep frees them",
                bound.as_secs_f64()
            ),
        )
        .await;
        return;
    };
    let temps: Vec<_> = all
        .into_iter()
        .filter(|m| crate::process::job::should_assign_to_ephemeral_job(&m.config.kind()))
        .collect();

    // Read each temp runner under its lifecycle lock (bounded), and latch a
    // stop request on every one about to be signalled, so its exit monitor
    // records an intended stop — not a crash to report or to restart.
    let reads = temps.iter().map(|managed| async move {
        let lifecycle = tokio::time::timeout_at(settle_deadline, managed.lifecycle.lock())
            .await
            .ok();
        let row = {
            let mut runner = managed.runner.write().await;
            if runner.running && runner.pid.is_some() {
                runner.latch_stop_request(chrono::Utc::now());
            }
            (
                managed.config.id.clone(),
                managed.config.port,
                managed.config.kind(),
                runner.pid,
                runner.running,
            )
        };
        (row, lifecycle.is_some())
    });
    let Ok(read) = tokio::time::timeout_at(deadline, futures::future::join_all(reads)).await else {
        emit(
            state,
            LogLevel::Warn,
            format!(
                "Shutdown sweep: could not read the temp runners within {}s — NO temp runner was \
                 stopped; the supervisor's systemd unit (if any) or the next supervisor's \
                 ledger-gated reconcile sweep frees them",
                bound.as_secs_f64()
            ),
        )
        .await;
        return;
    };
    for ((id, ..), settled) in &read {
        if !settled {
            emit(
                state,
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: a start or stop of temp runner '{id}' was still in progress \
                     after {}ms; read its PID without waiting for it",
                    SWEEP_SETTLE_BOUND.as_millis()
                ),
            )
            .await;
        }
    }
    let snapshot: Vec<SweepRow> = read.into_iter().map(|(row, _)| row).collect();

    let victims = temp_runners_to_sweep(&snapshot);
    if victims.is_empty() {
        emit(
            state,
            LogLevel::Info,
            "Shutdown sweep: no temp runner with a known PID to stop".to_string(),
        )
        .await;
        return;
    }

    let kills = victims.iter().map(|v| async move {
        let outcome =
            tokio::time::timeout_at(deadline, crate::process::proc_kill::kill_by_pid_tree(v.pid))
                .await;
        (v, outcome)
    });
    for (v, outcome) in futures::future::join_all(kills).await {
        let (level, msg) = match outcome {
            // `kill_by_pid_tree` answers `true` both when the group exited
            // inside the SIGTERM grace and when it escalated to SIGKILL, so
            // look again rather than claim an exit nobody observed.
            Ok(Ok(true)) if !crate::process::proc_kill::pid_alive(v.pid) => (
                LogLevel::Info,
                format!(
                    "Shutdown sweep: stopped temp runner '{}' (PID {}, port {})",
                    v.id, v.pid, v.port
                ),
            ),
            Ok(Ok(true)) => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: signalled temp runner '{}' (PID {}, port {}) — SIGKILL was \
                     sent after the SIGTERM grace and its exit was NOT yet observed",
                    v.id, v.pid, v.port
                ),
            ),
            Ok(Ok(false)) => (
                LogLevel::Info,
                format!(
                    "Shutdown sweep: temp runner '{}' (PID {}, port {}) was already gone",
                    v.id, v.pid, v.port
                ),
            ),
            Ok(Err(e)) => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: stopping temp runner '{}' (PID {}, port {}) failed: {e}",
                    v.id, v.pid, v.port
                ),
            ),
            Err(_) => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: temp runner '{}' (PID {}, port {}) was CUT OFF by the {}s \
                     bound and may still be running",
                    v.id,
                    v.pid,
                    v.port,
                    bound.as_secs_f64()
                ),
            ),
        };
        emit(state, level, msg).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RUNNING row; `stopped` below builds one whose runner has exited.
    fn row(id: &str, port: u16, kind: RunnerKind, pid: Option<u32>) -> SweepRow {
        (id.to_string(), port, kind, pid, true)
    }

    fn stopped(id: &str, port: u16, kind: RunnerKind, pid: Option<u32>) -> SweepRow {
        (id.to_string(), port, kind, pid, false)
    }

    fn pids(victims: &[SweepVictim]) -> Vec<u32> {
        victims.iter().map(|v| v.pid).collect()
    }

    /// Only RUNNING temp runners with a known pid are swept; every user-owned
    /// kind is spared whatever its pid.
    #[test]
    fn sweep_selects_temp_runners_with_a_pid_only() {
        let snapshot = vec![
            row(
                "test-a",
                9877,
                RunnerKind::Temp {
                    id: "test-a".into(),
                },
                Some(1001),
            ),
            row(
                "test-b",
                9878,
                RunnerKind::Temp {
                    id: "test-b".into(),
                },
                None,
            ),
            row("primary", 9876, RunnerKind::Primary, Some(1002)),
            row(
                "named-9880",
                9880,
                RunnerKind::Named {
                    name: "named-9880".into(),
                },
                Some(1003),
            ),
            row("ext", 9890, RunnerKind::External, Some(1004)),
            row(
                "test-c",
                9879,
                RunnerKind::Temp {
                    id: "test-c".into(),
                },
                Some(1005),
            ),
            // Exited: its registry pid may already name another process.
            stopped(
                "test-d",
                9881,
                RunnerKind::Temp {
                    id: "test-d".into(),
                },
                Some(1006),
            ),
        ];
        let victims = temp_runners_to_sweep(&snapshot);
        assert_eq!(pids(&victims), vec![1001, 1005]);
        assert_eq!(pids(&temp_runners_to_sweep(&[])), Vec::<u32>::new());
        assert_eq!(
            victims[0],
            SweepVictim {
                id: "test-a".into(),
                port: 9877,
                pid: 1001
            }
        );
    }

    #[test]
    fn sweep_never_selects_a_user_owned_runner() {
        let snapshot = vec![
            row("primary", 9876, RunnerKind::Primary, Some(1)),
            row("n", 9880, RunnerKind::Named { name: "n".into() }, Some(2)),
            row("e", 9890, RunnerKind::External, Some(3)),
        ];
        assert_eq!(pids(&temp_runners_to_sweep(&snapshot)), Vec::<u32>::new());
    }
}

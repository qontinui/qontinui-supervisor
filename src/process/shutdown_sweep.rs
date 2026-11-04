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

/// Pick the sweep's victims from a registry snapshot of
/// `(id, port, kind, pid)`: every runner the ownership predicate assigns to the
/// ephemeral job AND whose pid is known. Pure.
#[cfg_attr(windows, allow(dead_code))]
pub fn temp_runners_to_sweep(
    runners: &[(String, u16, RunnerKind, Option<u32>)],
) -> Vec<SweepVictim> {
    runners
        .iter()
        .filter(|(_, _, kind, _)| crate::process::job::should_assign_to_ephemeral_job(kind))
        .filter_map(|(id, port, _, pid)| {
            pid.map(|pid| SweepVictim {
                id: id.clone(),
                port: *port,
                pid,
            })
        })
        .collect()
}

/// Stop every temp runner this supervisor tracks, concurrently, within
/// `bound`. Called from `shutdown_signal` after `state.signal_shutdown()` and
/// before the drain-deadline exit is armed.
#[cfg(not(windows))]
pub async fn sweep_temp_runners_at_shutdown(state: &crate::state::SharedState, bound: Duration) {
    use crate::log_capture::{LogLevel, LogSource};
    use tracing::{info, warn};

    let deadline = tokio::time::Instant::now() + bound;

    let snapshot = tokio::time::timeout_at(deadline, async {
        let mut rows = Vec::new();
        for managed in state.get_all_runners().await {
            let pid = managed.runner.read().await.pid;
            rows.push((
                managed.config.id.clone(),
                managed.config.port,
                managed.config.kind(),
                pid,
            ));
        }
        rows
    })
    .await;
    let Ok(snapshot) = snapshot else {
        let msg = format!(
            "Shutdown sweep: could not read the runner registry within {}s — NO temp runner was \
             stopped; the supervisor's systemd unit (if any) or the next supervisor's ledger-gated \
             reconcile sweep frees them",
            bound.as_secs_f64()
        );
        warn!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, msg)
            .await;
        return;
    };

    let victims = temp_runners_to_sweep(&snapshot);
    if victims.is_empty() {
        let msg = "Shutdown sweep: no temp runner with a known PID to stop".to_string();
        info!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, msg)
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
            Ok(Ok(true)) => (
                LogLevel::Info,
                format!(
                    "Shutdown sweep: stopped temp runner '{}' (PID {}, port {})",
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
        match level {
            LogLevel::Warn => warn!("{}", msg),
            _ => info!("{}", msg),
        }
        state.logs.emit(LogSource::Supervisor, level, msg).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: &str,
        port: u16,
        kind: RunnerKind,
        pid: Option<u32>,
    ) -> (String, u16, RunnerKind, Option<u32>) {
        (id.to_string(), port, kind, pid)
    }

    fn pids(victims: &[SweepVictim]) -> Vec<u32> {
        victims.iter().map(|v| v.pid).collect()
    }

    /// Only temp runners with a known pid are swept; every user-owned kind is
    /// spared whatever its pid.
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

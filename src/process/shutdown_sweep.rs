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
//! The sweep is bounded in two phases: reading the temp runners (each waits
//! at most [`SWEEP_SETTLE_BOUND`], 500 ms, for a start or stop of that runner
//! in flight), then killing them, with a deadline of its own
//! ([`SHUTDOWN_SWEEP_BOUND`], 2 s) started only once the read is done, so a
//! slow read can never eat the kill's SIGTERM grace: `unix_kill`'s grace is
//! 1.5 s, so a straggler is SIGKILLed inside the kill bound. At most ~2.5 s in
//! all; it runs before `shutdown_signal` arms its 10 s drain deadline, and
//! before the self-restart endpoint arms its exit. It logs one line per victim
//! (id, pid, port), and one line for any victim the bound cut off — which the
//! supervisor's systemd unit (if any) or the next supervisor's ledger-gated
//! reconcile sweep then frees.

use std::time::Duration;

use qontinui_types::wire::runner_kind::RunnerKind;

/// The kill phase — every temp runner signalled concurrently — finishes inside
/// this, counted from the end of the read phase. It must exceed `unix_kill`'s
/// SIGTERM grace plus one poll, or the SIGKILL that follows the grace would be
/// cut off.
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

/// How long the read phase waits for a temp runner's lifecycle lock — a start
/// or stop of that runner in flight — before reading it anyway. A temp start
/// reads the shutdown latch twice under that lock: on entry, and again just
/// before its spawn. So a start the latch did not stop is at most a spawn
/// syscall and a registry write from recording its pid, and this wait sees it.
/// A start whose lock outlasts the wait is named in a WARN line.
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

/// Read one runner's sweep row and, when it is a running runner with a pid,
/// latch a stop request on it — so its exit monitor records an intended stop,
/// not a crash to report or to restart. Returns `(pid, running)`. Pure over the
/// state it is handed.
#[cfg_attr(windows, allow(dead_code))]
pub fn read_and_latch(
    runner: &mut crate::state::RunnerState,
    now: chrono::DateTime<chrono::Utc>,
) -> (Option<u32>, bool) {
    if runner.running && runner.pid.is_some() {
        runner.latch_stop_request(now);
    }
    (runner.pid, runner.running)
}

/// The rows the sweep goes on to select from: every runner that WAS read —
/// settled, or read without its lifecycle lock — and none that could not be.
/// Pure.
#[cfg_attr(windows, allow(dead_code))]
pub fn snapshot_of(reads: &[RowRead]) -> Vec<SweepRow> {
    reads
        .iter()
        .filter_map(|read| match read {
            RowRead::Settled(row) | RowRead::Unsettled(row) => Some(row.clone()),
            RowRead::Unreadable { .. } => None,
        })
        .collect()
}

/// How one runner's read went.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowRead {
    /// Read after its lifecycle lock was taken: no start or stop in flight.
    Settled(SweepRow),
    /// Read without the lifecycle lock, which a start or stop still held at
    /// the settle deadline.
    Unsettled(SweepRow),
    /// The runner state itself could not be locked by the deadline.
    Unreadable { id: String },
}

/// Read one temp runner for the sweep, waiting until `settle_deadline` for its
/// lifecycle lock and its state lock. Never waits past the deadline.
#[cfg_attr(windows, allow(dead_code))]
pub async fn read_row(
    managed: &crate::state::ManagedRunner,
    settle_deadline: tokio::time::Instant,
) -> RowRead {
    let lifecycle = tokio::time::timeout_at(settle_deadline, managed.lifecycle.lock())
        .await
        .ok();
    let Ok(mut runner) = tokio::time::timeout_at(settle_deadline, managed.runner.write()).await
    else {
        return RowRead::Unreadable {
            id: managed.config.id.clone(),
        };
    };
    let (pid, running) = read_and_latch(&mut runner, chrono::Utc::now());
    let row = (
        managed.config.id.clone(),
        managed.config.port,
        managed.config.kind(),
        pid,
        running,
    );
    drop(runner);
    match lifecycle {
        Some(_guard) => RowRead::Settled(row),
        None => RowRead::Unsettled(row),
    }
}

#[cfg(not(windows))]
async fn sweep_once(state: &crate::state::SharedState, bound: Duration) {
    use crate::log_capture::{LogLevel, LogSource};
    use tracing::{info, warn};

    async fn emit(state: &crate::state::SharedState, level: LogLevel, msg: String) {
        match level {
            LogLevel::Warn => warn!("{}", msg),
            _ => info!("{}", msg),
        }
        state.logs.emit(LogSource::Supervisor, level, msg).await;
    }

    // ---- read phase: bounded by SWEEP_SETTLE_BOUND -----------------------
    let settle_deadline = tokio::time::Instant::now() + SWEEP_SETTLE_BOUND;
    let Ok(all) = tokio::time::timeout_at(settle_deadline, state.get_all_runners()).await else {
        emit(
            state,
            LogLevel::Warn,
            format!(
                "Shutdown sweep: could not read the runner registry within {}ms — NO temp runner \
                 was stopped; the supervisor's systemd unit (if any) or the next supervisor's \
                 ledger-gated reconcile sweep frees them",
                SWEEP_SETTLE_BOUND.as_millis()
            ),
        )
        .await;
        return;
    };
    let temps: Vec<_> = all
        .into_iter()
        .filter(|m| crate::process::job::should_assign_to_ephemeral_job(&m.config.kind()))
        .collect();
    let reads = futures::future::join_all(temps.iter().map(|m| read_row(m, settle_deadline))).await;
    for read in &reads {
        match read {
            RowRead::Settled(_) => {}
            RowRead::Unsettled(row) => {
                // With no pid yet, a start may record one just after this read:
                // UNKNOWN, so say it is not stopped rather than let the summary
                // read as "nothing to stop".
                let msg = match row.3 {
                    Some(pid) => format!(
                        "Shutdown sweep: a start or stop of temp runner '{}' was still in \
                         progress after {}ms; read its PID ({pid}) without waiting for it",
                        row.0,
                        SWEEP_SETTLE_BOUND.as_millis()
                    ),
                    None => format!(
                        "Shutdown sweep: a start or stop of temp runner '{}' was still in \
                         progress after {}ms and it had no PID yet, so it was NOT stopped; the \
                         supervisor's systemd unit (if any) or the next supervisor's \
                         ledger-gated reconcile sweep frees it",
                        row.0,
                        SWEEP_SETTLE_BOUND.as_millis()
                    ),
                };
                emit(state, LogLevel::Warn, msg).await;
            }
            RowRead::Unreadable { id } => {
                emit(
                    state,
                    LogLevel::Warn,
                    format!(
                        "Shutdown sweep: temp runner '{id}' could not be read within {}ms and \
                         was NOT stopped; the supervisor's systemd unit (if any) or the next \
                         supervisor's ledger-gated reconcile sweep frees it",
                        SWEEP_SETTLE_BOUND.as_millis()
                    ),
                )
                .await;
            }
        }
    }

    let victims = temp_runners_to_sweep(&snapshot_of(&reads));
    if victims.is_empty() {
        emit(
            state,
            LogLevel::Info,
            "Shutdown sweep: no temp runner with a known PID to stop".to_string(),
        )
        .await;
        return;
    }

    // ---- kill phase: its own deadline, started only now ---------------------
    for (v, outcome) in kill_victims(&victims, bound).await {
        let (level, msg) = match outcome {
            KillOutcome::Stopped => (
                LogLevel::Info,
                format!(
                    "Shutdown sweep: stopped temp runner '{}' (PID {}, port {})",
                    v.id, v.pid, v.port
                ),
            ),
            KillOutcome::SignalledNotObserved => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: signalled temp runner '{}' (PID {}, port {}) — SIGKILL was \
                     sent after the SIGTERM grace and its exit was NOT yet observed",
                    v.id, v.pid, v.port
                ),
            ),
            KillOutcome::AlreadyGone => (
                LogLevel::Info,
                format!(
                    "Shutdown sweep: temp runner '{}' (PID {}, port {}) had already exited",
                    v.id, v.pid, v.port
                ),
            ),
            KillOutcome::Failed(e) => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: stopping temp runner '{}' (PID {}, port {}) failed: {e}",
                    v.id, v.pid, v.port
                ),
            ),
            KillOutcome::CutOff => (
                LogLevel::Warn,
                format!(
                    "Shutdown sweep: temp runner '{}' (PID {}, port {}) was CUT OFF by the {}s \
                     kill bound and may still be running",
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

/// How one victim's kill ended.
#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// Signalled, and observed gone afterwards.
    Stopped,
    /// SIGKILL was sent after the SIGTERM grace, and the exit was not yet
    /// observed when the kill returned.
    SignalledNotObserved,
    /// Gone before the sweep signalled it.
    AlreadyGone,
    /// The kill itself errored.
    Failed(String),
    /// The kill bound ran out first.
    CutOff,
}

/// Kill every victim's process tree concurrently: SIGTERM to its group, then
/// SIGKILL after `unix_kill`'s grace. The deadline is `bound` from NOW — this
/// function starts its own clock, so time spent before the call (the read
/// phase) can never eat the grace that precedes the SIGKILL.
#[cfg(not(windows))]
pub async fn kill_victims(
    victims: &[SweepVictim],
    bound: Duration,
) -> Vec<(SweepVictim, KillOutcome)> {
    let deadline = tokio::time::Instant::now() + bound;
    let kills = victims.iter().map(|v| async move {
        // Gone only when the leader AND its group are: a runner's children can
        // outlive it in its group, and the group signal below still reaches them.
        if !crate::process::proc_kill::pid_alive(v.pid)
            && !crate::process::unix_kill::group_alive(v.pid)
        {
            return (v.clone(), KillOutcome::AlreadyGone);
        }
        let outcome =
            tokio::time::timeout_at(deadline, crate::process::proc_kill::kill_by_pid_tree(v.pid))
                .await;
        let outcome = match outcome {
            // `kill_by_pid_tree` answers `true` both when the group exited
            // inside the SIGTERM grace and when it escalated to SIGKILL, so
            // look again rather than claim an exit nobody observed.
            Ok(Ok(_)) if !crate::process::proc_kill::pid_alive(v.pid) => KillOutcome::Stopped,
            // Still a live pid after the kill: SIGKILL went out after the grace
            // and the exit has not been observed (or the number now names
            // another process) — never "stopped".
            Ok(Ok(_)) => KillOutcome::SignalledNotObserved,
            Ok(Err(e)) => KillOutcome::Failed(e.to_string()),
            Err(_) => KillOutcome::CutOff,
        };
        (v.clone(), outcome)
    });
    futures::future::join_all(kills).await
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

    /// The kill phase must outlast the SIGTERM grace plus one poll, or the
    /// SIGKILL that follows the grace is cut off by the bound.
    #[cfg(not(windows))]
    #[test]
    fn the_kill_bound_outlasts_the_sigterm_grace() {
        let grace = Duration::from_millis(
            crate::process::unix_kill::TERM_GRACE_MS + crate::process::unix_kill::TERM_POLL_MS,
        );
        assert!(
            grace < SHUTDOWN_SWEEP_BOUND,
            "grace {grace:?} must be inside the kill bound {SHUTDOWN_SWEEP_BOUND:?}"
        );
    }

    /// A running runner with a pid gets a stop request latched, so its exit
    /// monitor records an intended stop; anything else is left untouched.
    #[test]
    fn the_sweep_latches_a_stop_request_only_on_a_running_runner_with_a_pid() {
        let now = chrono::Utc::now();
        let state_with = |running: bool, pid: Option<u32>| crate::state::RunnerState {
            running,
            pid,
            ..Default::default()
        };
        let mut running = state_with(true, Some(4242));
        assert_eq!(read_and_latch(&mut running, now), (Some(4242), true));
        assert!(running.stop_requested);
        assert_eq!(running.stop_requested_at, Some(now));

        for (pid, is_running) in [(Some(4242), false), (None, true), (None, false)] {
            let mut other = state_with(is_running, pid);
            assert_eq!(read_and_latch(&mut other, now), (pid, is_running));
            assert!(
                !other.stop_requested,
                "pid {pid:?} running {is_running}: nothing to stop, so no stop is latched"
            );
        }
    }

    fn temp_runner(id: &str) -> crate::state::ManagedRunner {
        let config = crate::config::RunnerConfig {
            id: id.to_string(),
            name: format!("Runner {id}"),
            port: 9899,
            kind: RunnerKind::Temp { id: id.to_string() },
            protected: false,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
            fresh_device_identity: false,
        };
        crate::state::ManagedRunner::new(config, false)
    }

    /// A start still holding the lifecycle lock when the sweep reads is waited
    /// for, so the pid it records is seen; a lock held past the settle
    /// deadline is read without it and reported Unsettled.
    #[tokio::test]
    async fn the_read_waits_for_a_start_in_flight_then_gives_up_at_the_deadline() {
        let managed = std::sync::Arc::new(temp_runner("test-in-flight"));
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let starter = {
            let managed = managed.clone();
            tokio::spawn(async move {
                let _lifecycle = managed.lifecycle.lock().await;
                let _ = held_tx.send(());
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut runner = managed.runner.write().await;
                runner.running = true;
                runner.pid = Some(4242);
            })
        };
        held_rx.await.expect("the start holds the lifecycle lock");
        let read = read_row(
            &managed,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
        starter.await.expect("the start finishes");
        match read {
            RowRead::Settled((_, _, _, pid, running)) => {
                assert_eq!(pid, Some(4242), "the pid the start recorded must be seen");
                assert!(running);
            }
            other => panic!("the read must wait for the start in flight: {other:?}"),
        }
        assert!(
            managed.runner.read().await.stop_requested,
            "the swept runner carries a stop request"
        );

        let stuck = temp_runner("test-stuck");
        let _held = stuck.lifecycle.lock().await;
        let read = read_row(
            &stuck,
            tokio::time::Instant::now() + Duration::from_millis(50),
        )
        .await;
        assert!(matches!(read, RowRead::Unsettled(_)), "{read:?}");
    }

    /// A runner whose STATE lock is held past the deadline is reported
    /// Unreadable (and named) rather than stalling the whole read phase.
    #[tokio::test]
    async fn a_state_lock_held_past_the_deadline_is_unreadable_not_a_stall() {
        let busy = temp_runner("test-state-busy");
        let _state = busy.runner.write().await;
        let started = std::time::Instant::now();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            read_row(
                &busy,
                tokio::time::Instant::now() + Duration::from_millis(50),
            ),
        )
        .await
        .expect("the read must give up at its own deadline, not wait for the lock");
        assert_eq!(
            read,
            RowRead::Unreadable {
                id: "test-state-busy".to_string()
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the read must give up at its deadline, not wait for the lock"
        );
    }

    /// Both callers — `shutdown_signal` and the self-restart endpoint — share
    /// ONE sweep through the state's once-cell.
    #[test]
    fn the_sweep_runs_once_per_process() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("process")
                .join("shutdown_sweep.rs"),
        )
        .expect("read shutdown_sweep.rs")
        .replace("\r\n", "\n");
        let body =
            crate::source_scan::fn_body(&src, "pub async fn sweep_temp_runners_at_shutdown(")
                .expect("sweep_temp_runners_at_shutdown exists");
        assert!(
            body.contains(".shutdown_sweep") && body.contains(".get_or_init("),
            "the sweep must run through the state's once-cell, or `shutdown_signal` and \
             `supervisor_restart` signal every temp runner twice, concurrently: {body}"
        );
    }

    /// A runner read without its lifecycle lock is still swept; only one that
    /// could not be read at all is left out.
    #[test]
    fn the_snapshot_keeps_settled_and_unsettled_rows_and_drops_unreadable_ones() {
        let row = |id: &str, pid: u32| -> SweepRow {
            (
                id.to_string(),
                9877,
                RunnerKind::Temp { id: id.to_string() },
                Some(pid),
                true,
            )
        };
        let reads = [
            RowRead::Settled(row("test-a", 1)),
            RowRead::Unsettled(row("test-b", 2)),
            RowRead::Unreadable {
                id: "test-c".to_string(),
            },
        ];
        assert_eq!(
            snapshot_of(&reads),
            vec![row("test-a", 1), row("test-b", 2)]
        );
    }

    /// `sweep_once` selects from every runner it read and kills them through
    /// `kill_victims` with the bound it was handed — the helpers are tested
    /// directly, this pins that the sweep uses them.
    #[test]
    fn the_sweep_selects_from_the_snapshot_and_kills_with_its_bound() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("process")
                .join("shutdown_sweep.rs"),
        )
        .expect("read shutdown_sweep.rs")
        .replace("\r\n", "\n");
        let body =
            crate::source_scan::fn_body(&src, "async fn sweep_once(").expect("sweep_once exists");
        for needle in [
            "temp_runners_to_sweep(&snapshot_of(&reads))",
            "kill_victims(&victims, bound)",
        ] {
            assert!(body.contains(needle), "sweep_once lacks `{needle}`");
        }
    }
}

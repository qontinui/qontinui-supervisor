//! Post-start health probe for runners.
//!
//! Used by the legacy `POST /runner/restart` and the per-runner
//! `POST /runners/{id}/start` endpoints to wait until a freshly-(re)started
//! runner is actually serving HTTP `/health` before returning a 200 to the
//! caller. Without this wait, those endpoints would return successfully the
//! moment the OS process spawned, even when the runner immediately hung
//! without binding its API port — leaving callers unable to distinguish
//! "started successfully" from "started but hung" without a separate probe.
//!
//! The probe encapsulates a simple 500ms poll loop against
//! [`crate::process::port::is_runner_responding`]. On timeout it captures
//! the last few log lines from the runner's per-runner log buffer using the
//! same mechanism `routes/runners.rs::recent_runner_log_lines` uses for the
//! spawn-test failure response, so the route handler can build a cleanly
//! diagnosable 503.
//!
//! Spawn-test has its own, richer post-spawn probe in `routes/runners.rs`
//! (`probe_runner_health`) that also captures `recent_panic` and child-alive
//! state. That probe is intentionally untouched here — restart/start callers
//! get the simpler "did the port come up?" semantics, which is what the
//! original failure mode warranted.
//!
//! A start nobody waits on takes its dev-action verdict from
//! [`wait_for_action_boot_healthy`] instead, which asks a narrower question: did
//! THIS action's boot come up?

use std::time::Duration;

use tokio::time::Instant;
use uuid::Uuid;

use crate::log_capture::LogLevel;
use crate::process::port::is_runner_responding;
use crate::state::SharedState;

/// How often to poll the runner's `/health` endpoint while waiting.
///
/// Matches the cadence the spawn-test wait-loop uses for its `wait` parameter
/// (the `wait_timeout_secs` flow polls every 500ms-ish via a 2s coarse loop;
/// here we go finer-grained because the typical observed bind time on a
/// healthy runner is well under one poll interval).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Default total wait budget, for a caller holding an HTTP request open. Shorter
/// than the first-healthy budget: a runner that answers after this is slow, not
/// failed.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest [`wait_for_action_boot_healthy`] runs, whatever the first-healthy
/// budget's env override says. A `restart` window has to outlast it: on its backstop
/// it closes with no result. A restart queued behind two stops under the lifecycle
/// lock spends about 270 s before this wait starts, and the assertion holds the sum.
const ACTION_BOOT_WAIT_CEILING: Duration = Duration::from_secs(300);
const _: () = assert!(
    ACTION_BOOT_WAIT_CEILING.as_secs() + 270
        < crate::dev_action::attribution::RESTART_BACKSTOP_WINDOW.as_secs()
);

/// How many recent log lines to include in the failure body. Matches
/// `routes/runners.rs::probe_runner_health`'s 20-line snapshot.
const RECENT_LOG_LIMIT: usize = 20;

/// Captured failure context for a runner that didn't come back up within
/// the wait budget. Carries enough information for the route handler to
/// build a 503 body with `elapsed_ms` and `recent_logs` without re-querying.
#[derive(Debug)]
pub struct HealthProbeFailure {
    /// How long we waited before giving up, in milliseconds. On a real
    /// timeout this is >= the configured budget; on an early-out (e.g.
    /// runner removed from registry mid-probe) it can be smaller.
    pub elapsed_ms: u64,
    /// Last [`RECENT_LOG_LIMIT`] log lines from the runner's per-runner log
    /// buffer, formatted as `"<level> <message>"` to keep the JSON payload
    /// compact. Empty if the runner was removed from the registry between
    /// the probe starting and timing out.
    pub recent_logs: Vec<String>,
}

/// Poll `runner_id`'s `/health` endpoint at [`POLL_INTERVAL`] cadence until
/// it succeeds or `timeout` elapses.
///
/// Returns:
/// - `Ok(())` on the first successful health response.
/// - `Err(HealthProbeFailure { elapsed_ms, recent_logs })` if the runner
///   never responded within the budget.
///
/// **Resolution semantics.** The runner's port is read from the live
/// `state.runners` registry on every poll. If the runner is removed from the
/// registry mid-probe (e.g. another caller stopped it), we abort with a
/// failure carrying whatever logs we can still snapshot. We deliberately do
/// not consult `state.stopped_runners` here — a runner missing from the live
/// registry means the start failed in a way the probe can't usefully wait
/// out.
pub async fn wait_for_runner_healthy(
    state: &SharedState,
    runner_id: &str,
    timeout: Duration,
) -> Result<(), HealthProbeFailure> {
    poll_until_healthy(state, runner_id, timeout, None).await
}

/// Default-timeout convenience wrapper. Use [`DEFAULT_TIMEOUT`] (30 s),
/// matching the contract documented on the `/runner/restart` and
/// `/runners/{id}/start` route handlers.
pub async fn wait_for_runner_healthy_default(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), HealthProbeFailure> {
    wait_for_runner_healthy(state, runner_id, DEFAULT_TIMEOUT).await
}

/// Poll until the boot `action_id` started answers, for a dev-action verdict
/// nobody is waiting on.
///
/// The budget is the first-healthy watchdog's, since no caller is held open and a
/// boot answering inside it is not a failure. It is a bound, not proof: the watchdog
/// stands down once the health cache lowers a booting primary's `running`, so a
/// primary answering after it reads as failed while alive.
///
/// That budget overlaps a crash-watchdog respawn, whose answer on the same port must
/// not read as this boot's, so the wait is held to the action's own spawn: once that
/// spawn has exited or a later one has replaced it, the boot never came up, whoever
/// answers now. With no spawn naming the action, the port decides, as
/// [`wait_for_runner_healthy`] does.
pub async fn wait_for_action_boot_healthy(
    state: &SharedState,
    runner_id: &str,
    action_id: Uuid,
) -> Result<(), HealthProbeFailure> {
    let budget = action_boot_wait(crate::process::manager::first_healthy_timeout_secs());
    poll_until_healthy(state, runner_id, budget, Some(action_id)).await
}

/// The budget of [`wait_for_action_boot_healthy`], from the first-healthy budget.
fn action_boot_wait(first_healthy_secs: u64) -> Duration {
    Duration::from_secs(first_healthy_secs).min(ACTION_BOOT_WAIT_CEILING)
}

/// The poll behind both waits. `boot_of` holds it to that action's own spawn
/// ([`crate::state::SpawnRing::action_spawn_ended`]).
async fn poll_until_healthy(
    state: &SharedState,
    runner_id: &str,
    timeout: Duration,
    boot_of: Option<Uuid>,
) -> Result<(), HealthProbeFailure> {
    let start = Instant::now();

    loop {
        // Re-read the runner each iteration so a config change (rare) is
        // honored, and so a registry removal cleanly aborts the loop.
        let Some(managed) = state.get_runner(runner_id).await else {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            return Err(HealthProbeFailure {
                elapsed_ms,
                recent_logs: Vec::new(),
            });
        };

        let responding = is_runner_responding(managed.config.port).await;
        // Read AFTER the answer: a later spawn is recorded moments after its
        // process starts, long before a runner can boot far enough to answer, so
        // an answer from one finds this boot ended here. Read before the probe,
        // a spawn recorded while the probe was out would pass as this boot.
        if let Some(action_id) = boot_of {
            if managed
                .recent_spawns
                .read()
                .await
                .action_spawn_ended(action_id)
            {
                break;
            }
        }
        if responding {
            return Ok(());
        }

        if start.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let recent_logs = recent_runner_log_lines(state, runner_id, RECENT_LOG_LIMIT).await;
    Err(HealthProbeFailure {
        elapsed_ms,
        recent_logs,
    })
}

/// Snapshot the last `limit` log lines from a runner's per-runner log buffer.
///
/// Mirrors `routes/runners.rs::recent_runner_log_lines` (the spawn-test
/// failure-body helper) — we duplicate the small formatting routine here
/// rather than make that helper public, because the two callers live in
/// different modules and the function body is six lines. If a third caller
/// appears, fold this into a shared helper.
async fn recent_runner_log_lines(
    state: &SharedState,
    runner_id: &str,
    limit: usize,
) -> Vec<String> {
    let Some(managed) = state.get_runner(runner_id).await else {
        return Vec::new();
    };
    let entries = managed.logs.history().await;
    let total = entries.len();
    let start = total.saturating_sub(limit);
    entries[start..]
        .iter()
        .map(|e| {
            let level = match e.level {
                LogLevel::Info => "info",
                LogLevel::Warn => "warn",
                LogLevel::Error => "error",
                LogLevel::Debug => "debug",
            };
            format!("{} {}", level, e.message)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{routing::get, Router};

    use super::*;
    use crate::state::{ManagedRunner, SpawnMarker, SupervisorState};

    /// Far past anything these waits should take: a wait that returns well inside
    /// this ended on the spawn, not on the clock.
    const GENEROUS: Duration = Duration::from_secs(60);

    /// A port that answers `/health`, as whatever process holds it does.
    async fn answering_port() -> u16 {
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    /// A port nothing listens on.
    fn silent_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }

    async fn runner_on(port: u16) -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
        let (config, root) = crate::config::config_over_an_empty_runner_tree();
        let state: SharedState = Arc::new(SupervisorState::new(config));
        let mut runner = crate::config::RunnerConfig::default_primary();
        runner.port = port;
        let managed = Arc::new(ManagedRunner::new(runner, false));
        state
            .runners
            .write()
            .await
            .insert(managed.config.id.clone(), managed.clone());
        (state, managed, root)
    }

    fn spawn_by(action_id: Uuid, at: chrono::DateTime<chrono::Utc>) -> SpawnMarker {
        SpawnMarker::new(at, Some(1), None, Some(action_id))
    }

    #[tokio::test]
    async fn an_answer_while_the_actions_boot_is_up_is_its_own() {
        let action = Uuid::new_v4();
        let (state, managed, _root) = runner_on(answering_port().await).await;
        managed
            .recent_spawns
            .write()
            .await
            .push(spawn_by(action, chrono::Utc::now()));

        poll_until_healthy(&state, &managed.config.id, GENEROUS, Some(action))
            .await
            .expect("its own boot answered");
    }

    /// The regression: the boot crashed, the crash watchdog respawned the runner,
    /// and the respawn answers the same port. That is not this action's boot.
    #[tokio::test]
    async fn a_respawns_answer_is_not_the_boot_that_exited() {
        let action = Uuid::new_v4();
        let (state, managed, _root) = runner_on(answering_port().await).await;
        let spawned = chrono::Utc::now();
        {
            let mut ring = managed.recent_spawns.write().await;
            ring.push(spawn_by(action, spawned));
            assert!(ring.stamp_exit(spawned, spawned, None));
        }

        let failure = poll_until_healthy(&state, &managed.config.id, GENEROUS, Some(action))
            .await
            .expect_err("the boot exited, whoever answers now");
        assert!(failure.elapsed_ms < GENEROUS.as_millis() as u64 / 2);
    }

    /// The monitor stamps an exit outside the lifecycle lock, so a later spawn can
    /// be recorded, and answer, before the earlier one's exit is stamped.
    #[tokio::test]
    async fn a_later_spawns_answer_is_not_the_boot_it_replaced() {
        let (action, later) = (Uuid::new_v4(), Uuid::new_v4());
        let (state, managed, _root) = runner_on(answering_port().await).await;
        let spawned = chrono::Utc::now();
        {
            let mut ring = managed.recent_spawns.write().await;
            ring.push(spawn_by(action, spawned));
            ring.push(spawn_by(later, spawned + chrono::Duration::seconds(1)));
        }

        let failure = poll_until_healthy(&state, &managed.config.id, GENEROUS, Some(action))
            .await
            .expect_err("a later spawn replaced it");
        assert!(failure.elapsed_ms < GENEROUS.as_millis() as u64 / 2);
    }

    /// No spawn naming the action says nothing about it, so the port decides, and
    /// a wait with no action never reads the ring at all.
    #[tokio::test]
    async fn with_no_spawn_naming_the_action_the_port_decides() {
        let (state, managed, _root) = runner_on(answering_port().await).await;
        poll_until_healthy(&state, &managed.config.id, GENEROUS, Some(Uuid::new_v4()))
            .await
            .expect("the port answered");

        let exited = chrono::Utc::now();
        {
            let mut ring = managed.recent_spawns.write().await;
            ring.push(spawn_by(Uuid::new_v4(), exited));
            assert!(ring.stamp_exit(exited, exited, None));
        }
        wait_for_runner_healthy(&state, &managed.config.id, GENEROUS)
            .await
            .expect("the port answered, and this wait holds no action");
    }

    /// The ring is read AFTER the answer, not before the probe. This `/health`
    /// records a later spawn and then answers, so a wait that read the ring first
    /// would see its boot up and take the answer as its own.
    #[tokio::test]
    async fn a_spawn_recorded_while_the_probe_was_out_is_not_the_boot_it_replaced() {
        let (action, later) = (Uuid::new_v4(), Uuid::new_v4());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, managed, _root) = runner_on(listener.local_addr().unwrap().port()).await;
        let spawned = chrono::Utc::now();
        managed
            .recent_spawns
            .write()
            .await
            .push(spawn_by(action, spawned));

        let replaced = managed.clone();
        let app = Router::new().route(
            "/health",
            get(move || {
                let replaced = replaced.clone();
                async move {
                    replaced
                        .recent_spawns
                        .write()
                        .await
                        .push(spawn_by(later, spawned + chrono::Duration::seconds(1)));
                    "ok"
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        poll_until_healthy(&state, &managed.config.id, GENEROUS, Some(action))
            .await
            .expect_err("a later spawn answered");
    }

    /// The wiring: a deferred verdict waits on the boot of the action it records
    /// on. Handed any other id, no spawn would name it, the port would decide, and
    /// this crashed boot would read `Ran`.
    #[tokio::test]
    async fn a_deferred_verdict_reads_the_boot_of_its_own_action() {
        use crate::dev_action::run_result::UNHEALTHY_AFTER_START;
        use crate::dev_action::{
            record_start_verdict, ActionKind, ActionRecord, ActionRunResult, RunResultGuard,
            StartVerdict,
        };

        let (state, managed, _root) = runner_on(answering_port().await).await;
        let record = Arc::new(ActionRecord::new(
            ActionKind::Spawn,
            None,
            "test".to_string(),
            &[],
        ));
        let spawned = chrono::Utc::now();
        {
            let mut ring = managed.recent_spawns.write().await;
            ring.push(spawn_by(record.action_id, spawned));
            assert!(ring.stamp_exit(spawned, spawned, None));
        }

        record_start_verdict(
            &state,
            &managed.config.id,
            StartVerdict::AfterHealthWait,
            RunResultGuard::arm(record.clone(), "the deferred verdict test"),
        );

        let deadline = Instant::now() + GENEROUS;
        let result = loop {
            if let Some(result) = record.run_result() {
                break result;
            }
            assert!(Instant::now() < deadline, "no verdict was recorded");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        match result {
            ActionRunResult::Errored { reason } => {
                assert!(reason.starts_with(UNHEALTHY_AFTER_START), "{reason}")
            }
            other => panic!("a crashed boot must not read as up: {other:?}"),
        }
    }

    /// The first-healthy budget, up to the ceiling that keeps it inside a `restart`
    /// window's backstop whatever the env override says.
    #[test]
    fn the_wait_takes_the_first_healthy_budget_up_to_its_ceiling() {
        assert_eq!(action_boot_wait(90), Duration::from_secs(90));
        assert_eq!(action_boot_wait(86_400), ACTION_BOOT_WAIT_CEILING);
    }

    /// It lasts its whole budget and not much more. On the paused clock, so this
    /// does not take a minute and a half.
    #[tokio::test(start_paused = true)]
    async fn the_wait_nobody_holds_open_lasts_the_first_healthy_budget() {
        let action = Uuid::new_v4();
        let (state, managed, _root) = runner_on(silent_port()).await;
        managed
            .recent_spawns
            .write()
            .await
            .push(spawn_by(action, chrono::Utc::now()));
        let budget = action_boot_wait(crate::process::manager::first_healthy_timeout_secs());

        let started = Instant::now();
        wait_for_action_boot_healthy(&state, &managed.config.id, action)
            .await
            .expect_err("nothing answers");
        let elapsed = started.elapsed();
        assert!(elapsed >= budget, "{elapsed:?}");
        // One more probe's 3 s timeout and a poll interval at most.
        assert!(elapsed <= budget + Duration::from_secs(5), "{elapsed:?}");
    }
}

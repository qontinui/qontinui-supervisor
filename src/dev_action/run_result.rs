//! Mapping an executing path's exit to its action's terminal
//! [`ActionRunResult`], and recording it.
//!
//! The attribution watcher holds every window open until the action records its
//! own terminal result ([`crate::dev_action::attribution`]), so every path that
//! mints an action must record one on every exit. This module holds the maps
//! those paths share:
//!
//! - [`run_result_for`]: an error that ended an action;
//! - [`spawn_run_result`]: the result of a spawn route's inner future, the one
//!   chokepoint spawn-test and spawn-named each have;
//! - [`unhealthy_after_start_reason`]: the reason a failed health wait records;
//! - [`start_verdict`] and [`record_start_verdict`]: what a start records from
//!   the health it observed before answering, and, when it observed none, the
//!   detached health wait that decides instead (plan
//!   `2026-10-29-supervisor-spawn-attribution-window-and-hermetic-dev-action-coord`
//!   D9).

use axum::http::StatusCode;
use futures::future::BoxFuture;

use crate::dev_action::record::{ActionRunResult, RunResultGuard};
use crate::error::SupervisorError;
use crate::process::health_probe::{
    wait_for_action_boot_healthy, HealthProbeFailure, HealthWaitEnd,
};
use crate::state::SharedState;

/// The health wait [`record_start_verdict_with`] runs for a deferred verdict, on
/// the runner's id and the action's: `Ok` once that action's boot answered, else
/// the failure. Production passes [`default_health_wait`]
/// ([`wait_for_action_boot_healthy`]); a test passes a wait it controls, so the
/// detached half runs without a runner.
pub type HealthWait = for<'a> fn(
    &'a SharedState,
    &'a str,
    uuid::Uuid,
) -> BoxFuture<'a, Result<(), HealthProbeFailure>>;

/// The production [`HealthWait`]: [`wait_for_action_boot_healthy`].
pub fn default_health_wait<'a>(
    state: &'a SharedState,
    runner_id: &'a str,
    action_id: uuid::Uuid,
) -> BoxFuture<'a, Result<(), HealthProbeFailure>> {
    Box::pin(wait_for_action_boot_healthy(state, runner_id, action_id))
}

/// The reason token of a start whose runner never answered its health check.
pub const UNHEALTHY_AFTER_START: &str = "runner_unhealthy_after_start";

/// The reason a start records when its health wait gave up on `runner_id`,
/// `after` naming what the wait followed. It says how the wait ended, since this
/// is what `GET /actions/{id}/outcome` serves and, unless a `DEV-*` signature's excerpt
/// wins, what coord gets as `evidence_ref`: a boot that exited after 2 s must not read
/// as a runner slow for 2 s, and a runner out of budget may be alive.
pub fn unhealthy_after_start_reason(
    runner_id: &str,
    failure: &HealthProbeFailure,
    after: &str,
) -> String {
    let elapsed_ms = failure.elapsed_ms;
    match failure.ended {
        HealthWaitEnd::BudgetSpent => format!(
            "{UNHEALTHY_AFTER_START}: runner '{runner_id}' did not become healthy within \
             {elapsed_ms}ms of {after}"
        ),
        HealthWaitEnd::BootEnded => format!(
            "{UNHEALTHY_AFTER_START}: the boot of runner '{runner_id}' was found ended \
             {elapsed_ms}ms into the health wait after {after}, before it answered: its spawn \
             exited or a later one replaced it"
        ),
        HealthWaitEnd::RunnerRemoved => format!(
            "{UNHEALTHY_AFTER_START}: runner '{runner_id}' left the registry {elapsed_ms}ms \
             into the health wait after {after}"
        ),
    }
}

/// Map an error that ended an action to the action's own terminal result.
///
/// [`SupervisorError::RestartUnsafe`] is the readiness gate REFUSING — nothing
/// ran, so no effect surface was touched and a clean attribution window carries
/// no information about it. Everything else ran and failed. Both classify to
/// `D3Category::Failure` (plan §5 rejects a sixth category); the distinction is
/// carried on the record and surfaces in the outcome's `evidence_ref`.
///
/// The refusal's `message` already begins with the gate's own code —
/// `restart_refused_unsafe: …` when the runner reported live work,
/// `restart_refused_unknown: …` when no verdict could be obtained — which is
/// what an operator (and `GET /build/{id}/status`) greps for, so it is carried
/// verbatim rather than re-worded here.
///
/// The other precondition refusals map to `Refused` too, carried as their own
/// message: [`SupervisorError::RunnerAlreadyRunning`] (a start found the
/// runner up), [`SupervisorError::Validation`] (a request or configuration the
/// step will not act on, e.g. `manager::automated_restart_blocked`),
/// [`SupervisorError::RunnerLifecycleBusy`] (a start or stop holds the runner),
/// [`SupervisorError::StartSuperseded`] (a crash-watchdog start that found
/// the runner started, stopped or removed since the exit it answers),
/// [`SupervisorError::UnverifiedExe`] (the exe a start resolved has no identity),
/// [`SupervisorError::NoDisplay`] (a Linux start has no display),
/// [`SupervisorError::PortHeldByLiveRunner`] (a live runner holds the port),
/// [`SupervisorError::BuildPoolFull`] (the pool was full under `no-wait`) and
/// [`SupervisorError::InsufficientDisk`] (the disk guard refused the build).
/// Each refuses the step it guards before that step has started a process or a
/// build, which is what `Refused` asserts: nothing ran. That holds for a start
/// with no build in front of it and for a restart refused before it acted. A
/// spawn route that built first and then found its port held reads `Refused` too:
/// it started nothing, and a window reads only what its action started. It does
/// NOT hold for a restart's start
/// refused AFTER its stop or build ran (a start slipped in between them, or the port
/// `apply_non_primary_instance_env_with` refuses): the stop is an effect, and a
/// `Refused` would close the attribution window on it unobserved. The body
/// that acted marks such an exit [`SupervisorError::AfterAct`], which this map
/// reads as `Errored` whatever it wraps; the door's own record takes it, so no
/// store lookup (which the store's eviction cap could miss) is involved.
pub fn run_result_for(err: &SupervisorError) -> ActionRunResult {
    match err {
        SupervisorError::AfterAct(inner) => ActionRunResult::Errored {
            reason: inner.to_string(),
        },
        SupervisorError::RestartUnsafe(detail) => ActionRunResult::Refused {
            reason: detail.message.clone(),
        },
        e @ (SupervisorError::RunnerAlreadyRunning
        | SupervisorError::Validation(_)
        | SupervisorError::RunnerLifecycleBusy(_)
        | SupervisorError::StartSuperseded(_)
        | SupervisorError::UnverifiedExe(_)
        | SupervisorError::NoDisplay(_)
        | SupervisorError::PortHeldByLiveRunner { .. }
        | SupervisorError::BuildPoolFull { .. }
        | SupervisorError::InsufficientDisk { .. }) => ActionRunResult::Refused {
            reason: e.to_string(),
        },
        other => ActionRunResult::Errored {
            reason: other.to_string(),
        },
    }
}

/// What a start knew about its runner's health when it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncProbe {
    /// No synchronous health check ran: `?wait=false` on a restart,
    /// `wait: false` on spawn-named, `health_probe_timeout_ms: 0` and no wait
    /// on spawn-test.
    NotProbed,
    /// The runner answered its health check.
    Healthy,
    /// The runner never answered; `reason` says how the wait ended.
    Unhealthy { reason: String },
}

/// What a path records now, or that the detached health wait records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartVerdict {
    /// Record this result now.
    Now(ActionRunResult),
    /// The start answered before anything checked the runner's health, so
    /// recording `Ran` now would call a boot that never binds its API a success.
    /// [`record_start_verdict`] runs the health wait detached and records its
    /// verdict.
    AfterHealthWait,
}

/// The one rule every start follows: what it observed decides, and nothing
/// observed defers to the health wait, never to `Ran` at the spawn. Recording
/// nothing instead would read the spawn as a boot that did not come up, so a
/// caught panic in a runner that came up would count: the opposite error.
pub fn start_verdict(probe: SyncProbe) -> StartVerdict {
    match probe {
        SyncProbe::Healthy => StartVerdict::Now(ActionRunResult::Ran),
        SyncProbe::Unhealthy { reason } => StartVerdict::Now(ActionRunResult::Errored { reason }),
        SyncProbe::NotProbed => StartVerdict::AfterHealthWait,
    }
}

/// The terminal result of a spawn route's inner future, the one chokepoint of
/// spawn-test (`execute_spawn_build`) and of spawn-named (`spawn_named`), read
/// before an `Err` is rendered to a body.
///
/// - A 2xx is the start's own verdict, by its body's `status`: `"healthy"` ran;
///   `"timeout"` never answered its wait; `"started"` is healthy when the
///   synchronous probe passed (the body carries `health_probe_ms`), else nothing
///   checked, and the health wait decides ([`start_verdict`]).
/// - Any other status is `Errored`, carrying the body's `error` token
///   (`runner_died_during_startup`, `runner_started_but_unresponsive`,
///   `frontend_dist_missing`, `frontend_stale`, the paired-profile failure).
/// - An `Err` is [`run_result_for`]'s: `Refused` when it provably ran nothing,
///   else `Errored`, `Timeout` and `Process` included: `Timeout` has three raise
///   sites (the queue watcher, the cargo timeout, the prewarm) that one variant
///   cannot separate, and both results classify `Failure`.
pub fn spawn_run_result(
    result: &Result<(StatusCode, serde_json::Value), SupervisorError>,
) -> StartVerdict {
    let (status, body) = match result {
        Err(e) => return StartVerdict::Now(run_result_for(e)),
        Ok(ok) => ok,
    };
    if !status.is_success() {
        let token = body
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("spawn_failed_http_{}", status.as_u16()));
        let reason = match body.get("message").and_then(|v| v.as_str()) {
            Some(message) if !message.starts_with(&token) => format!("{token}: {message}"),
            Some(message) => message.to_string(),
            None => token,
        };
        return StartVerdict::Now(ActionRunResult::Errored { reason });
    }
    let probe = match body.get("status").and_then(|v| v.as_str()) {
        Some("healthy") => SyncProbe::Healthy,
        Some("timeout") => SyncProbe::Unhealthy {
            reason: format!(
                "{UNHEALTHY_AFTER_START}: the spawned runner did not become healthy \
                 within its wait"
            ),
        },
        Some("started") if body.get("health_probe_ms").is_some_and(|v| !v.is_null()) => {
            SyncProbe::Healthy
        }
        Some("started") => SyncProbe::NotProbed,
        other => {
            return StartVerdict::Now(ActionRunResult::Errored {
                reason: format!("spawn_status_unrecognised: a 2xx body with status {other:?}"),
            })
        }
    };
    start_verdict(probe)
}

/// Record `verdict` on the action `guard` covers, or, for
/// [`StartVerdict::AfterHealthWait`], run the default health wait on
/// `runner_id` in a detached task and record `Ran` or
/// `runner_unhealthy_after_start` from it, so the caller's response stays
/// fire-and-forget. The guard moves into that task, so a task that ends with no
/// result still records the action abandoned.
pub fn record_start_verdict(
    state: &SharedState,
    runner_id: &str,
    verdict: StartVerdict,
    guard: RunResultGuard,
) {
    record_start_verdict_with(state, runner_id, verdict, guard, default_health_wait);
}

/// [`record_start_verdict`] with the health wait supplied: the seam the
/// detached half is tested through. Every production caller goes through
/// [`record_start_verdict`], which passes [`default_health_wait`].
pub fn record_start_verdict_with(
    state: &SharedState,
    runner_id: &str,
    verdict: StartVerdict,
    guard: RunResultGuard,
    health_wait: HealthWait,
) {
    match verdict {
        StartVerdict::Now(result) => {
            guard.record().record_run_result(result);
        }
        StartVerdict::AfterHealthWait => {
            if let Some(result) = no_health_wait_target(runner_id) {
                guard.record().record_run_result(result);
                return;
            }
            let state = state.clone();
            let runner_id = runner_id.to_string();
            tokio::spawn(async move {
                let action_id = guard.record().action_id;
                let result = match health_wait(&state, &runner_id, action_id).await {
                    Ok(()) => ActionRunResult::Ran,
                    Err(failure) => ActionRunResult::Errored {
                        reason: unhealthy_after_start_reason(
                            &runner_id,
                            &failure,
                            "a start nobody waited on",
                        ),
                    },
                };
                guard.record().record_run_result(result);
            });
        }
    }
}

/// The reason token of a start that left no runner to wait on.
pub const NO_RUNNER_TO_WAIT_ON: &str = "no_runner_to_wait_on";

/// The result to record instead of a health wait when there is no runner to
/// wait on. A restart resolves the primary's id after its start and passes
/// `""` when no primary is registered by then; probing `""` would only time
/// out and record `runner_unhealthy_after_start` about a runner that does not
/// exist. Say what happened instead.
fn no_health_wait_target(runner_id: &str) -> Option<ActionRunResult> {
    runner_id
        .trim()
        .is_empty()
        .then(|| ActionRunResult::Errored {
            reason: format!(
                "{NO_RUNNER_TO_WAIT_ON}: the start answered without waiting and no runner \
             was registered to run its health wait on"
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_start_with_no_runner_to_wait_on_records_that_instead_of_a_health_wait() {
        match no_health_wait_target("") {
            Some(ActionRunResult::Errored { reason }) => {
                assert!(reason.starts_with("no_runner_to_wait_on:"), "{reason}");
            }
            other => panic!("an empty runner id must record Errored, got {other:?}"),
        }
        assert!(no_health_wait_target("  ").is_some());
        assert_eq!(no_health_wait_target("primary"), None);
    }

    /// The errors a start or a build returns before it has touched anything.
    fn start_and_build_refusals() -> Vec<SupervisorError> {
        vec![
            SupervisorError::BuildPoolFull {
                queue_position: 1,
                active_builds: vec![],
                estimated_wait_secs: None,
            },
            SupervisorError::InsufficientDisk {
                free_bytes: 1,
                required_bytes: 2,
                footprint: Box::new(None),
            },
            SupervisorError::NoDisplay("no_display".to_string()),
            SupervisorError::UnverifiedExe(Box::new(crate::error::UnverifiedExeInfo {
                path: "qontinui-runner.exe".to_string(),
                mtime: None,
                build_sha: None,
                build_source: None,
                target_dir_source: "workspace_default".to_string(),
                detail: "unverified".to_string(),
            })),
            SupervisorError::PortHeldByLiveRunner { port: 9877, pid: 1 },
        ]
    }

    fn verdict_of(status: u16, body: serde_json::Value) -> StartVerdict {
        spawn_run_result(&Ok((StatusCode::from_u16(status).unwrap(), body)))
    }

    fn errored_reason(verdict: &StartVerdict) -> &str {
        match verdict {
            StartVerdict::Now(ActionRunResult::Errored { reason }) => reason,
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    /// A [`HealthWait`] that never sees the runner answer: it gives up after
    /// 30 s, on the tokio clock.
    fn never_healthy<'a>(
        _: &'a SharedState,
        _: &'a str,
        _: uuid::Uuid,
    ) -> BoxFuture<'a, Result<(), HealthProbeFailure>> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Err(HealthProbeFailure {
                elapsed_ms: 30_000,
                ended: HealthWaitEnd::BudgetSpent,
                recent_logs: Vec::new(),
            })
        })
    }

    /// A [`HealthWait`] whose runner answers at once.
    fn healthy_at_once<'a>(
        _: &'a SharedState,
        _: &'a str,
        _: uuid::Uuid,
    ) -> BoxFuture<'a, Result<(), HealthProbeFailure>> {
        Box::pin(async { Ok(()) })
    }

    /// Plan `2026-11-03-supervisor-dev-action-residuals-…` Phase 4.1: the
    /// detached half of [`record_start_verdict`]. A start that answered
    /// without waiting records nothing while its health wait runs, then the
    /// wait's verdict: `runner_unhealthy_after_start` naming the runner and
    /// the wait's elapsed time when it never answered, `Ran` when it did.
    #[tokio::test(start_paused = true)]
    async fn record_start_verdict_detached_records_unhealthy_after_start() {
        use crate::dev_action::record::{ActionKind, ActionRecord};
        use std::sync::Arc;
        use std::time::Duration;

        let (config, _root) = crate::config::config_over_an_empty_runner_tree();
        let state: SharedState = Arc::new(crate::state::SupervisorState::new(config));

        let record = Arc::new(ActionRecord::new(
            ActionKind::Spawn,
            None,
            "test".to_string(),
            &[],
        ));
        record_start_verdict_with(
            &state,
            "test-detached",
            StartVerdict::AfterHealthWait,
            RunResultGuard::arm(record.clone(), "the detached health-wait test"),
            never_healthy,
        );
        assert_eq!(
            record.run_result(),
            None,
            "a deferred verdict must not record at the spawn"
        );
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(
            record.run_result(),
            None,
            "the verdict was recorded before the health wait gave up"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        match record.run_result() {
            Some(ActionRunResult::Errored { reason }) => {
                assert!(reason.starts_with(UNHEALTHY_AFTER_START), "{reason}");
                assert!(reason.contains("'test-detached'"), "{reason}");
                assert!(reason.contains("30000ms"), "{reason}");
            }
            other => panic!("expected runner_unhealthy_after_start, got {other:?}"),
        }

        let came_up = Arc::new(ActionRecord::new(
            ActionKind::Spawn,
            None,
            "test".to_string(),
            &[],
        ));
        record_start_verdict_with(
            &state,
            "test-detached",
            StartVerdict::AfterHealthWait,
            RunResultGuard::arm(came_up.clone(), "the detached health-wait test"),
            healthy_at_once,
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(came_up.run_result(), Some(ActionRunResult::Ran));
    }

    /// The reason says how the wait ended, so a boot that exited early does not read
    /// as a runner that was merely slow for that long. Every ending keeps the token.
    #[test]
    fn a_failed_waits_reason_says_how_it_ended() {
        let reason = |ended, elapsed_ms| {
            unhealthy_after_start_reason(
                "primary",
                &HealthProbeFailure {
                    elapsed_ms,
                    ended,
                    recent_logs: Vec::new(),
                },
                "the start",
            )
        };

        let spent = reason(HealthWaitEnd::BudgetSpent, 90_000);
        assert!(
            spent.contains("did not become healthy within 90000ms of the start"),
            "{spent}"
        );
        let ended = reason(HealthWaitEnd::BootEnded, 2_000);
        assert!(ended.contains("ended 2000ms into"), "{ended}");
        assert!(!ended.contains("did not become healthy"), "{ended}");
        let removed = reason(HealthWaitEnd::RunnerRemoved, 5);
        assert!(removed.contains("left the registry 5ms"), "{removed}");
        for reason in [spent, ended, removed] {
            assert!(reason.starts_with(UNHEALTHY_AFTER_START), "{reason}");
            assert!(reason.contains("'primary'"), "{reason}");
        }
    }

    /// T1-3: one row per spawn exit.
    #[test]
    fn spawn_run_result_maps_every_spawn_exit() {
        assert_eq!(
            verdict_of(200, json!({"status": "healthy"})),
            StartVerdict::Now(ActionRunResult::Ran)
        );
        assert!(
            errored_reason(&verdict_of(200, json!({"status": "timeout"})))
                .contains(UNHEALTHY_AFTER_START)
        );
        assert_eq!(
            verdict_of(200, json!({"status": "started"})),
            StartVerdict::AfterHealthWait,
            "a start nothing checked defers to the health wait"
        );
        assert_eq!(
            verdict_of(200, json!({"status": "started", "health_probe_ms": 812})),
            StartVerdict::Now(ActionRunResult::Ran),
            "spawn-test's synchronous probe passed"
        );

        for (status, token) in [
            (500, "runner_died_during_startup"),
            (502, "runner_started_but_unresponsive"),
            (503, "frontend_dist_missing"),
            (503, "frontend_stale"),
            (400, "profile_not_found"),
            (500, "profile_apply_failed"),
        ] {
            let verdict = verdict_of(status, json!({"error": token, "message": "why"}));
            let reason = errored_reason(&verdict);
            assert!(reason.contains(token), "{status}: {reason}");
        }

        for err in start_and_build_refusals() {
            let name = format!("{err:?}");
            assert!(
                matches!(
                    spawn_run_result(&Err(err)),
                    StartVerdict::Now(ActionRunResult::Refused { .. })
                ),
                "{name} ran nothing, so it is Refused"
            );
        }

        for err in [
            SupervisorError::Timeout("queue".to_string()),
            SupervisorError::BuildFailed("could not compile".to_string()),
            SupervisorError::Process(
                "Runner binary not found in any build slot. Use rebuild: true".to_string(),
            ),
        ] {
            let name = format!("{err:?}");
            assert!(
                matches!(
                    spawn_run_result(&Err(err)),
                    StartVerdict::Now(ActionRunResult::Errored { .. })
                ),
                "{name} is Errored"
            );
        }
    }

    /// T1-14 (D9): a start that observed its runner's health records that
    /// verdict now; one that observed none (restart `?wait=false`, spawn-named
    /// `wait: false`, spawn-test `health_probe_timeout_ms: 0` with no wait)
    /// defers to the detached health wait and never records `Ran` at the spawn.
    #[test]
    fn a_start_with_no_synchronous_probe_records_the_health_waits_verdict() {
        assert_eq!(
            start_verdict(SyncProbe::NotProbed),
            StartVerdict::AfterHealthWait
        );
        assert_eq!(
            start_verdict(SyncProbe::Healthy),
            StartVerdict::Now(ActionRunResult::Ran)
        );
        assert_eq!(
            start_verdict(SyncProbe::Unhealthy {
                reason: "runner_unhealthy_after_start: x".to_string()
            }),
            StartVerdict::Now(ActionRunResult::Errored {
                reason: "runner_unhealthy_after_start: x".to_string()
            })
        );
        // spawn-named under `wait: false` and spawn-test with no probe and no
        // wait answer `"started"` with no `health_probe_ms`.
        assert_eq!(
            verdict_of(200, json!({"status": "started", "health_probe_ms": null})),
            StartVerdict::AfterHealthWait
        );
    }

    /// The readiness gate's refusal maps to `Refused`, carrying the
    /// `restart_refused_unsafe` token verbatim — everything else to `Errored`.
    /// Both classify to `Failure`; §5 rejects a sixth `D3Category`.
    #[test]
    fn run_result_for_maps_the_readiness_refusal_to_refused() {
        let detail = crate::restart_readiness::RefusalDetail {
            cause: "sessions_live",
            message: "restart_refused_unsafe: runner 'primary' (port 9876) reports it is NOT \
                      safe to restart."
                .to_string(),
            payload: serde_json::json!({"error": "restart_refused_unsafe"}),
        };
        let refused = run_result_for(&SupervisorError::RestartUnsafe(Box::new(detail)));
        match &refused {
            ActionRunResult::Refused { reason } => {
                assert!(
                    reason.contains("restart_refused_unsafe"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(
            crate::dev_action::attribution::classify(Some(&refused), &[]),
            crate::dev_action::record::D3Category::Failure
        );

        for precondition in [
            SupervisorError::RunnerAlreadyRunning,
            SupervisorError::Validation("automated restart blocked".to_string()),
            SupervisorError::RunnerLifecycleBusy("named-a".to_string()),
            SupervisorError::StartSuperseded("runner 'primary' changed".to_string()),
        ]
        .into_iter()
        .chain(start_and_build_refusals())
        {
            let reason = precondition.to_string();
            assert_eq!(
                run_result_for(&precondition),
                ActionRunResult::Refused { reason },
                "{precondition:?}"
            );
            // The same refusal after a body acted (a restart's stop ran) is not
            // "nothing ran": a `Refused` would close the window on that stop.
            let reason = precondition.to_string();
            assert_eq!(
                run_result_for(&SupervisorError::after_act(precondition)),
                ActionRunResult::Errored { reason },
            );
        }

        let errored = run_result_for(&SupervisorError::RunnerNotRunning);
        assert!(matches!(errored, ActionRunResult::Errored { .. }));
        assert_eq!(
            crate::dev_action::attribution::classify(Some(&errored), &[]),
            crate::dev_action::record::D3Category::Failure
        );
    }
}

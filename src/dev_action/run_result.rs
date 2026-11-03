//! Mapping an executing path's exit to its action's terminal
//! [`ActionRunResult`], and recording it.
//!
//! The attribution watcher holds every window open until the action records its
//! own terminal result ([`crate::dev_action::attribution`]), so every path that
//! mints an action must record one on every exit. This module holds the maps
//! those paths share:
//!
//! - [`run_result_for`]: an error that ended a primary-runner action;
//! - [`spawn_run_result`]: the result of a spawn route's inner future, the one
//!   chokepoint spawn-test and spawn-named each have;
//! - [`start_verdict`] and [`record_start_verdict`]: what a start records from
//!   the health it observed before answering, and, when it observed none, the
//!   detached health wait that decides instead (plan
//!   `2026-10-29-supervisor-spawn-attribution-window-and-hermetic-dev-action-coord`
//!   D9).

use axum::http::StatusCode;

use crate::dev_action::record::{ActionRunResult, RunResultGuard};
use crate::error::SupervisorError;
use crate::process::health_probe::wait_for_runner_healthy_default;
use crate::state::SharedState;

/// The reason token of a start whose runner never answered its health check.
pub const UNHEALTHY_AFTER_START: &str = "runner_unhealthy_after_start";

/// Map an error that ended a primary-runner action to the action's own terminal
/// result.
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
/// Four precondition refusals map to `Refused` too, carried as their own
/// message: [`SupervisorError::RunnerAlreadyRunning`] (a start found the
/// runner up), [`SupervisorError::Validation`] (a request or configuration the
/// step will not act on, e.g. `manager::automated_restart_blocked`),
/// [`SupervisorError::RunnerLifecycleBusy`] (a start or stop holds the runner)
/// and [`SupervisorError::StartSuperseded`] (a crash-watchdog start that found
/// the runner started, stopped or removed since the exit it answers).
/// Each refuses the step it guards before that step touches anything, which is
/// what `Refused` asserts: nothing ran. That holds for a start door and for a
/// restart refused before it acted. It does NOT hold for a restart's start
/// refused AFTER its stop or build ran (a start slipped in between them, or the port
/// `apply_non_primary_instance_env_with` refuses): the stop is an effect, and a
/// `Refused` would close the attribution window on it unobserved. The body
/// that acted marks such an exit [`SupervisorError::AfterAct`], which this map
/// reads as `Errored` whatever it wraps; the door's own record takes it, so no
/// store lookup (which the store's eviction cap could miss) is involved.
///
/// The map is not widened for spawns: a restart's `PortHeldByLiveRunner`
/// follows a stop that already ran, so `Errored` is right there.
/// [`spawn_run_result`] handles the spawn-only arms and delegates the rest here.
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
        | SupervisorError::StartSuperseded(_)) => ActionRunResult::Refused {
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
    /// The runner never answered within the budget; `reason` names it.
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
/// - An `Err` that provably ran nothing is `Refused`: the pool was full under
///   `no-wait`, the disk guard refused, there is no display, the exe has no
///   identity, or a live runner holds the port. Every other `Err` is `Errored`,
///   `Timeout` and `Process` included: `Timeout` has three raise sites (the
///   queue watcher, the cargo timeout, the prewarm) that one variant cannot
///   separate, and both results classify `Failure`.
pub fn spawn_run_result(
    result: &Result<(StatusCode, serde_json::Value), SupervisorError>,
) -> StartVerdict {
    let (status, body) = match result {
        Err(
            e @ (SupervisorError::BuildPoolFull { .. }
            | SupervisorError::InsufficientDisk { .. }
            | SupervisorError::NoDisplay(_)
            | SupervisorError::UnverifiedExe(_)
            | SupervisorError::PortHeldByLiveRunner { .. }),
        ) => {
            return StartVerdict::Now(ActionRunResult::Refused {
                reason: e.to_string(),
            })
        }
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
                let result = match wait_for_runner_healthy_default(&state, &runner_id).await {
                    Ok(()) => ActionRunResult::Ran,
                    Err(failure) => ActionRunResult::Errored {
                        reason: format!(
                            "{UNHEALTHY_AFTER_START}: runner '{runner_id}' did not become \
                             healthy within {}ms of a start that answered without waiting",
                            failure.elapsed_ms
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

    fn verdict_of(status: u16, body: serde_json::Value) -> StartVerdict {
        spawn_run_result(&Ok((StatusCode::from_u16(status).unwrap(), body)))
    }

    fn errored_reason(verdict: &StartVerdict) -> &str {
        match verdict {
            StartVerdict::Now(ActionRunResult::Errored { reason }) => reason,
            other => panic!("expected Errored, got {other:?}"),
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

        let refused = [
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
        ];
        for err in refused {
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
        ] {
            let reason = precondition.to_string();
            assert_eq!(
                run_result_for(&precondition),
                ActionRunResult::Refused { reason },
                "{precondition:?}"
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

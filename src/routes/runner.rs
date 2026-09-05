use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::dev_action::{
    assess_action_risk, evaluate_all, fetch_expectations, spawn_attribution_watcher, ActionKind,
    ActionRecord, ActionRisk, ActionRunResult, AttributionTargets, PredictedSignature,
    SlotResolution,
};
use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::claude_env::StripInheritedClaudeMarkers;
use crate::process::health_probe::{wait_for_runner_healthy_default, HealthProbeFailure};
use crate::process::manager;
use crate::routes::optional_json::OptionalJson;
use crate::state::SharedState;

/// Query string for endpoints that opt-OUT of the post-spawn health wait.
///
/// Default is `wait=true` (poll up to 30s for the runner's `/health` to come
/// up before returning 200). Pass `?wait=false` to get the legacy
/// fire-and-forget behavior (200 the moment the OS process spawns, even if
/// it's hung).
#[derive(Deserialize, Default)]
pub struct StartWaitQuery {
    #[serde(default = "default_wait_true")]
    pub wait: bool,
}

fn default_wait_true() -> bool {
    true
}

/// Build the standard 503 body for a runner that started but didn't bind
/// its API port within the wait budget. Shared between the legacy
/// `/runner/restart` and the per-runner `/runners/{id}/start` so both
/// endpoints expose an identical contract.
pub(crate) fn unhealthy_after_start_response(
    runner_id: &str,
    failure: HealthProbeFailure,
) -> Response {
    let body = serde_json::json!({
        "error": "runner_unhealthy_after_start",
        "runner_id": runner_id,
        "elapsed_ms": failure.elapsed_ms,
        "recent_logs": failure.recent_logs,
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

/// Render an [`ActionRisk`] as the `risk` ACK field: `null` when there is no
/// actionable risk (healthy / no-history case), otherwise an object carrying the
/// `route_lkg` advice, the human-readable `warning`, and the `top_signature`
/// evidence so the initiator sees exactly what tripped the assessment.
fn risk_ack_json(risk: &ActionRisk) -> serde_json::Value {
    if !risk.is_actionable() {
        return serde_json::Value::Null;
    }
    serde_json::json!({
        "route_lkg": risk.route_lkg,
        "warning": risk.warn,
        "top_signature": risk.top_signature,
    })
}

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
fn run_result_for(err: &SupervisorError) -> ActionRunResult {
    match err {
        SupervisorError::RestartUnsafe(detail) => ActionRunResult::Refused {
            reason: detail.message.clone(),
        },
        other => ActionRunResult::Errored {
            reason: other.to_string(),
        },
    }
}

/// Mint a dev-action snapshot for a primary-runner action (restart / build),
/// evaluate the active dev-state set at execution time, store the record, and
/// spawn the detached attribution watcher.
///
/// State evaluation includes `LEGACY_EXE_FALLBACK` via a cheap
/// `resolve_source_exe_with_slot` probe — the `None` slot id is the
/// genuinely-new signal (§2 of the plan) that the legacy `target/debug` exe
/// (no embedded assets) is what would deploy. Resolution is a read of facts
/// already in memory, so this stays off the blocking green path (§8 criterion
/// 6).
///
/// Returns `(record, states_active, predicted, risk)`. The `Arc<ActionRecord>`
/// is returned (rather than just its id) so the executing path can record the
/// action's **terminal result** on it — the input the attribution watcher was
/// missing, without which a refused or failed action still closed `Confirmed`
/// (`plans/2026-09-04-supervisor-refused-restart-reports-confirmed.md`).
/// Phase 4: `predicted` is coord's state-conditioned expectations for this
/// `(kind, states_active)` — fetched best-effort with a 2s timeout so the
/// initiating agent is warned BEFORE the action completes. An empty vec when
/// coord is unreachable / has no history (fail-open; never blocks the action).
///
/// "Act on predictions": `risk` is the pure
/// [`assess_action_risk`] verdict over `predicted` + `states_active`. It is
/// [`ActionRisk::none`] in the common healthy / no-history case (behavior
/// unchanged) and carries `route_lkg` + a loud `warn` when a boot-failure-class
/// signature clears the threshold. The caller decides what to do with it
/// (the restart path biases toward LKG + warns; never refuses).
async fn mint_primary_action(
    state: &SharedState,
    kind: ActionKind,
    params_digest: String,
    requester_id: Option<String>,
    build_duration: Option<std::time::Duration>,
) -> (
    std::sync::Arc<ActionRecord>,
    Vec<&'static str>,
    Vec<PredictedSignature>,
    ActionRisk,
) {
    // Resolve the slot the start path WOULD deploy so LEGACY_EXE_FALLBACK is a
    // real signal, not Unknown. A resolution error (no exe anywhere) ⇒ leave it
    // unevaluated so the state surfaces as `Unknown`, never a false `False`.
    let slot_resolution = match manager::resolve_source_exe_with_origin(state).await {
        Ok((origin, _)) => SlotResolution::Resolved(origin),
        Err(_) => SlotResolution::NotEvaluated,
    };
    let states = evaluate_all(state, slot_resolution).await;
    let record = ActionRecord::new(kind, requester_id, params_digest, &states);
    let action_id = record.action_id;
    // Stamp the ACK with the canonical string ids (Phase-2b stores typed
    // `DevState`; the ACK wire form is the canonical-id array, unchanged).
    let states_active: Vec<&'static str> =
        record.states_active.iter().map(|s| s.as_str()).collect();

    // Phase 4: fetch coord's state-conditioned expectations for this
    // (kind, states) so the ACK warns the agent before the action completes.
    // Best-effort + 2s timeout; an empty vec on any failure (fail-open).
    let predicted = fetch_expectations(kind.as_str(), &states_active, 5).await;

    // "Act on predictions": assess the prediction set against the active states.
    // Pure + fail-open — `none()` in the healthy case. `route_lkg` + `warn` only
    // when a boot-failure-class signature clears the threshold. Emit the warning
    // here (loud `tracing::warn!` + a Supervisor log entry) so the risk is
    // visible BEFORE the action's outcome lands, regardless of how the caller
    // uses the advice.
    let risk = assess_action_risk(&predicted, &states_active);
    if let Some(warn) = risk.warn.as_deref() {
        tracing::warn!(action_id = %action_id, kind = kind.as_str(), "{warn}");
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, warn.to_string())
            .await;
    }

    let arc = state.dev_actions.write().await.insert(record);

    // The watcher targets the primary's early-log + cached health. The
    // early-log path is read at window close (it may not be set until the
    // restart spawns the runner); capture the runner id now so the health scan
    // can find the right cached snapshot.
    let (early_log_path, runner_id) = match state.get_primary().await {
        Some(primary) => (
            primary.early_log_path.read().await.clone(),
            Some(primary.config.id.clone()),
        ),
        None => (None, None),
    };
    spawn_attribution_watcher(
        state.clone(),
        arc.clone(),
        AttributionTargets {
            early_log_path,
            managed: None,
            panic_log_path: None,
            runner_id,
        },
        build_duration,
    );

    (arc, states_active, predicted, risk)
}

#[derive(Deserialize)]
pub struct RestartRequest {
    #[serde(default)]
    pub rebuild: bool,
    #[serde(default)]
    pub force: bool,
    /// Phase B escape hatch. Only meaningful with `rebuild: true`.
    ///
    /// - `false` (DEFAULT) — the rebuild materializes a fresh `origin/main`
    ///   worktree and compiles THAT (provenance `origin_main`), so the primary
    ///   always runs latest-green-main and never touches the contested working
    ///   checkout. This is the canonical path.
    /// - `true` — legacy behavior: compile the live working tree
    ///   (`project_dir`, provenance `live_tree`), for the rare case the operator
    ///   deliberately wants the primary to run uncommitted local changes. The
    ///   canonical WIP-test path is a temp runner via spawn-test, NOT the
    ///   primary.
    #[serde(default)]
    pub from_working_tree: bool,
    /// "Act on predictions" opt-out / explicit exe selector for the NON-rebuild
    /// restart path.
    ///
    /// - `None` (DEFAULT) — no explicit operator choice. The supervisor is free
    ///   to AUTO-bias toward the last-known-good binary when
    ///   [`assess_action_risk`] flags this restart as high-risk (a
    ///   boot-failure-class prediction under the active states), instead of the
    ///   freshest slot exe that history says white-screens. The restart is never
    ///   refused — only biased — and the risk is warned loudly.
    /// - `Some(true)` — explicitly pin this restart to the LKG binary (same
    ///   effect the auto-bias would apply, but operator-requested).
    /// - `Some(false)` — explicitly OPT OUT of the auto-LKG bias: run the
    ///   freshest slot exe even when the prediction is high-risk. The risk is
    ///   still warned, but the supervisor respects the explicit choice and does
    ///   NOT route to LKG.
    ///
    /// Only meaningful on the `rebuild: false` restart (the rebuild path
    /// compiles a fresh exe, so an LKG pin would be self-defeating).
    #[serde(default)]
    pub use_lkg: Option<bool>,
}

#[derive(Deserialize)]
pub struct WatchdogRequest {
    pub enabled: bool,
    #[serde(default)]
    pub reset_attempts: bool,
}

/// Body for `POST /runner/stop`. Every field defaults, so the body as a whole
/// is optional — see [`OptionalJson`] for why that needs a custom extractor
/// rather than `Option<Json<_>>`.
#[derive(Deserialize, Default)]
pub struct StopRequest {
    #[serde(default)]
    pub force: bool,
}

pub async fn stop_runner(
    State(state): State<SharedState>,
    body: OptionalJson<StopRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let force = body.into_inner_or_default().force;
    manager::stop_runner(&state, force).await?;

    Ok(Json(serde_json::json!({
        "status": "stopped",
        "message": "Runner stopped successfully"
    })))
}

/// Body for `POST /runner/rebuild-when-idle`. Every field defaults, so the body
/// as a whole is optional — see [`OptionalJson`] for why that needs a custom
/// extractor rather than `Option<Json<_>>`.
#[derive(Deserialize)]
pub struct RebuildWhenIdleRequest {
    /// Give up after this long rather than waiting forever. Expiry NEVER
    /// rebuilds — it reports that it stopped waiting.
    #[serde(default = "default_idle_deadline_secs")]
    pub deadline_secs: u64,
    /// How often to re-probe the primary's session planes.
    #[serde(default = "default_idle_poll_secs")]
    pub poll_secs: u64,
}

fn default_idle_deadline_secs() -> u64 {
    6 * 60 * 60
}

fn default_idle_poll_secs() -> u64 {
    30
}

impl Default for RebuildWhenIdleRequest {
    fn default() -> Self {
        Self {
            deadline_secs: default_idle_deadline_secs(),
            poll_secs: default_idle_poll_secs(),
        }
    }
}

/// `POST /runner/rebuild-when-idle` — watch the primary until its Claude
/// sessions drain, then rebuild it from `origin/main`.
///
/// WHY THIS IS NOT "restart the primary on a timer". `CLAUDE.md`'s runner
/// lifecycle scope says the supervisor never restarts a user-managed runner
/// that is SERVING, with one carved-out exception (the serving watchdog) whose
/// terms are: act only on a zero census, refuse on a non-zero count AND on a
/// census that cannot be taken, never against `stop_requested`/`restart_requested`,
/// and stay bounded. This endpoint is operator-initiated but FIRES UNATTENDED,
/// so it is held to those same terms rather than to a plain button's.
///
/// Concretely:
///
///   * Only [`Readiness::Safe`] with BOTH session planes at zero, or
///     [`Readiness::CensusSafe`] (zero live `claude` under the runner's pid),
///     is treated as drained.
///   * Every `Unsafe`, `CensusUnsafe` and **every `Unknown`** keeps waiting. An
///     unreachable runner, a 404 from an old build, an unparseable body and a
///     census that could not be taken are all "not proven idle", never "idle"
///     [policy: verification-and-evidence silent-empty-is-unknown].
///   * The deadline EXPIRES the wait; it never converts into a rebuild.
///   * `force` stays false, so [`restart_readiness::enforce`] — the same gate a
///     manual restart passes through — re-runs at fire time inside
///     `manager::restart_runner` and is the AUTHORITATIVE decision. The loop
///     below is a quiet optimistic poller; it cannot talk the gate into
///     anything, and if the two ever disagree the gate wins and the refusal is
///     reported.
///   * `from_working_tree` stays false, so the rebuild compiles a fresh
///     `origin/main` worktree — "latest code" as asked, not the contested
///     working checkout.
///
/// Returns 202 + `build_id` immediately; poll `GET /build/{id}/status`.
pub async fn rebuild_when_idle(
    State(state): State<SharedState>,
    body: OptionalJson<RebuildWhenIdleRequest>,
) -> Result<Response, SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound("primary".to_string()))?;
    let port = primary.config.port;
    let runner_name = primary.config.name.clone();

    // Every field defaults, so an absent body is the all-defaults request.
    let body = body.into_inner_or_default();
    let poll_secs = body.poll_secs.clamp(5, 600);
    let deadline_secs = body.deadline_secs.clamp(60, 24 * 60 * 60);

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Rebuild-when-idle armed for '{}' (poll {}s, give up after {}s)",
                runner_name, poll_secs, deadline_secs
            ),
        )
        .await;

    // A rebuild-restart is a `build`-kind action, exactly as the rebuild arm of
    // `restart_runner` mints it — the outcome lands after the build, not after
    // this 202.
    let (action_record, _states_active, _predicted, risk) = mint_primary_action(
        &state,
        ActionKind::Build,
        format!(
            "rebuild_when_idle;poll_secs={};deadline_secs={}",
            poll_secs, deadline_secs
        ),
        None,
        None,
    )
    .await;
    let action_id = action_record.action_id;
    let risk_json = risk_ack_json(&risk);

    let exec_state = state.clone();
    let exec_record = action_record.clone();
    // The detached future outlives this handler, so it gets its own copy of the
    // name rather than borrowing the one the 202 body still needs.
    let exec_runner_name = runner_name.clone();
    let (submission_id, _arc) = crate::build_submissions::submit_detached(
        state.build_submissions.clone(),
        state.config.project_dir.clone(),
        None,
        async move {
            let started = std::time::Instant::now();
            let deadline = std::time::Duration::from_secs(deadline_secs);
            let poll = std::time::Duration::from_secs(poll_secs);
            let mut ticks: u64 = 0;
            // Assigned at the top of every iteration before it is read; no
            // initial value is meaningful, and giving it one only earns an
            // unused_assignments warning under clippy -D warnings.
            let mut last_reason: String;

            loop {
                // Re-read the primary every tick: it can be replaced, stopped,
                // or removed while we wait, and a handle captured once would
                // describe a runner that no longer exists.
                let Some(managed) = exec_state.get_primary().await else {
                    exec_record.record_run_result(ActionRunResult::Errored {
                        reason: "primary_disappeared_while_waiting".to_string(),
                    });
                    return (
                        StatusCode::NOT_FOUND.as_u16(),
                        serde_json::json!({
                            "status": "aborted",
                            "error": "primary_disappeared_while_waiting",
                            "waited_secs": started.elapsed().as_secs(),
                        }),
                    );
                };

                let (port_open, api_responding) = {
                    let h = managed.cached_health.read().await;
                    (h.runner_port_open, h.runner_responding)
                };
                let (liveness, root_pid, started_at) = {
                    let r = managed.runner.read().await;
                    (r.liveness(port_open, api_responding), r.pid, r.started_at)
                };

                let readiness = crate::restart_readiness::probe_with_census_fallback(
                    port, liveness, root_pid, started_at,
                )
                .await;

                let verdict = crate::restart_readiness::drained_for_deferred_rebuild(&readiness);
                let drained = verdict.drained;
                last_reason = verdict.reason;

                if drained {
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Info,
                            format!(
                                "Rebuild-when-idle: '{}' drained after {}s ({}); rebuilding from origin/main",
                                exec_runner_name,
                                started.elapsed().as_secs(),
                                last_reason
                            ),
                        )
                        .await;
                    break;
                }

                if started.elapsed() >= deadline {
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!(
                                "Rebuild-when-idle: gave up on '{}' after {}s without rebuilding ({})",
                                exec_runner_name,
                                started.elapsed().as_secs(),
                                last_reason
                            ),
                        )
                        .await;
                    exec_record.record_run_result(ActionRunResult::Errored {
                        reason: format!("deadline_expired_while_waiting: {last_reason}"),
                    });
                    return (
                        StatusCode::REQUEST_TIMEOUT.as_u16(),
                        serde_json::json!({
                            "status": "gave_up_waiting",
                            "reason": last_reason,
                            "waited_secs": started.elapsed().as_secs(),
                            "ticks": ticks,
                            "note": "The deadline expired. Nothing was rebuilt and nothing was stopped.",
                        }),
                    );
                }

                // Log the first observation and then only occasionally, so a
                // multi-hour wait does not flood the log pane.
                if ticks == 0 || ticks.is_multiple_of(20) {
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Info,
                            format!(
                                "Rebuild-when-idle: still waiting on '{}' after {}s — {}",
                                exec_runner_name,
                                started.elapsed().as_secs(),
                                last_reason
                            ),
                        )
                        .await;
                }
                ticks += 1;
                tokio::time::sleep(poll).await;
            }

            // Drained. Fire the SAME call the Rebuild button makes: rebuild
            // (origin/main, because `from_working_tree` is false) then restart.
            // `force: false` keeps the readiness gate armed inside — it is the
            // authoritative check, and a refusal here is a real refusal.
            if let Err(e) =
                manager::restart_runner(&exec_state, true, RestartSource::Manual, false, false)
                    .await
            {
                exec_record.record_run_result(run_result_for(&e));
                exec_state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Error,
                        format!("Rebuild-when-idle: rebuild failed after drain: {}", e),
                    )
                    .await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    serde_json::json!({
                        "status": "error",
                        "error": e.to_string(),
                        "waited_secs": started.elapsed().as_secs(),
                    }),
                );
            }

            (
                StatusCode::OK.as_u16(),
                serde_json::json!({
                    "status": "rebuilt",
                    "waited_secs": started.elapsed().as_secs(),
                    "ticks": ticks,
                    "drained_reason": last_reason,
                }),
            )
        },
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "watching",
            "build_id": submission_id,
            "action_id": action_id,
            "runner": runner_name,
            "poll_secs": poll_secs,
            "deadline_secs": deadline_secs,
            "risk": risk_json,
            "message": "Watching the primary for active sessions. It will rebuild from origin/main once they drain, and give up without rebuilding if the deadline passes first.",
        })),
    )
        .into_response())
}

pub async fn restart_runner(
    State(state): State<SharedState>,
    Query(wait_q): Query<StartWaitQuery>,
    Json(body): Json<RestartRequest>,
) -> Result<Response, SupervisorError> {
    let rebuild = body.rebuild;

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Restart requested (rebuild: {})", rebuild),
        )
        .await;

    // Mint a dev-action snapshot + evaluate the active dev-state set BEFORE the
    // restart runs (the facts are in memory at resolve time). A rebuild-restart
    // is a `build`-kind action (its outcome lands after the ~10-20min build),
    // an in-place restart is a `restart`-kind action. The attribution watcher
    // (spawned inside `mint_primary_action`) folds the verdict in after the
    // per-kind window and writes it back for `GET /actions/{id}/outcome`.
    let action_kind = if rebuild {
        ActionKind::Build
    } else {
        ActionKind::Restart
    };
    let (action_record, states_active, predicted, risk) = mint_primary_action(
        &state,
        action_kind,
        format!("rebuild={};force={}", rebuild, body.force),
        None,
        None,
    )
    .await;
    let action_id = action_record.action_id;
    let risk_json = risk_ack_json(&risk);

    // ── Rebuild path: DETACH from the HTTP connection. ──────────────────────
    //
    // A `rebuild: true` restart runs a full `cargo build` of the live runner
    // tree (~10-20 min) followed by a start. Previously the whole stop → build
    // → start sequence ran *inside* this handler future, so when the client
    // disconnected (short `curl -m N`, E2E timeout, Ctrl-C) axum dropped the
    // future and the in-flight build was abandoned mid-flight (process-wrap's
    // KillOnDrop killed the cargo tree) — the "rebuild wedge": the runner never
    // restarted and the slot was left with a partial exe and no post-build
    // bookkeeping.
    //
    // The build+restart now runs detached via the #63 build-submissions state
    // machine ([`submit_detached`], same seam `/runner/fix-and-rebuild` uses).
    // The detached task owns its own `SharedState` clone, so it survives this
    // handler future being dropped. We return 202 immediately; callers poll
    // `GET /builds` (or `GET /build/{id}/status`) for the terminal outcome.
    if rebuild {
        let exec_state = state.clone();
        // Phase 3 path 1: the detached body is where the observed refusal
        // landed. It owns a clone of the record so the terminal result reaches
        // the watcher BEFORE the window closes.
        let exec_record = action_record.clone();
        let force = body.force;
        let from_working_tree = body.from_working_tree;
        let do_health_wait = wait_q.wait;
        let (submission_id, _arc) = crate::build_submissions::submit_detached(
            state.build_submissions.clone(),
            state.config.project_dir.clone(),
            None,
            async move {
                // The exact stop → build → start sequence the handler used to
                // await inline. Errors are surfaced in the (status, body)
                // result so they land in /builds AND are logged below — never
                // silently dropped.
                if let Err(e) = manager::restart_runner(
                    &exec_state,
                    true,
                    RestartSource::Manual,
                    force,
                    from_working_tree,
                )
                .await
                {
                    // The refusal / error the dev-action outcome used to lose.
                    // Recorded before the early return so the watcher reads it
                    // when it composes the verdict.
                    exec_record.record_run_result(run_result_for(&e));
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Error,
                            format!("Detached rebuild-restart failed: {}", e),
                        )
                        .await;
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        serde_json::json!({
                            "status": "error",
                            "error": e.to_string(),
                        }),
                    );
                }

                // Port-bind verification (the manager call returns once the OS
                // process spawns; the runner may still be pre-bind or hung).
                // `wait=false` opts out (legacy behavior).
                if do_health_wait {
                    if let Some(primary) = exec_state.get_primary().await {
                        let runner_id = primary.config.id.clone();
                        if let Err(failure) =
                            wait_for_runner_healthy_default(&exec_state, &runner_id).await
                        {
                            exec_state
                                .logs
                                .emit(
                                    LogSource::Supervisor,
                                    LogLevel::Warn,
                                    format!(
                                        "Runner '{}' did not become healthy {}ms after \
                                         detached rebuild-restart",
                                        runner_id, failure.elapsed_ms
                                    ),
                                )
                                .await;
                            // Phase 3 path 4: the runner started but never
                            // bound its port. It RAN, and it failed.
                            exec_record.record_run_result(ActionRunResult::Errored {
                                reason: format!(
                                    "runner_unhealthy_after_start: runner '{}' did not become \
                                     healthy within {}ms of the detached rebuild-restart",
                                    runner_id, failure.elapsed_ms
                                ),
                            });
                            return (
                                StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                                serde_json::json!({
                                    "status": "unhealthy_after_start",
                                    "error": "runner_unhealthy_after_start",
                                    "runner_id": runner_id,
                                    "elapsed_ms": failure.elapsed_ms,
                                    "recent_logs": failure.recent_logs,
                                }),
                            );
                        }
                    }
                }

                // Phase A4: the rebuild is detached, so the resolved built sha
                // does not exist when the 202 was sent. Evaluate origin/main
                // drift HERE and attach it to the recorded outcome body — it
                // surfaces via `GET /build/{id}/status` and seeds the next
                // `GET /builds`. `warn!` is the loud signal for the rebuild
                // path. Best-effort: a non-repo / missing remote yields `null`
                // and never fails the restart.
                //
                // ONLY meaningful for a `from_working_tree` rebuild. The
                // DEFAULT primary rebuild compiles a supervisor-materialized
                // `origin/main` worktree (`primary_rebuild_from_origin_main`),
                // not the live checkout — probing the live tree there reports a
                // sha this build never compiled and can raise a false "the
                // primary may NOT be running latest main" alarm when the shared
                // checkout is parked on someone's branch. Same wrong-tree
                // defect as the build-time provenance warning. An origin/main
                // worktree is at origin/main by construction, so there is
                // nothing to compare: report `null` rather than a wrong sha.
                let drift_json = match exec_state
                    .config
                    .project_dir
                    .parent()
                    .filter(|_| from_working_tree)
                {
                    Some(repo_root) => {
                        let head = crate::build_monitor::rev_parse_head(repo_root).await;
                        match head {
                            Some(sha) => {
                                let drift =
                                    crate::git_provenance::origin_main_drift(repo_root, &sha).await;
                                if drift.origin_main_sha.is_empty() || drift.is_up_to_date() {
                                    serde_json::Value::Null
                                } else {
                                    let msg = format!(
                                        "Detached rebuild-restart: the just-built primary sha {} \
                                         is {} origin/main ({}) by {} commit(s) (diverged={}). \
                                         The primary may NOT be running latest main.",
                                        drift.built_sha.chars().take(12).collect::<String>(),
                                        if drift.is_diverged() {
                                            "DIVERGED from"
                                        } else {
                                            "behind"
                                        },
                                        drift.origin_main_sha.chars().take(12).collect::<String>(),
                                        drift.behind_count,
                                        drift.is_diverged(),
                                    );
                                    tracing::warn!("{}", msg);
                                    exec_state
                                        .logs
                                        .emit(LogSource::Build, LogLevel::Warn, msg)
                                        .await;
                                    serde_json::json!({
                                        "built_sha": drift.built_sha,
                                        "origin_main_sha": drift.origin_main_sha,
                                        "behind_count": drift.behind_count,
                                        "is_ancestor": drift.is_ancestor,
                                        "diverged": drift.is_diverged(),
                                        "fetched": drift.fetched,
                                    })
                                }
                            }
                            None => serde_json::Value::Null,
                        }
                    }
                    None => serde_json::Value::Null,
                };

                // The build + restart completed. `Ran` leaves the effect
                // surfaces in charge of the verdict (existing precedence) and,
                // critically, CLOSES the window here rather than at the
                // backstop — the window is now a measurement of the action.
                exec_record.record_run_result(ActionRunResult::Ran);

                (
                    StatusCode::OK.as_u16(),
                    serde_json::json!({
                        "status": "restarted",
                        "message": "Runner restarted successfully (with rebuild)",
                        "origin_main_drift": drift_json,
                    }),
                )
            },
        );

        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                // Phase 4: this ACK is emitted BEFORE the readiness gate runs,
                // so it cannot honestly claim the rebuild is under way — the
                // observed case is a restart refused 1.4s later that had already
                // been ACKed as "rebuilding". `submitted` is what is actually
                // known at this point. The gate deliberately stays BEHIND the
                // 202 (plan §6 Phase 4): moving it ahead would reverse PR #64's
                // test-pinned detach, add a 15s readiness probe to the request
                // path, and turn an always-202 endpoint into 202-or-409 for
                // existing callers.
                "status": "submitted",
                "build_id": submission_id.to_string(),
                "submission_id": submission_id.to_string(),
                "poll": "/builds",
                "action_id": action_id.to_string(),
                "states_active": states_active,
                "predicted": predicted,
                "risk": risk_json,
                "outcome_url": format!("/actions/{}/outcome", action_id),
                "message": "rebuild-restart submitted; the build+restart runs detached from \
                            this connection — poll GET /builds (or GET /build/{id}/status) \
                            for the terminal outcome",
            })),
        )
            .into_response());
    }

    // ── No-rebuild path: stays synchronous (fast restart). ──────────────────
    // `from_working_tree` only affects the build (no build here), so it's
    // forwarded for signature consistency and has no effect when rebuild=false.
    //
    // "Act on predictions": when the risk assessment flagged this restart as
    // high-risk (a boot-failure-class prediction under the active states), bias
    // the exe pick toward the last-known-good binary instead of the freshest
    // slot exe that history says white-screens — the prevention this whole
    // feature exists for. We NEVER refuse the restart (a refused restart could
    // strand the operator); we only bias, and only when the operator did NOT
    // make an explicit choice:
    //   - `use_lkg: Some(false)` ⇒ explicit opt-OUT: run the freshest slot exe
    //     even under high risk (the risk is still warned).
    //   - `use_lkg: Some(true)`  ⇒ explicit opt-IN: always pin to LKG.
    //   - `use_lkg: None`        ⇒ auto: bias to LKG iff `risk.route_lkg`.
    // The bias only applies when the LKG exe actually exists on disk; otherwise
    // it is a no-op (fail-open) and the normal slot resolution runs.
    let want_lkg = match body.use_lkg {
        Some(explicit) => explicit,
        None => risk.route_lkg,
    };
    // Track whether we set a transient override so we can clear it after the
    // restart, keeping the bias scoped to THIS restart (later restarts re-decide
    // from fresh predictions rather than inheriting a sticky pin).
    let mut biased_primary: Option<std::sync::Arc<crate::state::ManagedRunner>> = None;
    if want_lkg {
        let lkg_exe = state.config.lkg_exe_path();
        if lkg_exe.exists() {
            if let Some(primary) = state.get_primary().await {
                *primary.source_exe_override.write().await = Some(lkg_exe.clone());
                let reason = if body.use_lkg == Some(true) {
                    "operator-requested (use_lkg:true)"
                } else {
                    "high-risk prediction (auto-bias)"
                };
                let msg = format!(
                    "Restart biased to last-known-good binary {lkg_exe:?} — {reason}; \
                     pass use_lkg:false to opt out"
                );
                tracing::warn!("{msg}");
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                    .await;
                biased_primary = Some(primary);
            }
        } else if risk.route_lkg || body.use_lkg == Some(true) {
            // Wanted LKG but none exists — surface it rather than silently
            // running the freshest slot exe under known high risk.
            let msg = format!(
                "Restart wanted the last-known-good binary (risk_route_lkg={}, \
                 use_lkg={:?}) but no LKG exe exists at {:?}; proceeding with the \
                 freshest slot exe",
                risk.route_lkg,
                body.use_lkg,
                state.config.lkg_exe_path()
            );
            tracing::warn!("{msg}");
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
    }

    let restart_result = manager::restart_runner(
        &state,
        false,
        RestartSource::Manual,
        body.force,
        body.from_working_tree,
    )
    .await;

    // Clear the transient LKG override so the bias does not leak into the next
    // restart (which re-evaluates risk from fresh predictions). The runner has
    // already resolved + copied its exe by the time `restart_runner` returns, so
    // clearing now does not affect the just-started process.
    if let Some(primary) = biased_primary {
        *primary.source_exe_override.write().await = None;
    }
    // Phase 3 path 2: the SYNCHRONOUS `rebuild:false` restart. The action was
    // minted before the `if rebuild` branch, so a readiness refusal here
    // returned 409 to the caller while the minted action still closed
    // `Confirmed` 30s later — and was ingested to coord that way.
    if let Err(e) = &restart_result {
        action_record.record_run_result(run_result_for(e));
    }
    restart_result?;

    // Port-bind verification: the manager call above returns the moment the
    // OS process is spawned, but the runner may still be pre-bind (or hung).
    // Poll its `/health` for up to 30s so callers can distinguish a healthy
    // restart from a wedged one. `wait=false` opts out (legacy behavior).
    if wait_q.wait {
        // Look up the primary runner id so the probe targets the right
        // managed entry. The legacy endpoint always restarts the primary,
        // so this mirrors `manager::restart_runner`'s lookup.
        if let Some(primary) = state.get_primary().await {
            let runner_id = primary.config.id.clone();
            if let Err(failure) = wait_for_runner_healthy_default(&state, &runner_id).await {
                action_record.record_run_result(ActionRunResult::Errored {
                    reason: format!(
                        "runner_unhealthy_after_start: runner '{}' did not become healthy \
                         within {}ms of the restart",
                        runner_id, failure.elapsed_ms
                    ),
                });
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Runner '{}' did not become healthy {}ms after restart",
                            runner_id, failure.elapsed_ms
                        ),
                    )
                    .await;
                return Ok(unhealthy_after_start_response(&runner_id, failure));
            }
        }
    }

    action_record.record_run_result(ActionRunResult::Ran);

    Ok(Json(serde_json::json!({
        "status": "restarted",
        "message": "Runner restarted successfully",
        "action_id": action_id.to_string(),
        "states_active": states_active,
        "predicted": predicted,
        "risk": risk_json,
        "routed_lkg": want_lkg,
        "outcome_url": format!("/actions/{}/outcome", action_id),
    }))
    .into_response())
}

pub async fn control_watchdog(
    State(state): State<SharedState>,
    Json(body): Json<WatchdogRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Update the primary runner's managed watchdog (used by the watchdog bg task)
    let response = if let Some(primary) = state.get_primary().await {
        let mut wd = primary.watchdog.write().await;
        wd.enabled = body.enabled;

        if body.reset_attempts {
            wd.restart_attempts = 0;
            wd.disabled_reason = None;
            wd.crash_history.clear();
        }

        serde_json::json!({
            "watchdog": {
                "enabled": wd.enabled,
                "restart_attempts": wd.restart_attempts,
            }
        })
    } else {
        // Fallback to legacy state if no managed runners
        let mut wd = state.watchdog.write().await;
        wd.enabled = body.enabled;

        if body.reset_attempts {
            wd.restart_attempts = 0;
            wd.disabled_reason = None;
            wd.crash_history.clear();
        }

        serde_json::json!({
            "watchdog": {
                "enabled": wd.enabled,
                "restart_attempts": wd.restart_attempts,
            }
        })
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Watchdog {} {}",
                if body.enabled { "enabled" } else { "disabled" },
                if body.reset_attempts {
                    "(attempts reset)"
                } else {
                    ""
                }
            ),
        )
        .await;

    state.notify_health_change();

    Ok(Json(response))
}

/// POST /build/reset — force-release every build slot and clear the legacy flag.
///
/// Under the parallel build pool a "stuck build" is most likely a `slot.busy`
/// leak: the `SlotGuard` RAII type in `build_monitor.rs` now prevents this
/// going forward, but this endpoint remains as a forensic / recovery tool.
///
/// We intentionally do NOT force-release semaphore permits. `Semaphore` has
/// no public API for that, and `add_permits` would break the invariant
/// `permits.available + permits.held == pool_size`. If a permit is genuinely
/// stuck (which would require a tokio bug), the only safe recovery is a
/// supervisor restart.
pub async fn reset_build(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let now = chrono::Utc::now();
    let mut slots_cleared = Vec::with_capacity(state.build_pool.slots.len());
    let mut cleared_count = 0usize;

    for slot in &state.build_pool.slots {
        let mut busy = slot.busy.write().await;
        if let Some(prior) = busy.take() {
            cleared_count += 1;
            let elapsed_secs = (now - prior.started_at).num_seconds().max(0);
            tracing::warn!(
                "Forcibly cleared stuck build slot {}: requester_id={:?}, rebuild_kind={}, elapsed_secs={}",
                slot.id, prior.requester_id, prior.rebuild_kind, elapsed_secs
            );
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Warn,
                    format!(
                        "Build reset: forcibly cleared slot {} (requester_id={:?}, elapsed_secs={})",
                        slot.id, prior.requester_id, elapsed_secs
                    ),
                )
                .await;
            slots_cleared.push(serde_json::json!({
                "id": slot.id,
                "was_busy": true,
                "prior_requester_id": prior.requester_id,
                "rebuild_kind": prior.rebuild_kind,
                "elapsed_secs": elapsed_secs,
            }));
        } else {
            slots_cleared.push(serde_json::json!({
                "id": slot.id,
                "was_busy": false,
            }));
        }
    }

    let legacy_flag_was_set = {
        let mut build = state.build.write().await;
        let was = build.build_in_progress;
        build.build_in_progress = false;
        was
    };

    state.notify_health_change();

    let message = if cleared_count > 0 {
        format!(
            "Cleared {} stuck slot{}",
            cleared_count,
            if cleared_count == 1 { "" } else { "s" }
        )
    } else if legacy_flag_was_set {
        "No slots busy; cleared stale legacy build flag".to_string()
    } else {
        "No stuck slots; legacy flag already clear".to_string()
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Build reset: {}", message),
        )
        .await;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "slots_cleared": slots_cleared,
        "legacy_flag_was_set": legacy_flag_was_set,
        "message": message,
    })))
}

/// POST /runner/fix-and-rebuild — Rebuild the live runner tree, detached from
/// the HTTP connection.
///
/// **Why detached.** The rebuild is a full `cargo build` of the live runner
/// tree (~10-20 min). Previously this ran *inside* the request handler future,
/// so when the HTTP client disconnected (E2E timeout, `curl -m 30`, Ctrl-C)
/// axum dropped the handler future and cancelled the build mid-flight — twice
/// today this left a slot with a bare exe and no provenance sidecar / LKG
/// bookkeeping because the post-build steps never ran. The route is the
/// documented recovery path in the #65 start-gate refusal, so its success can't
/// depend on the caller's TCP patience.
///
/// **New contract.** The build is submitted to the #63 build-submissions state
/// machine ([`submit_detached`]) and runs in a background task independent of
/// this handler's future. The handler returns **202 Accepted** immediately with
/// the submission id; callers poll `GET /build/{id}/status` for the terminal
/// outcome. `run_cargo_build` runs to completion regardless of the connection,
/// so the provenance sidecar + LKG promotion + slot state are always written.
///
/// **Idempotent accept.** If a previous fix-and-rebuild submission is still
/// in-flight (non-terminal), a second request returns that same submission id
/// (202) instead of kicking off a duplicate live-tree build that would race the
/// same slots + LKG.
pub async fn fix_and_rebuild(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Idempotent accept: if a prior rebuild is still running, return it.
    {
        let inflight = state.fix_and_rebuild_inflight.read().await;
        if let Some(existing_id) = *inflight {
            if let Some(arc) = state.build_submissions.get(&existing_id).await {
                if !arc.read().await.status.is_terminal() {
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Info,
                            format!(
                                "Fix & Rebuild: rebuild {existing_id} already in flight — \
                                 returning existing submission (idempotent accept)"
                            ),
                        )
                        .await;
                    return Ok((
                        StatusCode::ACCEPTED,
                        Json(serde_json::json!({
                            "status": "accepted",
                            "build_id": existing_id.to_string(),
                            "submission_id": existing_id.to_string(),
                            "poll": format!("/build/{existing_id}/status"),
                            "deduplicated": true,
                            "message": "fix-and-rebuild already in flight; \
                                        poll GET /build/{id}/status for the terminal outcome",
                        })),
                    )
                        .into_response());
                }
            }
        }
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Fix & Rebuild: submitting detached runner rebuild",
        )
        .await;

    // Mint a `build`-kind dev-action snapshot at submission. The window is
    // build-duration + 30s; the exact build duration isn't known at submission
    // time, so the watcher uses the 30s tail (the build's own time is bounded
    // by the build-submissions state machine — Phase 3's coord ingest will
    // thread the precise duration). State eval captures the cause-side context
    // (SLOTS_EMPTY / LEGACY_EXE_FALLBACK / DIST_STALE) at submission.
    let (action_record, states_active, predicted, risk) = mint_primary_action(
        &state,
        ActionKind::Build,
        "fix_and_rebuild".to_string(),
        None,
        None,
    )
    .await;
    let action_id = action_record.action_id;
    let risk_json = risk_ack_json(&risk);

    // Submit the live-tree rebuild to the build-submissions state machine. The
    // exec future runs in a detached background task — a client disconnect can
    // no longer cancel it, so the post-build provenance/LKG steps always run.
    let exec_state = state.clone();
    // Phase 3 path 3: `fix-and-rebuild` mints an identical `ActionKind::Build`,
    // so a failing `run_cargo_build` returned 500 into the submission while the
    // action closed `Confirmed`.
    let exec_record = action_record.clone();
    let (submission_id, _arc) = crate::build_submissions::submit_detached(
        state.build_submissions.clone(),
        state.config.project_dir.clone(),
        None,
        async move {
            match crate::build_monitor::run_cargo_build(&exec_state).await {
                Ok(()) => {
                    exec_record.record_run_result(ActionRunResult::Ran);
                    (
                        StatusCode::OK.as_u16(),
                        serde_json::json!({
                            "status": "ok",
                            "message": "Runner rebuilt (restart manually to apply)"
                        }),
                    )
                }
                Err(e) => {
                    exec_record.record_run_result(ActionRunResult::Errored {
                        reason: e.to_string(),
                    });
                    (
                        StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        serde_json::json!({
                            "status": "error",
                            "error": e.to_string(),
                        }),
                    )
                }
            }
        },
    );

    // Record the new in-flight submission id for idempotent-accept dedup.
    {
        let mut inflight = state.fix_and_rebuild_inflight.write().await;
        *inflight = Some(submission_id);
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "accepted",
            "build_id": submission_id.to_string(),
            "submission_id": submission_id.to_string(),
            "poll": format!("/build/{submission_id}/status"),
            "action_id": action_id.to_string(),
            "states_active": states_active,
            "predicted": predicted,
            "risk": risk_json,
            "outcome_url": format!("/actions/{}/outcome", action_id),
            "message": "fix-and-rebuild submitted; the build runs detached from this \
                        connection — poll GET /build/{id}/status for the terminal outcome",
        })),
    )
        .into_response())
}

/// Trigger a graceful shutdown of the supervisor.
///
/// Unlike `Stop-Process -Force` (which uses `TerminateProcess` on Windows and
/// can't be caught), this endpoint routes through the same drain that
/// `Ctrl+C` uses: axum finishes in-flight requests before the process exits,
/// so callers waiting on a long `spawn-test` build see a proper response (or
/// a 503 if they hit us after the drain started) instead of an empty body.
///
/// Fire-and-return: the shutdown signal is dispatched on a `tokio::spawn`ed
/// task so the HTTP response can return in well under 100ms. Without this
/// handoff, the handler awaited the broadcast send acknowledgement before
/// responding, which routinely added ~2s to the round-trip while subscribers
/// drained their channels. The actual shutdown drain still happens — it just
/// runs after the response goes out.
pub async fn supervisor_shutdown(State(state): State<SharedState>) -> Json<serde_json::Value> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "HTTP shutdown requested — initiating graceful drain",
        )
        .await;

    // The shutdown_signal task in main.rs races ctrl_c against shutdown_tx;
    // `signal_shutdown` flips the latched flag *and* broadcasts so any
    // handler that subscribes after this point still observes shutdown
    // (broadcast channels don't replay missed messages). Fire it from a
    // spawned task so this handler can return immediately — the response
    // must complete before the drain window closes our listener.
    let state_for_signal = state.clone();
    tokio::spawn(async move {
        state_for_signal.signal_shutdown();
    });

    Json(serde_json::json!({
        "status": "shutting_down",
        "message": "Supervisor graceful shutdown initiated"
    }))
}

/// Self-restart the supervisor process with the same CLI args.
pub async fn supervisor_restart(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Supervisor self-restart requested",
        )
        .await;

    let args = state.config.cli_args.clone();
    let exe = args.first().cloned().unwrap_or_else(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "qontinui-supervisor".to_string())
    });

    let remaining_args: Vec<String> = args.into_iter().skip(1).collect();

    // NOTE: we deliberately do NOT call `stop_all_temp_runners` here — same
    // reasoning as the shutdown path in `main.rs` ("the dominant source of
    // `POST /supervisor/shutdown` latency: it iterated every temp runner with
    // a 5s graceful-stop poll plus a 5s port-free wait, easily 30+ seconds
    // wall-clock"). The `std::process::exit(0)` below closes this process's
    // handles, including the kill-on-exit JobObject's, so the kernel reaps
    // every assigned temp runner anyway.
    //
    // User-owned runners (primary, named, external) genuinely survive this
    // restart: `process::job::should_assign_to_ephemeral_job` never assigns
    // them to that job. Before 2026-07-28 they did NOT survive — this
    // endpoint is spawn-and-exit, so it reaped them exactly like the
    // `restart-supervisor.ps1` path did, while the comment here claimed the
    // opposite.

    // Spawn replacement process.
    //
    // The strip matters MORE here than at any other spawn site: this is the
    // only path that spawns another supervisor. Without it an inherited
    // `CLAUDE_CODE_CHILD_SESSION` survives every self-restart, so the remedy
    // the startup warning prints ("launch from a shell that does not carry the
    // marker") is defeated by the restart endpoint — the marker would outlive
    // the session that set it for as long as the supervisor keeps restarting
    // itself.
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&remaining_args).strip_inherited_claude_markers();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_child) => {
            // Signal the shutdown now that the replacement is confirmed
            // spawned — and BEFORE the `std::process::exit(0)` below.
            //
            // This endpoint is spawn-and-exit: it hard-exits ~600ms from here.
            // Every OTHER request in flight at that moment used to be dropped
            // mid-connection with no response written — exactly the "`HTTP=000`,
            // empty body" symptom reported against the synchronous
            // `POST /runners/spawn-test`, whose handler holds its connection
            // open for the whole cargo build. Signalling gives those handlers
            // their shutdown wake-up inside this window, so they answer
            // (spawn-test replies 503 `spawn_abandoned`, naming the runner id
            // and port it reserved) instead of being cut off.
            //
            // Deliberately NOT signalled before `cmd.spawn()`: a failed spawn
            // returns an error and leaves this supervisor running, and an
            // already-latched shutdown would then take it down anyway.
            state.signal_shutdown();

            // Give the new process a moment to start
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Send response before exiting
            let response = Json(serde_json::json!({
                "status": "restarting",
                "message": "Supervisor is restarting"
            }));

            // Schedule exit after response is sent
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });

            Ok(response)
        }
        Err(e) => Err(SupervisorError::Process(format!(
            "Failed to spawn replacement supervisor: {}",
            e
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_submissions::{BuildKind, BuildStatus, BuildSubmission};
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
    use crate::state::{SharedState, SupervisorState};
    use axum::extract::State;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A port in the IANA dynamic range that nothing on a dev box or a CI
    /// runner listens on — see [`test_state_dead_runner_port`].
    const DEAD_RUNNER_PORT: u16 = 39876;

    /// Serializes the tests that drive a real [`crate::build_monitor::run_cargo_build`].
    ///
    /// Both of them close the build-pool semaphore so the build itself fails
    /// fast — but the PRE-permit guards still run, and `check_ram_guard`'s
    /// `available_commit_bytes()` is a whole-machine memory/process query.
    /// Two of those in flight at once cost ~3.3s wall-clock apiece on a loaded
    /// box, which is enough to trip
    /// `restart_rebuild_true_detaches_returns_202_and_registers_build`'s 2s
    /// latency pin — a pin about the HANDLER detaching, not about how busy the
    /// machine running the suite is. Serializing the two keeps that pin
    /// measuring what it was written to measure. Measured before/after: 1 in 4
    /// full-suite runs failed, then 0 in 8.
    static BUILDING_TESTS: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    fn test_state(root: &std::path::Path) -> SharedState {
        test_state_on_port(root, crate::config::DEFAULT_RUNNER_API_PORT)
    }

    /// A [`test_state`] whose primary points at [`DEAD_RUNNER_PORT`].
    ///
    /// Use this for any test that drives a restart to the point of running
    /// `restart_readiness::enforce` **synchronously**. Against a dead port the
    /// gate takes its `every unknown refuses` arm (`CLAUDE.md`: *"No error path
    /// resolves to 'safe'"*), which is the exact refusal these tests want —
    /// reached deterministically, and without the test ever being one gate
    /// verdict away from stopping a live runner that happens to be listening on
    /// `DEFAULT_RUNNER_API_PORT` on the machine running the suite.
    fn test_state_dead_runner_port(root: &std::path::Path) -> SharedState {
        test_state_on_port(root, DEAD_RUNNER_PORT)
    }

    fn test_state_on_port(root: &std::path::Path, runner_port: u16) -> SharedState {
        let mut primary = RunnerConfig::default_primary();
        primary.port = runner_port;
        let config = SupervisorConfig {
            project_dir: root.join("src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: 9875,
            dev_logs_dir: root.join(".dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 8081,
            runners: vec![primary],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
        };
        Arc::new(SupervisorState::new(config))
    }

    fn queued_submission(id: uuid::Uuid) -> BuildSubmission {
        BuildSubmission {
            id,
            worktree_path: PathBuf::from("/tmp/runner/src-tauri"),
            source: None,
            build_kind: BuildKind::Build,
            agent_id: None,
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: chrono::Utc::now(),
            status: BuildStatus::Running {
                started_at: chrono::Utc::now(),
            },
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
            spawn: None,
            detached: None,
        }
    }

    async fn body_json(resp: axum::response::Response) -> (u16, serde_json::Value) {
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response body must be JSON");
        (status, v)
    }

    /// A second fix-and-rebuild while one is still in flight is an idempotent
    /// accept: returns the EXISTING submission id (202, `deduplicated: true`)
    /// without kicking off a duplicate live-tree build. This path never calls
    /// `run_cargo_build`, so it's deterministic.
    #[tokio::test]
    async fn fix_and_rebuild_idempotent_accept_returns_existing_inflight() {
        let tmp = TempDir::new().expect("tempdir");
        let state = test_state(tmp.path());

        // Seed a still-running submission + point the in-flight tracker at it.
        let existing_id = uuid::Uuid::new_v4();
        state
            .build_submissions
            .insert(queued_submission(existing_id))
            .await;
        *state.fix_and_rebuild_inflight.write().await = Some(existing_id);

        let resp = fix_and_rebuild(State(state.clone()))
            .await
            .expect("handler ok")
            .into_response();
        let (status, body) = body_json(resp).await;

        assert_eq!(status, 202, "idempotent accept must be 202");
        assert_eq!(body["status"], "accepted");
        assert_eq!(body["deduplicated"], true);
        assert_eq!(
            body["build_id"],
            existing_id.to_string(),
            "must return the EXISTING submission id, not a fresh one"
        );
        // The tracker is unchanged — still the original submission.
        assert_eq!(
            *state.fix_and_rebuild_inflight.read().await,
            Some(existing_id)
        );
    }

    /// `POST /runner/restart {"rebuild": true}` must DETACH: the handler returns
    /// 202 quickly (without awaiting the ~10-20min build) and registers a build
    /// submission so progress is observable via `GET /builds`. The detached task
    /// owns its own state clone, so it survives the handler future being dropped
    /// — the whole point of the change. We close the build-pool semaphore up
    /// front so the detached `run_cargo_build` fails fast (semaphore closed)
    /// instead of attempting a real cargo build in the sandbox — that keeps the
    /// detached task deterministic and short-lived while still exercising the
    /// full submit_detached → terminal-status path.
    #[tokio::test]
    async fn restart_rebuild_true_detaches_returns_202_and_registers_build() {
        let _building = BUILDING_TESTS.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let state = test_state(tmp.path());

        // Make the detached build fail fast rather than spawn real cargo.
        state.build_pool.permits.close();

        let started = std::time::Instant::now();
        let resp = restart_runner(
            State(state.clone()),
            Query(StartWaitQuery { wait: false }),
            Json(RestartRequest {
                rebuild: true,
                force: false,
                // Legacy live-tree path so the closed-semaphore fast-fail is
                // exercised deterministically without a network fetch /
                // origin/main worktree materialization in the sandbox.
                from_working_tree: true,
                use_lkg: None,
            }),
        )
        .await
        .expect("handler ok")
        .into_response();
        let elapsed = started.elapsed();
        let (status, body) = body_json(resp).await;

        assert_eq!(status, 202, "rebuild:true must return 202 (detached)");
        // Phase 4: the ACK is emitted BEFORE the readiness gate runs, so it
        // says what is actually known — the rebuild was SUBMITTED. It used to
        // claim "rebuilding" for a restart the gate refused 1.4s later.
        assert_eq!(body["status"], "submitted");
        assert_eq!(body["poll"], "/builds");
        assert!(
            body["build_id"].is_string(),
            "must carry a submission id for /builds correlation"
        );
        // The handler must NOT block on the build — it returns near-instantly.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "handler should return fast (detached), took {:?}",
            elapsed
        );

        // submit_detached registers the submission on a spawned task; give it a
        // beat to insert, then confirm a new build is observable via the store
        // that backs GET /builds / GET /build/{id}/status.
        let build_id = body["build_id"].as_str().unwrap().to_string();
        let target = uuid::Uuid::parse_str(&build_id).expect("build_id is a uuid");
        let mut arc = None;
        for _ in 0..100 {
            if let Some(a) = state.build_submissions.get(&target).await {
                arc = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let arc = arc.expect("the detached rebuild must register a build submission");

        // The detached task runs independently of this (already-returned)
        // handler. With the pool semaphore closed it fails fast and the
        // submission reaches a terminal status — proving the task ran to
        // completion on its own.
        let mut terminal = false;
        for _ in 0..100 {
            if arc.read().await.status.is_terminal() {
                terminal = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            terminal,
            "the detached task must run to a terminal status independently of the handler"
        );

        // Plan §6 Phase 5 test 5c — Phase 3 path 1, the OBSERVED case, pinned
        // here rather than in a second test that would drive this same handler
        // a second time. Whatever the detached body hits (here: a closed build
        // pool; in the incident: the readiness gate refusing 1.4s after this
        // 202) must reach the dev-action. It used to reach `/build/{id}/status`
        // and nothing else, while `/actions/{id}/outcome` closed `Confirmed`
        // 30s later — two pointers in this very response body, two different
        // answers to the same question.
        let action_id = uuid::Uuid::parse_str(body["action_id"].as_str().expect("action_id"))
            .expect("action_id is a uuid");
        let run = await_run_result(&state, action_id, std::time::Duration::from_secs(10)).await;
        assert!(
            run.is_failure(),
            "the detached body's failure must reach the action: {run:?}"
        );
        assert_ne!(
            crate::dev_action::attribution::classify(Some(&run), &[]),
            crate::dev_action::record::D3Category::Confirmed,
            "a clean window behind a failed detached rebuild must not classify Confirmed"
        );
    }

    // ── Regression tests for
    //    `2026-09-04-supervisor-refused-restart-reports-confirmed` ──────────

    /// Poll the action store for a record's terminal result.
    async fn await_run_result(
        state: &SharedState,
        action_id: uuid::Uuid,
        budget: std::time::Duration,
    ) -> crate::dev_action::ActionRunResult {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(rec) = state.dev_actions.read().await.get(&action_id) {
                if let Some(r) = rec.run_result() {
                    return r;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no terminal result was ever recorded for action {action_id}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
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
            crate::dev_action::ActionRunResult::Refused { reason } => {
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

        let errored = run_result_for(&SupervisorError::RunnerNotRunning);
        assert!(matches!(
            errored,
            crate::dev_action::ActionRunResult::Errored { .. }
        ));
        assert_eq!(
            crate::dev_action::attribution::classify(Some(&errored), &[]),
            crate::dev_action::record::D3Category::Failure
        );
    }

    /// Plan §6 Phase 5 test 5a — the SYNCHRONOUS `rebuild:false` path. The
    /// action is minted at the top of the handler, *before* the `if rebuild`
    /// branch, so a failure here (a readiness refusal returns 409) used to
    /// leave the minted action closing `Confirmed` 30s later — and being
    /// ingested to coord that way. It must now carry a terminal result that
    /// classifies non-`Confirmed`.
    #[tokio::test]
    async fn sync_restart_failure_records_a_non_confirmed_terminal_result() {
        let tmp = TempDir::new().expect("tempdir");
        let state = test_state_dead_runner_port(tmp.path());
        // Evidence of life on a port nothing answers is the gate's UNKNOWN
        // signature, and every UNKNOWN refuses. Without `running = true` the
        // gate's `runner_not_running` exemption short-circuits it and the
        // restart proceeds to a stop/start — which a unit test must never do.
        state
            .get_primary()
            .await
            .expect("primary")
            .runner
            .write()
            .await
            .running = true;

        // The gate therefore refuses — the same 409 `RestartUnsafe` the
        // observed incident produced, reached deterministically and without a
        // live runner anywhere in the loop.
        let result = restart_runner(
            State(state.clone()),
            Query(StartWaitQuery { wait: false }),
            Json(RestartRequest {
                rebuild: false,
                force: false,
                from_working_tree: false,
                use_lkg: None,
            }),
        )
        .await;
        let err = result.expect_err("a refused restart must not report success");
        assert!(
            matches!(err, SupervisorError::RestartUnsafe(_)),
            "expected the readiness gate's refusal, got {err:?}"
        );

        let recent = state.dev_actions.read().await.recent(10);
        let record = recent
            .first()
            .cloned()
            .expect("the handler mints a dev-action before the restart runs");
        let run = record
            .run_result()
            .expect("the failure must be recorded on the action");
        match &run {
            // The gate's refusal family: `restart_refused_unsafe` when the
            // runner itself reports live work, `restart_refused_unknown` when
            // no verdict could be obtained (this test's arm — and the arm the
            // gate's "every unknown refuses" rule exists for). Both are
            // `RestartUnsafe`; the verbatim carry of the `_unsafe` spelling is
            // pinned by `run_result_for_maps_the_readiness_refusal_to_refused`.
            crate::dev_action::ActionRunResult::Refused { reason } => assert!(
                reason.starts_with("restart_refused_"),
                "the gate's refusal must be carried verbatim; got {reason}"
            ),
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_ne!(
            crate::dev_action::attribution::classify(Some(&run), &[]),
            crate::dev_action::record::D3Category::Confirmed,
            "a clean window behind a failed restart must not classify Confirmed"
        );
    }

    /// Plan §6 Phase 5 test 5b — `POST /runner/fix-and-rebuild`. An identical
    /// `ActionKind::Build`; a failing `run_cargo_build` returned 500 into the
    /// submission while the action went `Confirmed`. Closing the build-pool
    /// semaphore makes the detached build fail fast and deterministically,
    /// exactly as the PR #64 pin does.
    #[tokio::test]
    async fn fix_and_rebuild_failure_records_a_non_confirmed_terminal_result() {
        let _building = BUILDING_TESTS.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let state = test_state(tmp.path());
        state.build_pool.permits.close();

        let resp = fix_and_rebuild(State(state.clone()))
            .await
            .expect("handler ok")
            .into_response();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, 202);
        let action_id = uuid::Uuid::parse_str(body["action_id"].as_str().expect("action_id"))
            .expect("action_id is a uuid");

        let run = await_run_result(&state, action_id, std::time::Duration::from_secs(10)).await;
        assert!(
            run.is_failure(),
            "a failed fix-and-rebuild must not leave the action's result as Ran: {run:?}"
        );
        assert_ne!(
            crate::dev_action::attribution::classify(Some(&run), &[]),
            crate::dev_action::record::D3Category::Confirmed
        );
    }

    /// Source guard: `supervisor_restart` must latch the shutdown before it
    /// hard-exits, and only once the replacement is confirmed spawned.
    ///
    /// This endpoint is spawn-and-exit — it calls `std::process::exit(0)` ~600ms
    /// after accepting the request. Every OTHER request in flight at that moment
    /// is dropped mid-connection with no response written, which is exactly the
    /// reported "HTTP=000, empty body" symptom against the synchronous
    /// `POST /runners/spawn-test` (whose handler holds its connection open for
    /// the whole cargo build). Latching the shutdown first wakes those handlers
    /// inside the window so they answer instead of being cut off.
    ///
    /// The ordering half matters just as much: latching BEFORE `cmd.spawn()`
    /// would take this supervisor down even when the replacement failed to
    /// start, turning a failed restart into an outage.
    #[test]
    fn supervisor_restart_latches_the_shutdown_before_it_exits() {
        let this_file = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("routes")
                .join("runner.rs"),
        )
        .expect("this source file must be readable")
        .replace("\r\n", "\n");

        let body = this_file
            .split_once("pub async fn supervisor_restart(")
            .map(|(_, after)| after)
            .expect("supervisor_restart must exist — did it get renamed?");
        let body = body
            .find("\n#[cfg(test)]")
            .map(|end| &body[..end])
            .unwrap_or(body);

        let latch = body.find("state.signal_shutdown()").unwrap_or_else(|| {
            panic!(
                "supervisor_restart does not latch the shutdown before exiting. Without it, every \
                 in-flight request — notably a synchronous spawn-test mid-build — is cut off with \
                 no response at all (curl reports HTTP=000) and its caller never learns the \
                 runner id it created. Body scanned:\n{body}"
            )
        });
        let spawn = body
            .find("cmd.spawn()")
            .expect("supervisor_restart must spawn the replacement");
        // `rfind`: the doc comment above the spawn also names
        // `std::process::exit(0)` when explaining the JobObject teardown, and
        // that mention sits BEFORE the latch. The last occurrence is the call.
        let exit = body
            .rfind("std::process::exit(0)")
            .expect("supervisor_restart must still hard-exit after the handoff");

        assert!(
            spawn < latch,
            "the shutdown must be latched only AFTER the replacement is confirmed spawned — \
             latching first turns a failed replacement spawn into a supervisor outage"
        );
        assert!(
            latch < exit,
            "the shutdown must be latched BEFORE the hard exit, or in-flight handlers get no \
             window in which to answer"
        );
    }
}

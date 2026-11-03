//! The one mint every supervisor lifecycle door goes through.
//!
//! A door that starts, restarts or rebuilds a runner mints a dev-action trial
//! here before it acts (a plain stop is not a trial: no [`ActionKind`] measures
//! one): the active dev-state set is evaluated, coord's
//! state-conditioned expectations are fetched and assessed, the record is
//! stored, and the attribution watcher is spawned against the runner the door
//! acts on. The door then records the act's terminal result on the returned
//! record and threads its `action_id` into the start, so the spawn's ring entry
//! names the action (`SpawnMarker::initiated_by`).
//!
//! Plan `2026-11-03-supervisor-dev-action-residuals-coord-resolvers-restart-door-and-pre-fix-ledger-rows`
//! D2: before it, only the primary and spawn routes minted, each through its own
//! copy of this body, so a restart through any other door was never a trial.

use std::sync::Arc;
use std::time::Duration;

use crate::coord_client::coord_base;
use crate::dev_action::{
    assess_then_cut, evaluate_all, fetch_expectations, spawn_attribution_watcher, ActionKind,
    ActionRecord, ActionRisk, AttributionTargets, PredictedSignature, SlotResolution,
};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::state::{ManagedRunner, SharedState};

/// What a mint hands back to its door.
pub struct MintedAction {
    /// The stored record. The door records the act's terminal result on it.
    pub record: Arc<ActionRecord>,
    /// The canonical ids of the dev states active at the mint, for the ACK.
    pub states_active: Vec<&'static str>,
    /// Coord's expectations for `(kind, states_active)`, cut to the ACK's five.
    pub predicted: Vec<PredictedSignature>,
    /// The risk verdict over every expectation coord served, before the cut.
    pub risk: ActionRisk,
}

impl MintedAction {
    /// The fields a lifecycle door's ACK carries about its action. A response with
    /// no `action_id` was not stamped and carries none of them.
    pub fn ack_fields(&self) -> serde_json::Value {
        let action_id = self.record.action_id;
        serde_json::json!({
            "action_id": action_id.to_string(),
            "states_active": self.states_active,
            "predicted": self.predicted,
            "expectations_basis": self.record.expectations_basis,
            "risk": risk_ack_json(&self.risk),
            "outcome_url": format!("/actions/{action_id}/outcome"),
        })
    }

    /// `body` with this action's [`Self::ack_fields`] merged in. A body that is
    /// not an object is returned unchanged.
    pub fn stamp(&self, mut body: serde_json::Value) -> serde_json::Value {
        if let (Some(body), serde_json::Value::Object(fields)) =
            (body.as_object_mut(), self.ack_fields())
        {
            body.extend(fields);
        }
        body
    }
}

impl AttributionTargets {
    /// The targets of an action that drives `managed`'s boot: its registry
    /// entry, whose spawn ring the start writes, and its id for the health scan.
    pub fn for_runner(managed: Arc<ManagedRunner>) -> Self {
        Self {
            runner_id: Some(managed.config.id.clone()),
            managed: Some(managed),
        }
    }
}

/// A trial evaluated but not yet stored: [`prepare_action`]'s result, which
/// [`commit_action`] stores. Dropping one leaves no ledger row and spawns no
/// watcher, which is what a door whose act is refused before it starts wants.
pub struct PreparedAction {
    record: ActionRecord,
    states_active: Vec<&'static str>,
    predicted: Vec<PredictedSignature>,
    risk: ActionRisk,
}

/// Mint a dev-action trial of `kind` and spawn its attribution watcher over
/// `targets`: [`prepare_action`] then [`commit_action`], for a door with no
/// admission step between them.
pub async fn mint_action(
    state: &SharedState,
    kind: ActionKind,
    params_digest: String,
    requester_id: Option<String>,
    targets: AttributionTargets,
    backstop_override: Option<Duration>,
) -> MintedAction {
    let prepared = prepare_action(state, kind, params_digest, requester_id).await;
    commit_action(state, prepared, targets, backstop_override).await
}

/// Evaluate a dev-action trial of `kind` without storing it.
///
/// State evaluation includes `LEGACY_EXE_FALLBACK` through a cheap
/// `resolve_source_exe_with_origin` probe of the slot a start WOULD deploy; a
/// resolution error leaves it unevaluated, so the state surfaces as `Unknown`
/// rather than a false `False`. Resolution reads facts already in memory, so
/// this stays off the blocking path.
///
/// `predicted` is coord's state-conditioned expectations for
/// `(kind, states_active)`, fetched best-effort with a 2s timeout so the
/// initiator is warned BEFORE the act completes; empty when coord is
/// unreachable or has no rows (fail-open, never blocks the act), and the
/// record's `expectations_basis` separates those two. The fetch
/// reads coord's top 100 rows, because coord ranks by its pooled mean before
/// cutting and could otherwise drop the very row the exact-cell arm of the risk
/// policy reads; `risk` assesses all of them, then `predicted` is cut to five
/// ([`assess_then_cut`]).
///
/// The slow half of a mint lives here so a door can run it BEFORE an admission
/// step, so the step-to-act window grows only by [`commit_action`]'s in-memory insert
/// (`manager::serving_watchdog_restart`).
pub async fn prepare_action(
    state: &SharedState,
    kind: ActionKind,
    params_digest: String,
    requester_id: Option<String>,
) -> PreparedAction {
    let slot_resolution = match manager::resolve_source_exe_with_origin(state).await {
        Ok((origin, _)) => SlotResolution::Resolved(origin),
        Err(_) => SlotResolution::NotEvaluated,
    };
    let states = evaluate_all(state, slot_resolution).await;
    let mut record = ActionRecord::new(kind, requester_id, params_digest, &states);
    // The record stores typed `DevState`s; the ACK's wire form is their
    // canonical-id array.
    let states_active: Vec<&'static str> =
        record.states_active.iter().map(|s| s.as_str()).collect();

    let coord = coord_base(&state.config.ambient_coord);
    let fetched = fetch_expectations(coord.as_deref(), kind.as_str(), &states_active).await;
    record.expectations_basis = fetched.basis;
    let (predicted, risk) = assess_then_cut(fetched.rows, &states_active);

    PreparedAction {
        record,
        states_active,
        predicted,
        risk,
    }
}

/// Store a prepared trial and spawn its attribution watcher over `targets`.
///
/// The record's `started_at` and `started_instant` are re-stamped here.
///
/// A high risk is warned here, loudly, whatever the door does with the advice:
/// here rather than in [`prepare_action`], so an act refused before it is
/// committed warns about nothing it did.
///
/// `backstop_override` replaces the kind's backstop on the watcher's window,
/// for an act that can outlast it.
pub async fn commit_action(
    state: &SharedState,
    prepared: PreparedAction,
    targets: AttributionTargets,
    backstop_override: Option<Duration>,
) -> MintedAction {
    let PreparedAction {
        mut record,
        states_active,
        predicted,
        risk,
    } = prepared;
    // The trial starts when it is committed, not when it was evaluated: a door
    // that prepares before an admission step (the serving watchdog's readiness
    // gate) must not date its act before that step.
    record.started_at = chrono::Utc::now();
    // The watcher's window is measured from this instant, so it moves with the
    // wall-clock stamp: a window started at preparation would close early.
    record.started_instant = tokio::time::Instant::now();
    if let Some(warn) = risk.warn.as_deref() {
        tracing::warn!(action_id = %record.action_id, kind = record.kind.as_str(), "{warn}");
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, warn.to_string())
            .await;
    }

    let record = state.dev_actions.write().await.insert(record);
    spawn_attribution_watcher(state.clone(), record.clone(), targets, backstop_override);

    MintedAction {
        record,
        states_active,
        predicted,
        risk,
    }
}

/// Render an [`ActionRisk`] as an ACK's `risk` field: `null` when there is no
/// actionable risk (the healthy / no-history case), otherwise the `route_lkg`
/// advice, the human-readable `warning`, and the `top_signature` evidence, so
/// the initiator sees exactly what tripped the assessment.
pub fn risk_ack_json(risk: &ActionRisk) -> serde_json::Value {
    if !risk.is_actionable() {
        return serde_json::Value::Null;
    }
    serde_json::json!({
        "route_lkg": risk.route_lkg,
        "warning": risk.warn,
        "top_signature": risk.top_signature,
    })
}

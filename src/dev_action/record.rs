//! Action Snapshot records + the in-memory capped store.
//!
//! This is the Phase-1 (supervisor-local) data model for the dev-event
//! cause-effect ledger described in
//! `plans/2026-06-07-twin-dev-event-cause-effect-ledger.md` §5.2 / §6.1. It
//! mirrors the paper's *Action Snapshot* `AS = (o_a^h, S_Ξ^h, r_a^h)`
//! (Spinak 2025 §11.1): an action's parameters, **the active dev-state set at
//! execution time**, and its result.
//!
//! Phase 1 keeps everything in process memory — no coord persistence yet
//! (that is Phase 3). The store is a capped ring keyed by `action_id`,
//! intentionally mirroring [`crate::diagnostics::DiagnosticsState`]'s
//! `VecDeque` ring so the two cause/effect surfaces age out the same way.
//!
//! # Why a ring + a side index
//!
//! The watcher ([`crate::dev_action::attribution`]) writes a record's outcome
//! back *after* the per-kind attribution window elapses — seconds (restart) to
//! minutes (build) after the record is minted. The readback route
//! (`GET /actions/{id}/outcome`) must find the record by id during that whole
//! window. A bare `VecDeque` would force an O(n) scan per readback; a
//! `HashMap<Uuid, Arc<ActionRecord>>` gives O(1) lookup, and a parallel
//! `VecDeque<Uuid>` of insertion order drives the cap eviction (oldest id
//! popped + removed from the map). Each record's mutable outcome lives behind
//! its own `RwLock` so the watcher can fold the verdict in without taking the
//! whole-store lock.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use uuid::Uuid;

/// Cap on the in-memory action-record ring. Sized to match
/// [`crate::diagnostics::DiagnosticsState`]'s 200-entry buffer — a few hundred
/// recent actions is plenty for the "what just happened to my restart" readback
/// the Phase-1 feature exists to answer, and the durable ledger (Phase 3) is
/// where long-horizon history lives.
pub const ACTION_STORE_CAP: usize = 200;

/// The kind of dev action being snapshotted. Phase 1 covers only the
/// supervisor-local kinds whose cause-facts the supervisor already holds in
/// memory and whose outcomes its early-log / health / panic surfaces already
/// observe (Q9 scope guard): `restart`, `spawn`, `build`. `deploy` / `migrate`
/// join in Phase 3 on the coord side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// A primary-runner restart (`POST /runner/restart`).
    Restart,
    /// A temp/named runner spawn (`POST /runners/spawn-test` / `spawn-named`).
    Spawn,
    /// A live-tree rebuild (`POST /runner/fix-and-rebuild`).
    Build,
}

impl ActionKind {
    /// Stable string id used in logs + the JSON `kind` field. Matches the
    /// `#[serde(rename_all = "snake_case")]` wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::Restart => "restart",
            ActionKind::Spawn => "spawn",
            ActionKind::Build => "build",
        }
    }
}

/// The D3 outcome category for an action — the five `outcome_category` values
/// the coord calibration flywheel already keys on
/// (`OutcomeCounts{confirmed,surprise,failure,contradiction,partial}`). The
/// `snake_case` serde rename guarantees this serializes to exactly those five
/// strings, which is what makes the Phase-4 calibration key compatible without
/// a translation layer (guarded by a serde round-trip test below).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D3Category {
    /// The action did exactly what the ACK claimed (clean window).
    Confirmed,
    /// An unexpected-but-not-failing signal appeared (reserved; Phase 1
    /// classifies into the other four).
    Surprise,
    /// The action's own machinery failed (panic on startup, port bind fail).
    Failure,
    /// The ACK claimed success but observation refutes it — the motivating
    /// 2026-06-07 case (a "restarted successfully" ACK over an asset-missing
    /// white screen).
    Contradiction,
    /// A partial / mixed outcome (reserved for Phase 1).
    Partial,
}

/// The truth value of a single dev-state predicate evaluation. A state that
/// *could not* be evaluated is [`Eval::Unknown`] — never silently absent (D4
/// blind-spot honesty, §5.1 / success criterion 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Eval {
    True,
    False,
    Unknown,
}

/// One dev-state predicate evaluation: the state's stable id + its truth value
/// at action time. State ids are `&'static str` because the Phase-1 seed set is
/// hardcoded in [`crate::dev_action::states`] (the shared `qontinui-types`
/// registry is Phase 2).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DevStateEval {
    pub id: &'static str,
    pub value: Eval,
}

/// The recorded outcome of an action, folded in by the attribution watcher
/// after the per-kind window elapses. `None` on an [`ActionRecord`] means the
/// window is still open (or the watcher has not run yet).
#[derive(Debug, Clone, Serialize)]
pub struct ActionOutcome {
    /// The D3 verdict — closed at window end; never re-opened by late effects.
    pub category: D3Category,
    /// `DEV-*` signatures observed within the attribution window.
    pub signatures: Vec<String>,
    /// When the window closed and this verdict was recorded.
    pub ended_at: DateTime<Utc>,
    /// Window duration in milliseconds (action start → window close).
    pub duration_ms: i64,
    /// Optional short evidence excerpt / reference (e.g. the offending log
    /// line). Kept compact so it can ride a JSON response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    /// `DEV-*` signatures that arrived *after* the window closed. These update
    /// statistics (Phase 4) but never re-open `category` (§3 theory item 2).
    pub late_signatures: Vec<String>,
}

/// An Action Snapshot: an action, the active dev-state set at execution time,
/// and (eventually) its result. The mutable `outcome` lives behind its own
/// `RwLock` so the detached watcher can fold a verdict in without contending on
/// the store-level lock.
#[derive(Debug, Serialize)]
pub struct ActionRecord {
    pub action_id: Uuid,
    pub kind: ActionKind,
    /// Caller identity (the existing `requester_id` already threaded through
    /// the spawn routes; `None` for callers that didn't supply one). Q8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_id: Option<String>,
    /// A short digest of the action's params (e.g. `rebuild=true`). Compact so
    /// the snapshot stays small; the durable ledger (Phase 3) carries the full
    /// param set.
    pub params_digest: String,
    /// Ids of the dev-states evaluated `True` at action time.
    pub states_active: Vec<&'static str>,
    /// Ids of the dev-states that could not be evaluated (`Unknown`). Recorded
    /// explicitly so a blind spot is never mistaken for a `False`.
    pub states_unknown: Vec<&'static str>,
    pub started_at: DateTime<Utc>,
    /// The folded outcome, or `None` while the attribution window is still
    /// open. Serializes inline as `outcome: null | {..}` via the custom
    /// serializer below (a bare `RwLock` is not `Serialize`).
    #[serde(serialize_with = "serialize_outcome_lock")]
    pub outcome: RwLock<Option<ActionOutcome>>,
}

/// Serialize the `RwLock<Option<ActionOutcome>>` field by taking a read of the
/// lock. This is a `std::sync::RwLock` (not tokio's): the critical sections are
/// tiny, fully synchronous, and never held across an `.await`, so a blocking
/// read is correct even inside an async route handler (a tokio async lock would
/// panic here, since serialization runs on a runtime worker thread). On the
/// vanishingly-rare poisoned-lock case we fall back to serializing `null`
/// rather than panicking the serializer.
fn serialize_outcome_lock<S>(
    lock: &RwLock<Option<ActionOutcome>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match lock.read() {
        Ok(guard) => guard.serialize(serializer),
        Err(_) => None::<ActionOutcome>.serialize(serializer),
    }
}

impl ActionRecord {
    /// Mint a fresh record at action time. `states` is the full evaluated set;
    /// it is split into the active (`True`) and unknown id lists here so the
    /// stored shape matches the paper's "ids only" Action Snapshot. `False`
    /// states are intentionally dropped from the stored record (a state that is
    /// not active and not unknown carries no signal worth persisting), but the
    /// `Unknown` ids are kept so a blind spot is auditable.
    pub fn new(
        kind: ActionKind,
        requester_id: Option<String>,
        params_digest: String,
        states: &[DevStateEval],
    ) -> Self {
        let states_active = states
            .iter()
            .filter(|s| s.value == Eval::True)
            .map(|s| s.id)
            .collect();
        let states_unknown = states
            .iter()
            .filter(|s| s.value == Eval::Unknown)
            .map(|s| s.id)
            .collect();
        Self {
            action_id: Uuid::new_v4(),
            kind,
            requester_id,
            params_digest,
            states_active,
            states_unknown,
            started_at: Utc::now(),
            outcome: RwLock::new(None),
        }
    }
}

/// In-memory capped store of action records, held on `SupervisorState`.
///
/// Mirrors [`crate::diagnostics::DiagnosticsState`]'s ring discipline: a cap of
/// [`ACTION_STORE_CAP`], oldest-evicted-first. The `HashMap` gives the readback
/// route O(1) lookup by id; the `order` `VecDeque` records insertion order so
/// eviction pops the oldest id and removes it from the map in lockstep.
pub struct ActionStore {
    records: HashMap<Uuid, Arc<ActionRecord>>,
    order: VecDeque<Uuid>,
}

impl ActionStore {
    pub fn new() -> Self {
        Self {
            records: HashMap::with_capacity(ACTION_STORE_CAP),
            order: VecDeque::with_capacity(ACTION_STORE_CAP),
        }
    }

    /// Insert a record, evicting the oldest if the cap is reached. Returns the
    /// shared `Arc` so the caller can hand it to the detached watcher without a
    /// second lookup.
    pub fn insert(&mut self, record: ActionRecord) -> Arc<ActionRecord> {
        let arc = Arc::new(record);
        if self.order.len() >= ACTION_STORE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.records.remove(&oldest);
            }
        }
        self.order.push_back(arc.action_id);
        self.records.insert(arc.action_id, arc.clone());
        arc
    }

    /// Look up a record by id. Returns the shared `Arc` (cheap clone).
    pub fn get(&self, id: &Uuid) -> Option<Arc<ActionRecord>> {
        self.records.get(id).cloned()
    }

    /// Most-recent-first list of records, capped at `limit`. Used by the cheap
    /// `GET /actions` list route.
    pub fn recent(&self, limit: usize) -> Vec<Arc<ActionRecord>> {
        self.order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| self.records.get(id).cloned())
            .collect()
    }
}

impl Default for ActionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `D3Category` must serialize to EXACTLY the five `outcome_category`
    /// snake_case strings the coord calibration flywheel keys on
    /// (`OutcomeCounts{confirmed,surprise,failure,contradiction,partial}`).
    /// This guards the Phase-4 calibration-key compatibility: if a rename ever
    /// drifts, this test breaks before the ledger does.
    #[test]
    fn d3_category_serializes_to_five_outcome_category_strings() {
        let cases = [
            (D3Category::Confirmed, "confirmed"),
            (D3Category::Surprise, "surprise"),
            (D3Category::Failure, "failure"),
            (D3Category::Contradiction, "contradiction"),
            (D3Category::Partial, "partial"),
        ];
        for (cat, expected) in cases {
            let json = serde_json::to_string(&cat).expect("serialize");
            assert_eq!(json, format!("\"{expected}\""), "category {cat:?}");
        }
    }

    /// And it must round-trip back from those exact strings.
    #[test]
    fn d3_category_deserializes_from_five_outcome_category_strings() {
        let cases = [
            ("\"confirmed\"", D3Category::Confirmed),
            ("\"surprise\"", D3Category::Surprise),
            ("\"failure\"", D3Category::Failure),
            ("\"contradiction\"", D3Category::Contradiction),
            ("\"partial\"", D3Category::Partial),
        ];
        for (json, expected) in cases {
            let parsed: D3Category = serde_json::from_str(json).expect("deserialize");
            assert_eq!(parsed, expected, "json {json}");
        }
    }

    #[test]
    fn action_kind_as_str_matches_serde_wire_form() {
        for kind in [ActionKind::Restart, ActionKind::Spawn, ActionKind::Build] {
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
    }

    #[test]
    fn record_new_splits_active_and_unknown_drops_false() {
        let states = [
            DevStateEval {
                id: "SLOTS_EMPTY",
                value: Eval::True,
            },
            DevStateEval {
                id: "LKG_STALE",
                value: Eval::False,
            },
            DevStateEval {
                id: "DIST_STALE",
                value: Eval::Unknown,
            },
        ];
        let rec = ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &states);
        assert_eq!(rec.states_active, vec!["SLOTS_EMPTY"]);
        assert_eq!(rec.states_unknown, vec!["DIST_STALE"]);
        // The `False` state carries no signal and is dropped from the snapshot.
        assert!(!rec.states_active.contains(&"LKG_STALE"));
        assert!(!rec.states_unknown.contains(&"LKG_STALE"));
    }

    #[test]
    fn store_caps_and_evicts_oldest_first() {
        let mut store = ActionStore::new();
        let mut first_id = None;
        for i in 0..(ACTION_STORE_CAP + 5) {
            let rec = ActionRecord::new(ActionKind::Spawn, None, format!("n={i}"), &[]);
            let arc = store.insert(rec);
            if i == 0 {
                first_id = Some(arc.action_id);
            }
        }
        // Cap is respected.
        assert_eq!(store.recent(usize::MAX).len(), ACTION_STORE_CAP);
        // The very first record was evicted.
        assert!(store.get(&first_id.unwrap()).is_none());
    }

    #[test]
    fn store_get_and_recent_roundtrip() {
        let mut store = ActionStore::new();
        let rec = ActionRecord::new(ActionKind::Build, Some("agent-7".into()), "x".into(), &[]);
        let id = rec.action_id;
        store.insert(rec);
        let fetched = store.get(&id).expect("present");
        assert_eq!(fetched.requester_id.as_deref(), Some("agent-7"));
        let recent = store.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].action_id, id);
    }

    #[test]
    fn record_serializes_with_null_outcome_then_with_outcome() {
        let rec = ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &[]);
        // Serialize while the window is still open (outcome None).
        let json = serde_json::to_value(&rec).expect("serialize open");
        assert!(json["outcome"].is_null());
        assert_eq!(json["kind"], "restart");

        // Fold in an outcome and re-serialize.
        *rec.outcome.write().unwrap() = Some(ActionOutcome {
            category: D3Category::Contradiction,
            signatures: vec!["DEV-TAURI-ASSET-MISSING".into()],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: Some("asset not found: index.html".into()),
            late_signatures: vec![],
        });
        let json = serde_json::to_value(&rec).expect("serialize folded");
        assert_eq!(json["outcome"]["category"], "contradiction");
        assert_eq!(json["outcome"]["signatures"][0], "DEV-TAURI-ASSET-MISSING");
    }
}

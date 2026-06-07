//! Dev-event Action Snapshots — Phase 1 (supervisor-local).
//!
//! Implements the supervisor half of the dev-event cause-effect ledger from
//! `plans/2026-06-07-twin-dev-event-cause-effect-ledger.md` §6.1: every
//! supervisor-local dev action (restart / spawn / build) is recorded as an
//! *Action Snapshot* — the action's params, the **active dev-state set at
//! execution time** (ids only, the paper's `S_Ξ^h`), and (after a per-kind
//! attribution window) its outcome with a D3 verdict.
//!
//! Phase 1 is self-contained in the supervisor with zero cross-repo deps:
//! seed dev-state predicates are hardcoded ([`states`]); records live in an
//! in-memory capped ring on `SupervisorState` ([`record::ActionStore`]); the
//! attribution watcher ([`attribution`]) is a detached task that folds the
//! verdict in after the window. Coord persistence, the shared `qontinui-types`
//! registry, and the `events.dev_actions` WS push are Phases 2–3.
//!
//! Readback is at `GET /actions/{id}/outcome` (+ a cheap `GET /actions` list),
//! handled in [`crate::routes::dev_action`].

pub mod attribution;
pub mod expectations;
pub mod ingest;
pub mod record;
pub mod states;

pub use attribution::{spawn_attribution_watcher, AttributionTargets};
pub use expectations::{fetch_expectations, PredictedSignature};
pub use ingest::post_snapshot_to_coord;
pub use record::{ActionKind, ActionRecord, ActionStore};
pub use states::{evaluate_all, SlotResolution};

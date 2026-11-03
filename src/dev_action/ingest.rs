//! Phase-3 coord persistence for dev-action snapshots.
//!
//! Phase 1 keeps Action Snapshots in an in-memory ring on `SupervisorState`;
//! Phase 3 makes them durable by POSTing each *completed* snapshot (after the
//! attribution watcher folds its verdict) to qontinui-coord's
//! `POST /coord/dev-actions/snapshot` ingest endpoint.
//!
//! This is **best-effort and fail-open**: the attribution watcher calls
//! [`post_snapshot_to_coord`] after writing `record.outcome`, and a network
//! failure / non-2xx only logs a single `warn!` line; with no coord base the
//! watcher does not call it at all. It NEVER blocks or fails the watcher (matching `fleet.rs::publish_budget`'s
//! visibility-only posture).
//!
//! # Wire contract (authoritative — NOT `ActionRecord`'s own Serialize shape)
//!
//! `ActionRecord` serializes with a nested `outcome` object and names its
//! active-states field `states_active`. The coord ingest contract is *flat* and
//! names that field `state_ids`. So we build a dedicated [`SnapshotIngestBody`]
//! rather than reusing `ActionRecord`'s derive — the two shapes intentionally
//! differ and must not be coupled.
//!
//! # Resolution reuse
//!
//! - **coord base**: [`crate::coord_client::coord_base`], the one resolver,
//!   answering from [`crate::config::AmbientCoord`].
//! - **device_id**: read from `~/.qontinui/machine.json`'s `device_id`
//!   (canonical), falling back to legacy `machine_id`.
//! - **tenant_id**: best-effort from `~/.qontinui/machine.json`'s
//!   `active_tenant_id` if present; `None` otherwise (no new config system).

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::dev_action::record::{ActionOutcome, ActionRecord};

/// Timeout for the outbound coord ingest POST, matching
/// `fleet.rs::publish_budget`'s 5s budget.
const INGEST_TIMEOUT_SECS: u64 = 5;

/// The attribution semantics every snapshot this build posts was recorded
/// under, stamped as `metadata.attribution_epoch`.
///
/// **Bump rule: bump this whenever attribution semantics change** — what a
/// spawn, restart or build verdict is attributed to, or the window it is read
/// over — so coord can tell rows recorded under the old semantics from rows
/// recorded under the new. As of coord `6846970ac` (2026-11-03) coord stores the
/// stamp and does not act on it: its half of the plan below (Phase 5) is to skip the
/// `dev_action_stats` increment for
/// `spawn` / `restart` / `build` rows whose epoch is missing or below a
/// `MIN_SUPERVISOR_ATTRIBUTION_EPOCH`, so an old supervisor on any box stops
/// feeding the LKG restart guard's statistics without a date cut-off.
///
/// Epoch 1 is every row recorded before the stamp existed (no key at all):
/// `spawn` rows that closed `Confirmed` at ~30 000 ms regardless of outcome
/// (before #263), and primary restart / build rows that carried the previous
/// boot's early-log signatures (before #257). Epoch 2 is the first build that
/// carries the stamp, which descends from #257, #263 and #264. Plan
/// `2026-11-03-supervisor-dev-action-residuals-coord-resolvers-restart-door-and-pre-fix-ledger-rows`
/// D3.
///
/// Epoch 3 follows #272, which attributes `recent_crash` only to the spawn it can
/// be tied to, reads an undated `derived_status: Errored` as `Inconclusive` unless
/// the API answered after the spawn, and measures the window's tail cap on the
/// tokio clock. #272 landed still stamping 2, so a row stamped 2 can carry either
/// semantics; a row stamped 3 carries #272's.
pub const ATTRIBUTION_EPOCH: u32 = 3;

/// The snapshot `metadata` this build stamps on every row, so a row names the
/// code that attributed it: `supervisor_build_sha` and `supervisor_build_dirty`
/// (from [`crate::self_provenance`], the same values `/health` serves; each is
/// JSON `null` when the build could not establish it, which is UNKNOWN, never
/// a sha or "clean"), and its [`ATTRIBUTION_EPOCH`].
fn snapshot_metadata() -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "supervisor_build_sha".to_string(),
        serde_json::Value::from(crate::self_provenance::built_from_sha()),
    );
    metadata.insert(
        "supervisor_build_dirty".to_string(),
        serde_json::Value::from(crate::self_provenance::built_from_dirty()),
    );
    metadata.insert(
        "attribution_epoch".to_string(),
        serde_json::Value::from(ATTRIBUTION_EPOCH),
    );
    metadata
}

// ---------------------------------------------------------------------------
// machine.json — device_id + tenant_id
// ---------------------------------------------------------------------------

/// Minimal subset of `~/.qontinui/machine.json` we need. The canonical
/// post-unified-devices field is `device_id`; `machine_id` is accepted as a
/// legacy fallback (older hosts / `fleet.rs`'s historical field name).
/// `active_tenant_id` is optional and only present on hosts that pin a tenant.
#[derive(Debug, Clone, Deserialize)]
struct MachineFile {
    device_id: Option<String>,
    machine_id: Option<String>,
    active_tenant_id: Option<String>,
}

fn machine_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("machine.json"))
}

fn load_machine_file() -> Option<MachineFile> {
    let bytes = std::fs::read(machine_file_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve this supervisor's device id from `machine.json`'s `device_id`
/// (canonical post-unified-devices field), falling back to the legacy
/// `machine_id`. `None` if the file is missing / unparseable / neither field
/// is a UUID. (The live `machine.json` carries `device_id`, not `machine_id`,
/// so reading only `machine_id` silently yielded a null device on every
/// snapshot — fixed 2026-06-08.)
pub fn resolve_device_id() -> Option<Uuid> {
    let machine = load_machine_file()?;
    let raw = machine.device_id.or(machine.machine_id)?;
    Uuid::parse_str(&raw).ok()
}

/// Best-effort tenant id from `machine.json`'s `active_tenant_id`. `None` when
/// absent or not a UUID — the wire contract sends `tenant_id: null` then.
pub fn resolve_tenant_id() -> Option<Uuid> {
    let machine = load_machine_file()?;
    let raw = machine.active_tenant_id?;
    Uuid::parse_str(&raw).ok()
}

// ---------------------------------------------------------------------------
// Wire body
// ---------------------------------------------------------------------------

/// The flat coord ingest body. Field names + serialization are the authoritative
/// wire contract; this is intentionally NOT `ActionRecord`'s own Serialize shape
/// (that nests `outcome` and names the active-states field `states_active`).
#[derive(Debug, Serialize)]
pub struct SnapshotIngestBody {
    action_id: Uuid,
    /// `"restart" | "spawn" | "build"` (from `ActionKind::as_str`).
    kind: &'static str,
    device_id: Option<Uuid>,
    requester_id: Option<String>,
    params_digest: String,
    /// Canonical SCREAMING_SNAKE dev-state ids active at action time. NOTE: named
    /// `state_ids` on the wire, NOT `states_active`.
    state_ids: Vec<String>,
    states_unknown: Vec<String>,
    /// RFC3339.
    started_at: String,
    /// RFC3339, or null while no outcome was folded (the watcher only calls this
    /// after folding, so it is always `Some` here — kept `Option` for contract
    /// fidelity).
    ended_at: Option<String>,
    /// `"confirmed" | "surprise" | "failure" | "contradiction" | "partial"` or
    /// null (snake_case via `D3Category`'s serde).
    category: Option<crate::dev_action::record::D3Category>,
    duration_ms: Option<i64>,
    evidence_ref: Option<String>,
    signatures: Vec<String>,
    late_signatures: Vec<String>,
    tenant_id: Option<Uuid>,
    metadata: serde_json::Map<String, serde_json::Value>,
}

/// `text` with each NUL replaced by U+FFFD, for the body's three free-text
/// fields (`requester_id`, `params_digest`, `evidence_ref`).
///
/// coord stores them as Postgres `text`, which cannot hold a NUL, and they can
/// carry one: a startup panic's excerpt is the runner's payload verbatim (a
/// `Display`-formatted, NUL-padded Win32 buffer, say), and the spawn routes
/// take `requester_id` from the caller's body. A coord that hands the NUL to
/// Postgres, as coord's main does before `clamp_snapshot_text`, fails the
/// INSERT with a 500, and this fail-open ingest then drops the whole trial
/// with one `warn!`, signatures included; a coord whose `clamp_snapshot_text`
/// replaces the NUL itself keeps it. Replacing the NUL here keeps the trial
/// whichever coord is listening.
///
/// A U+FFFD is 3 bytes against the NUL's 1 once decoded, and coord refuses no
/// free-text field for its length (a coord with a text bound cuts the text to
/// it). Its one length refusal is the route's body limit, a 413 counted in
/// wire bytes (256 KiB on a coord with the text bound, axum's 2 MiB default
/// before it), and on the wire the replacement is the shorter of the two:
/// serde_json writes a NUL as the 6-byte `\u0000`. So the growth cannot cost
/// the trial. A requester or git ref long enough to push the body past that
/// limit does, NUL or not.
fn without_nul(text: &str) -> String {
    text.replace('\0', "\u{FFFD}")
}

impl SnapshotIngestBody {
    /// Build the wire body from a completed record + its folded outcome.
    pub fn from_record(
        record: &ActionRecord,
        outcome: &ActionOutcome,
        device_id: Option<Uuid>,
        tenant_id: Option<Uuid>,
    ) -> Self {
        SnapshotIngestBody {
            action_id: record.action_id,
            kind: record.kind.as_str(),
            device_id,
            requester_id: record.requester_id.as_deref().map(without_nul),
            params_digest: without_nul(&record.params_digest),
            state_ids: record
                .states_active
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            states_unknown: record
                .states_unknown
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            started_at: record.started_at.to_rfc3339(),
            ended_at: Some(outcome.ended_at.to_rfc3339()),
            category: Some(outcome.category),
            duration_ms: Some(outcome.duration_ms),
            evidence_ref: outcome.evidence_ref.as_deref().map(without_nul),
            signatures: outcome.signatures.clone(),
            late_signatures: outcome.late_signatures.clone(),
            tenant_id,
            metadata: snapshot_metadata(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

/// Best-effort POST of a completed snapshot to the coord at `base`. Fail-open:
/// any failure (network error, non-2xx) logs a single `warn!` and returns. NEVER
/// panics, NEVER propagates — safe to `await` or `tokio::spawn` from the
/// attribution watcher without risk to the watcher.
pub async fn post_snapshot_to_coord(
    base: &str,
    record: &ActionRecord,
    outcome: &ActionOutcome,
    device_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
) {
    let body = SnapshotIngestBody::from_record(record, outcome, device_id, tenant_id);
    let url = format!("{base}/coord/dev-actions/snapshot");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(INGEST_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("dev_action::ingest: reqwest builder failed: {e}");
            return;
        }
    };

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                debug!(
                    "dev_action::ingest: persisted snapshot {} ({}) to coord",
                    record.action_id,
                    record.kind.as_str()
                );
            } else {
                let detail = resp.text().await.unwrap_or_default();
                warn!(
                    "dev_action::ingest: coord returned {status} for POST {url} \
                     (snapshot {}): {detail}",
                    record.action_id
                );
            }
        }
        Err(e) => {
            warn!(
                "dev_action::ingest: POST {url} failed for snapshot {} (non-fatal): {e}",
                record.action_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev_action::record::{ActionKind, D3Category, DevState, DevStateEval, Eval};
    use chrono::Utc;

    /// Every snapshot names the build that attributed it and the attribution
    /// epoch it was recorded under, so coord can keep pre-fix rows out of the
    /// stats the LKG restart guard reads.
    #[test]
    fn snapshot_metadata_carries_build_sha_and_attribution_epoch() {
        let record = ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &[]);
        let outcome = ActionOutcome {
            category: D3Category::Confirmed,
            signatures: vec![],
            ended_at: Utc::now(),
            duration_ms: 1_000,
            evidence_ref: None,
            late_signatures: vec![],
        };
        let json = serde_json::to_value(SnapshotIngestBody::from_record(
            &record, &outcome, None, None,
        ))
        .expect("serialize");

        assert_eq!(json["metadata"]["attribution_epoch"], ATTRIBUTION_EPOCH);
        // Epoch 1 is the unstamped pre-fix rows, and 2 can carry semantics from
        // before #272. A constant, so it is checked at compile time.
        const _: () = assert!(ATTRIBUTION_EPOCH >= 3);
        assert_eq!(
            json["metadata"]["supervisor_build_sha"],
            serde_json::Value::from(crate::self_provenance::built_from_sha()),
            "the sha is self_provenance's (null when unknown), never a literal `unknown`"
        );
        assert_ne!(json["metadata"]["supervisor_build_sha"], "unknown");
        assert_eq!(
            json["metadata"]["supervisor_build_dirty"],
            serde_json::Value::from(crate::self_provenance::built_from_dirty()),
        );
    }

    /// The wire body must use the coord contract's field names + values — in
    /// particular `state_ids` (NOT `states_active`), snake_case `kind`,
    /// snake_case `category`, and the signatures arrays — and must be flat (no
    /// nested `outcome`).
    #[test]
    fn ingest_body_serializes_to_coord_wire_contract() {
        let states = [
            DevStateEval::new(DevState::LkgStale, Eval::True),
            DevStateEval::new(DevState::DistStale, Eval::Unknown),
        ];
        let record = ActionRecord::new(
            ActionKind::Build,
            Some("agent-7".into()),
            "rebuild=true".into(),
            &states,
        );
        let outcome = ActionOutcome {
            category: D3Category::Contradiction,
            signatures: vec!["DEV-TAURI-ASSET-MISSING".into()],
            ended_at: Utc::now(),
            duration_ms: 42_000,
            evidence_ref: Some("asset not found: index.html".into()),
            late_signatures: vec!["DEV-UI-ERROR-BOUNDARY".into()],
        };

        let device_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let body =
            SnapshotIngestBody::from_record(&record, &outcome, Some(device_id), Some(tenant_id));
        let json = serde_json::to_value(&body).expect("serialize");

        // Active states ride the contract's `state_ids` field, NOT `states_active`.
        assert_eq!(json["state_ids"], serde_json::json!(["LKG_STALE"]));
        assert!(
            json.get("states_active").is_none(),
            "must not leak ActionRecord's `states_active` name"
        );
        assert_eq!(json["states_unknown"], serde_json::json!(["DIST_STALE"]));

        // kind is the snake_case string.
        assert_eq!(json["kind"], "build");
        // category is the snake_case D3 string, flat (NOT nested under `outcome`).
        assert_eq!(json["category"], "contradiction");
        assert!(json.get("outcome").is_none(), "body must be flat");

        // signatures + late_signatures arrays carry the canonical codes.
        assert_eq!(
            json["signatures"],
            serde_json::json!(["DEV-TAURI-ASSET-MISSING"])
        );
        assert_eq!(
            json["late_signatures"],
            serde_json::json!(["DEV-UI-ERROR-BOUNDARY"])
        );

        // Scalar carry-overs.
        assert_eq!(json["action_id"], record.action_id.to_string());
        assert_eq!(json["requester_id"], "agent-7");
        assert_eq!(json["params_digest"], "rebuild=true");
        assert_eq!(json["duration_ms"], 42_000);
        assert_eq!(json["evidence_ref"], "asset not found: index.html");
        assert_eq!(json["device_id"], device_id.to_string());
        assert_eq!(json["tenant_id"], tenant_id.to_string());
        assert_eq!(json["started_at"], record.started_at.to_rfc3339());
        assert_eq!(json["ended_at"], outcome.ended_at.to_rfc3339());
        assert!(json["metadata"].is_object());
    }

    /// `device_id` / `tenant_id` serialize as JSON null when unresolved.
    #[test]
    fn ingest_body_null_device_and_tenant() {
        let record = ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &[]);
        let outcome = ActionOutcome {
            category: D3Category::Confirmed,
            signatures: vec![],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: None,
            late_signatures: vec![],
        };
        let body = SnapshotIngestBody::from_record(&record, &outcome, None, None);
        let json = serde_json::to_value(&body).expect("serialize");
        assert!(json["device_id"].is_null());
        assert!(json["tenant_id"].is_null());
        assert!(json["requester_id"].is_null());
        assert!(json["evidence_ref"].is_null());
        assert_eq!(json["category"], "confirmed");
        assert_eq!(json["state_ids"], serde_json::json!([]));
    }

    /// coord cannot store a NUL in text, and one NUL in any free-text field
    /// used to cost the whole trial. Each reaches the wire as U+FFFD.
    #[test]
    fn a_nul_in_each_free_text_field_reaches_coord_as_the_replacement_character() {
        let record = ActionRecord::new(
            ActionKind::Spawn,
            Some("agent-7\0\0".into()),
            "rebuild=false\0".into(),
            &[],
        );
        let outcome = ActionOutcome {
            category: D3Category::Failure,
            signatures: vec!["DEV-PANIC-STARTUP".into()],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: Some("panic: open C:\\runner\0\0\0 failed".into()),
            late_signatures: vec![],
        };
        let body = SnapshotIngestBody::from_record(&record, &outcome, None, None);
        let wire = serde_json::to_string(&body).expect("serialize");
        assert!(!wire.contains("\\u0000"), "a NUL reached the wire: {wire}");

        let json: serde_json::Value = serde_json::from_str(&wire).expect("parse");
        assert_eq!(json["requester_id"], "agent-7\u{FFFD}\u{FFFD}");
        assert_eq!(json["params_digest"], "rebuild=false\u{FFFD}");
        assert_eq!(
            json["evidence_ref"],
            "panic: open C:\\runner\u{FFFD}\u{FFFD}\u{FFFD} failed"
        );
    }
}

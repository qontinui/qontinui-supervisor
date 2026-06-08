//! Phase-3 coord persistence for dev-action snapshots.
//!
//! Phase 1 keeps Action Snapshots in an in-memory ring on `SupervisorState`;
//! Phase 3 makes them durable by POSTing each *completed* snapshot (after the
//! attribution watcher folds its verdict) to qontinui-coord's
//! `POST /coord/dev-actions/snapshot` ingest endpoint.
//!
//! This is **best-effort and fail-open**: the attribution watcher calls
//! [`post_snapshot_to_coord`] after writing `record.outcome`, and a network
//! failure / non-2xx / unresolvable coord base only logs a single `warn!` line.
//! It NEVER blocks or fails the watcher (matching `fleet.rs::publish_budget`'s
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
//! - **coord base**: a local copy of the `COORD_HTTP_URL`-honoring resolver from
//!   [`crate::routes::lineage`] (that one is module-private). The lineage variant
//!   is preferred over `fleet.rs`'s because it honors the `COORD_HTTP_URL`
//!   override; this duplicates ~20 lines rather than refactoring broadly.
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
// coord base resolution (COORD_HTTP_URL-honoring; mirrors routes::lineage)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ProfilesFile {
    active: Option<String>,
    profiles: std::collections::HashMap<String, ProfileSubset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileSubset {
    coord_url: Option<String>,
}

fn profiles_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("profiles.json"))
}

/// Resolve the coord HTTP base. `COORD_HTTP_URL` wins; otherwise the active
/// profile's `coord_url` (`ws://…/ws` → `http://…`, `wss://` → `https://`).
///
/// Duplicated from [`crate::routes::lineage`]'s module-private `coord_http_base`
/// (preferred over `fleet.rs`'s copy because that one ignores `COORD_HTTP_URL`).
/// Kept local rather than refactoring both call sites into a shared helper.
///
/// `pub(crate)` so the Phase-4 expectations fetcher
/// ([`crate::dev_action::expectations`]) reuses this single resolver rather than
/// adding a third copy.
pub(crate) fn coord_http_base() -> Option<String> {
    if let Ok(env_url) = std::env::var("COORD_HTTP_URL") {
        let trimmed = env_url.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.trim_end_matches('/').to_string());
        }
    }

    let bytes = std::fs::read(profiles_path()?).ok()?;
    let pf: ProfilesFile = serde_json::from_slice(&bytes).ok()?;
    let active = pf.active.as_deref().unwrap_or("dev");
    let coord_url = pf.profiles.get(active)?.coord_url.as_deref()?;

    let trimmed = coord_url.trim_end_matches("/ws");
    let with_http = trimmed
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            trimmed
                .strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or_else(|| trimmed.to_string());
    Some(with_http.trim_end_matches('/').to_string())
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
            requester_id: record.requester_id.clone(),
            params_digest: record.params_digest.clone(),
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
            evidence_ref: outcome.evidence_ref.clone(),
            signatures: outcome.signatures.clone(),
            late_signatures: outcome.late_signatures.clone(),
            tenant_id,
            metadata: serde_json::Map::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

/// Best-effort POST of a completed snapshot to coord. Fail-open: any failure
/// (unresolvable coord base, network error, non-2xx) logs a single `warn!` and
/// returns. NEVER panics, NEVER propagates — safe to `await` or `tokio::spawn`
/// from the attribution watcher without risk to the watcher.
pub async fn post_snapshot_to_coord(
    record: &ActionRecord,
    outcome: &ActionOutcome,
    device_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
) {
    let base = match coord_http_base() {
        Some(b) => b,
        None => {
            debug!(
                "dev_action::ingest: coord base unresolved (no COORD_HTTP_URL / \
                 profiles.json coord_url) — skipping snapshot {} persistence.",
                record.action_id
            );
            return;
        }
    };

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
}

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

use crate::dev_action::record::{ActionKind, ActionOutcome, ActionRecord};

/// Timeout for the outbound coord ingest POST, matching
/// `fleet.rs::publish_budget`'s 5s budget.
const INGEST_TIMEOUT_SECS: u64 = 5;

/// The attribution semantics every snapshot this build posts was recorded
/// under, stamped as `metadata.attribution_epoch`.
///
/// **Bump rule: bump this whenever attribution semantics change** — what a
/// spawn, restart or build verdict is attributed to, what it closes as, or the
/// window it is read over — so coord can tell rows recorded under the old
/// semantics from rows recorded under the new. One epoch means one thing on a
/// row, so a change that moves only the stored category or evidence owes a bump
/// too. As of coord `6846970ac` (2026-11-03) coord stores the stamp and does not
/// act on it: its half of the plan below (Phase 5) is to skip the
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
/// be tied to, emits no signature for an undated `derived_status: Errored` unless
/// the API answered after the spawn, and measures the window's tail cap on the
/// tokio clock. #272 landed still stamping 2, so a row stamped 2 can carry either
/// semantics; a row stamped 3 carries #272's.
///
/// Epoch 4: a verdict the detached health wait decides is its own boot's, not
/// whichever spawn answered the port, and the wait runs to the first-healthy budget
/// (`process::health_probe::wait_for_action_boot_healthy`), so a boot answering
/// after 30 s no longer reads as failed and a crashed one no longer reads as up.
///
/// Epoch 5 closes `partial`, with its own `evidence_ref`, a window that holds an
/// `Errored` report it cannot place after the action's spawn, where it used to
/// close `confirmed`. Category and evidence only: no signature or window moves,
/// and coord's `dev_action_stats` keys on signatures, so its counts do not.
///
/// Epoch 6: a held-open door's `Ran` is held to the same spawn, as of when the port
/// answered (`dev_action::run_result::held_to_own_boot`). A row stamped 5 can still
/// read a crashed boot as up, when a respawn answered inside a held-open wait.
pub const ATTRIBUTION_EPOCH: u32 = 6;

/// The snapshot `metadata` this build stamps on every row, so a row names the
/// code that attributed it: `supervisor_build_sha` and `supervisor_build_dirty`
/// (from [`crate::self_provenance`], the same values `/health` serves; each is
/// JSON `null` when the build could not establish it, which is UNKNOWN, never
/// a sha or "clean"), and its [`ATTRIBUTION_EPOCH`].
///
/// A restart that fetched coord's expectations also carries
/// `route_lkg_shadow` (S3): the record's
/// [`crate::dev_action::policy::RouteLkgShadow`] plus `use_lkg`, which exe the
/// restart ran (the LKG one exactly when the door re-keyed it to
/// `restart_lkg`). Coord copies the block into its forecast ledger at first
/// ingest; snapshot metadata itself is pruned at 30 days.
fn snapshot_metadata(record: &ActionRecord) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(shadow) = route_lkg_shadow_block(record) {
        metadata.insert(ROUTE_LKG_SHADOW_KEY.to_string(), shadow);
    }
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

/// The metadata key of the S3 shadow stamp.
const ROUTE_LKG_SHADOW_KEY: &str = "route_lkg_shadow";

/// The metadata key a snapshot coord refused as `restart_lkg` carries when it
/// is re-sent as `restart` ([`post_snapshot_to_coord`]): the kind it was
/// refused as, so the row is still told apart from an untreated restart.
const REFUSED_KIND_KEY: &str = "refused_kind";

/// `metadata.route_lkg_shadow`: the record's shadow with `use_lkg` added, the
/// stamp's twelve fields. `None` for a record with no shadow.
fn route_lkg_shadow_block(record: &ActionRecord) -> Option<serde_json::Value> {
    let shadow = record.route_lkg_shadow.as_ref()?;
    let mut block = match serde_json::to_value(shadow) {
        Ok(serde_json::Value::Object(block)) => block,
        // A struct of numbers, strings and options always serializes to an
        // object; if it ever did not, the row goes without the stamp rather
        // than without the trial.
        _ => return None,
    };
    block.insert(
        "use_lkg".to_string(),
        serde_json::Value::Bool(record.kind() == ActionKind::RestartLkg),
    );
    Some(serde_json::Value::Object(block))
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
    /// `"restart" | "restart_lkg" | "spawn" | "build"` (from `ActionKind::as_str`).
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
            kind: record.kind().as_str(),
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
            metadata: snapshot_metadata(record),
        }
    }
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

impl SnapshotIngestBody {
    /// This body re-keyed from `restart_lkg` to `restart`, for a coord that
    /// refused the kind: `metadata.refused_kind` names it, and the shadow's
    /// `use_lkg: true` still marks the treatment.
    fn rekeyed_to_restart_for_a_coord_without_restart_lkg(mut self) -> Self {
        self.kind = ActionKind::Restart.as_str();
        self.metadata.insert(
            REFUSED_KIND_KEY.to_string(),
            serde_json::Value::from(ActionKind::RestartLkg.as_str()),
        );
        self
    }
}

/// Whether coord's answer to a snapshot of `kind` is the refusal of a coord
/// that does not accept `restart_lkg` yet: a 400 whose error is coord's
/// unknown-kind refusal (`dev_actions::snapshot_refusal`, "an unknown kind;
/// a snapshot's kind is one of ..."), for a `restart_lkg` snapshot. Any other
/// 400 is a refusal a re-key would not cure.
fn refused_for_restart_lkg(kind: &str, status: reqwest::StatusCode, detail: &str) -> bool {
    kind == ActionKind::RestartLkg.as_str()
        && status == reqwest::StatusCode::BAD_REQUEST
        && detail.contains("unknown kind")
}

/// Best-effort POST of a completed snapshot to the coord at `base`. Fail-open:
/// any failure (network error, non-2xx) logs a single `warn!` and returns. NEVER
/// panics, NEVER propagates — safe to `await` or `tokio::spawn` from the
/// attribution watcher without risk to the watcher.
///
/// A `restart_lkg` snapshot a coord refuses for its kind (one that predates the
/// kind: the snapshot table's CHECK and coord's `SNAPSHOT_KINDS` gained it in
/// the web migration and coord change of the same plan, and the refusal is
/// otherwise a lost trial) is sent once more as `restart`, carrying
/// `metadata.refused_kind = "restart_lkg"` and the shadow's `use_lkg: true`.
/// So no deploy order and no flag decides whether the trial survives: the
/// supervisor sends `restart_lkg` from the first build, coord keeps it under
/// that kind from the moment it accepts it, and before then keeps it, marked,
/// under `restart`. The cost is one refused POST per LKG-routed restart while
/// coord lags, at about one LKG restart a month.
pub async fn post_snapshot_to_coord(
    base: &str,
    record: &ActionRecord,
    outcome: &ActionOutcome,
    device_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
) {
    let body = SnapshotIngestBody::from_record(record, outcome, device_id, tenant_id);
    post_snapshot_body(base, record.action_id, body).await;
}

async fn post_snapshot_body(base: &str, action_id: Uuid, body: SnapshotIngestBody) {
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

    let mut body = body;
    // At most two sends: the second only for a `restart_lkg` refused for its
    // kind, re-keyed to `restart`, which cannot be refused that way again.
    for _ in 0..2 {
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!(
                        "dev_action::ingest: persisted snapshot {action_id} ({}) to coord",
                        body.kind
                    );
                    return;
                }
                let detail = resp.text().await.unwrap_or_default();
                if refused_for_restart_lkg(body.kind, status, &detail) {
                    warn!(
                        "dev_action::ingest: coord refused kind restart_lkg for snapshot \
                         {action_id} ({detail}); re-sending it as restart with \
                         metadata.refused_kind=restart_lkg"
                    );
                    body = body.rekeyed_to_restart_for_a_coord_without_restart_lkg();
                    continue;
                }
                warn!(
                    "dev_action::ingest: coord returned {status} for POST {url} \
                     (snapshot {action_id}): {detail}"
                );
                return;
            }
            Err(e) => {
                warn!(
                    "dev_action::ingest: POST {url} failed for snapshot {action_id} \
                     (non-fatal): {e}"
                );
                return;
            }
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
        // Epoch 1 is the unstamped pre-fix rows, 2 can carry semantics from before
        // #272, 3 reads a detached verdict off the port, 4 can close `confirmed` on a
        // report it could not place, and 5 reads a held-open verdict off the port. A
        // constant, so it is checked at compile time.
        const _: () = assert!(ATTRIBUTION_EPOCH >= 6);
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

    // -- S3: the route_lkg shadow stamp and the restart_lkg kind -------------

    use crate::dev_action::expectations::PredictedSignature;
    use crate::dev_action::policy::{assess_route_lkg_shadow, RouteLkgShadow};

    fn confirmed() -> ActionOutcome {
        ActionOutcome {
            category: D3Category::Confirmed,
            signatures: vec![],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: None,
            late_signatures: vec![],
        }
    }

    /// A restart whose expectations held the 4-of-4 incident cell.
    fn restart_with_shadow() -> ActionRecord {
        let mut record = ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &[]);
        let row = PredictedSignature {
            signature: "DEV-TAURI-ASSET-MISSING".to_string(),
            posterior_mean: 0.2,
            occurrences: 1,
            trials: 10,
            evidence_basis: "marginal".to_string(),
            exact_trials: Some(4),
            exact_occurrences: Some(4),
        };
        record.route_lkg_shadow = Some(assess_route_lkg_shadow(&[row], 0.5));
        record
    }

    /// The stamp carries the shadow's eleven fields plus `use_lkg`, which
    /// reads the kind at the stamp: `false` for a restart that ran the fresh
    /// exe, `true` once the door re-keyed it to `restart_lkg`, whose snapshot
    /// then goes out under that kind. A record with no shadow (another kind,
    /// or coord gave no answer) carries no stamp at all.
    #[test]
    fn a_restart_stamps_its_route_lkg_shadow_and_which_exe_ran() {
        let record = restart_with_shadow();
        let json = serde_json::to_value(SnapshotIngestBody::from_record(
            &record,
            &confirmed(),
            None,
            None,
        ))
        .expect("serialize");
        let block = &json["metadata"]["route_lkg_shadow"];
        let mut keys: Vec<&str> = block
            .as_object()
            .expect("the stamp is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "exact_trials",
                "expected_loss_fresh",
                "expected_loss_lkg",
                "live_route",
                "p_exact",
                "p_served",
                "ratio",
                "rule_version",
                "shadow_route",
                "top_signature",
                "trials",
                "use_lkg",
            ]
        );
        assert_eq!(block["use_lkg"], false);
        assert_eq!(block["shadow_route"], "lkg");
        assert_eq!(block["live_route"], "lkg");
        assert_eq!(block["top_signature"], "DEV-TAURI-ASSET-MISSING");
        assert_eq!(block["expected_loss_fresh"], 4.4 / 8.0);
        assert_eq!(block["expected_loss_lkg"], 0.5);
        assert_eq!(json["kind"], "restart");

        assert!(record.rekey_restart_to_lkg());
        let json = serde_json::to_value(SnapshotIngestBody::from_record(
            &record,
            &confirmed(),
            None,
            None,
        ))
        .expect("serialize");
        assert_eq!(json["kind"], "restart_lkg");
        assert_eq!(json["metadata"]["route_lkg_shadow"]["use_lkg"], true);

        let plain = ActionRecord::new(ActionKind::Spawn, None, "p".into(), &[]);
        let json = serde_json::to_value(SnapshotIngestBody::from_record(
            &plain,
            &confirmed(),
            None,
            None,
        ))
        .expect("serialize");
        assert!(json["metadata"].get("route_lkg_shadow").is_none(), "{json}");
    }

    /// The length Postgres's `numeric_out` prints for the JSON number `token`,
    /// a copy of coord's `dev_actions::rendered_numeric_len` (at coord
    /// `0a8d8e847`): no exponent, the integer digits the value needs (at least
    /// one), as many decimals as the token carries past its point (`1.0`
    /// keeps its one, `1.5e-7` carries 8), and a minus sign unless the value
    /// is zero. Applied to the token serde_json sends, not to a re-formatting
    /// of the value: serde_json writes a whole float as `1.0`, which Postgres
    /// prints with its scale.
    fn pg_numeric_len(token: &str) -> usize {
        let (sign, unsigned) = match token.strip_prefix('-') {
            Some(rest) => (1, rest),
            None => (0, token),
        };
        let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().unwrap_or(0)),
            None => (unsigned, 0),
        };
        let sign = if mantissa.bytes().all(|b| matches!(b, b'0' | b'.')) {
            0
        } else {
            sign
        };
        let (int_digits, frac_digits) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let int_len = (int_digits.len() as i64).saturating_add(exponent).max(1);
        let scale = (frac_digits.len() as i64).saturating_sub(exponent).max(0);
        let len = int_len.saturating_add(if scale > 0 { scale + 1 } else { 0 });
        usize::try_from(len)
            .unwrap_or(usize::MAX)
            .saturating_add(sign)
    }

    /// The length of `value` as Postgres prints a jsonb back, which is what
    /// coord bounds (`dev_actions::rendered_jsonb_len`, at coord `0a8d8e847`):
    /// `", "` between members, `": "` after each key, and each number as
    /// [`pg_numeric_len`] prints the token serde_json sends.
    fn pg_rendered_len(value: &serde_json::Value) -> usize {
        use serde_json::Value;
        let frame = |n: usize| 2 + 2 * n.saturating_sub(1);
        let escaped = |s: &str| serde_json::to_string(s).expect("a string").len();
        match value {
            Value::Null | Value::Bool(true) => 4,
            Value::Bool(false) => 5,
            Value::Number(n) => pg_numeric_len(&serde_json::to_string(n).expect("a number")),
            Value::String(s) => escaped(s),
            Value::Array(items) => {
                frame(items.len()) + items.iter().map(pg_rendered_len).sum::<usize>()
            }
            Value::Object(map) => {
                frame(map.len())
                    + map
                        .iter()
                        .map(|(k, v)| escaped(k) + 2 + pg_rendered_len(v))
                        .sum::<usize>()
            }
        }
    }

    /// The measure agrees with coord's rule on the tokens that matter here:
    /// the whole float `1.0` keeps its scale, an exponent expands, and an
    /// integer prints as sent.
    #[test]
    fn the_metadata_measure_prints_numbers_as_postgres_does() {
        assert_eq!(serde_json::to_string(&1.0_f64).expect("serialize"), "1.0");
        assert_eq!(pg_numeric_len("1.0"), 3);
        assert_eq!(pg_numeric_len("0.5"), 3);
        assert_eq!(pg_numeric_len("1.5e-7"), "0.00000015".len());
        assert_eq!(pg_numeric_len("5e-324"), 2 + 324);
        assert_eq!(pg_numeric_len("-0.0"), 3);
        assert_eq!(pg_numeric_len("18446744073709551615"), 20);
        assert_eq!(
            pg_rendered_len(&serde_json::json!({"a": 1.0, "b": [1, 2]})),
            r#"{"a": 1.0, "b": [1, 2]}"#.len()
        );
    }

    /// Coord refuses a snapshot whose metadata, as Postgres prints it back,
    /// passes 4096 bytes (`MAX_SNAPSHOT_METADATA_BYTES`), and a refused
    /// snapshot is a lost trial. The worst stamp this build can send, with
    /// every field at its longest (a 40-hex sha, a re-sent `restart_lkg`'s
    /// `refused_kind`, the longest boot signature, floats with the most digits
    /// and the smallest magnitudes, `u64::MAX` counts) beside the five-row
    /// `predicted[]` Phase 5 adds (128-byte signatures, coord's
    /// `MAX_SIGNATURE_BYTES`; the smallest posterior a `u64::MAX`-trial cell
    /// serves; `u64::MAX` counts), fits, at each extreme ratio
    /// `SUPERVISOR_ROUTE_LKG_RATIO` accepts: the smallest positive float
    /// (`5e-324`, which Postgres prints as 326 characters, twice in the stamp),
    /// `1` (sent as `1.0`), and a ratio with 17 significant digits. Fails if a
    /// field is added or lengthened past the budget, or the ratio parser
    /// starts accepting a value that prints longer.
    #[test]
    fn the_worst_case_stamp_and_a_five_row_predicted_fit_coords_metadata_cap() {
        const MAX_SNAPSHOT_METADATA_BYTES: usize = 4096;
        let longest_boot = crate::dev_action::policy::BOOT_FAILURE_SIGNATURES
            .iter()
            .max_by_key(|s| s.len())
            .expect("a boot signature");
        // A float serde_json sends with 16 significant digits in exponent form
        // behind 19 leading zeros: about as long as a probability prints.
        let tiny = 0.4 / (u64::MAX as f64 + 4.0) * (1.0 + f64::EPSILON);
        for declared in ["5e-324", "1", "0.30000000000000004"] {
            let (ratio, warning) = crate::dev_action::policy::parse_route_lkg_ratio(Some(declared));
            assert_eq!(warning, None, "{declared} is a ratio the env var accepts");
            let record = restart_with_shadow();
            let mut body = SnapshotIngestBody::from_record(&record, &confirmed(), None, None)
                .rekeyed_to_restart_for_a_coord_without_restart_lkg();
            let worst = RouteLkgShadow {
                rule_version: u32::MAX,
                ratio,
                shadow_route: crate::dev_action::policy::Route::Fresh,
                live_route: crate::dev_action::policy::Route::Fresh,
                top_signature: Some(longest_boot.to_string()),
                p_served: Some(tiny),
                trials: Some(u64::MAX),
                p_exact: Some(tiny),
                exact_trials: Some(u64::MAX),
                expected_loss_fresh: Some(tiny),
                expected_loss_lkg: ratio,
            };
            let mut worst_record =
                ActionRecord::new(ActionKind::Restart, None, "rebuild=false".into(), &[]);
            worst_record.route_lkg_shadow = Some(worst);
            assert!(worst_record.rekey_restart_to_lkg());
            body.metadata.insert(
                ROUTE_LKG_SHADOW_KEY.to_string(),
                route_lkg_shadow_block(&worst_record).expect("a stamp"),
            );
            body.metadata.insert(
                "supervisor_build_sha".to_string(),
                serde_json::Value::from("f".repeat(40)),
            );
            body.metadata.insert(
                "supervisor_build_dirty".to_string(),
                serde_json::Value::from(false),
            );
            body.metadata.insert(
                "attribution_epoch".to_string(),
                serde_json::Value::from(u32::MAX),
            );
            let predicted_row = |i: usize| PredictedSignature {
                signature: format!("DEV-{i}-{}", "X".repeat(128 - 6)),
                posterior_mean: tiny,
                occurrences: u64::MAX,
                trials: u64::MAX,
                evidence_basis: "prior_only".to_string(),
                exact_trials: Some(u64::MAX),
                exact_occurrences: Some(u64::MAX),
            };
            let predicted: Vec<PredictedSignature> = (0..5).map(predicted_row).collect();
            assert!(predicted.iter().all(|p| p.signature.len() == 128));
            body.metadata.insert(
                "predicted".to_string(),
                serde_json::to_value(&predicted).expect("serialize"),
            );

            // Measured on the bytes that go over the wire, as coord sees them.
            let wire = serde_json::to_string(&serde_json::Value::Object(body.metadata.clone()))
                .expect("serialize");
            let metadata: serde_json::Value = serde_json::from_str(&wire).expect("parse");
            let rendered = pg_rendered_len(&metadata);
            assert!(
                rendered <= MAX_SNAPSHOT_METADATA_BYTES,
                "ratio {declared}: the worst-case metadata renders {rendered} bytes, \
                 over coord's {MAX_SNAPSHOT_METADATA_BYTES}: {metadata}"
            );
            eprintln!("ratio {declared}: worst-case metadata renders {rendered} bytes");
        }
        // The measure is not vacuous: the probabilities print expanded.
        let token = serde_json::to_string(&tiny).expect("serialize");
        assert!(pg_numeric_len(&token) > 30, "{token}");
    }

    /// Only coord's unknown-kind 400 on a `restart_lkg` snapshot is cured by
    /// re-sending it as `restart`.
    #[test]
    fn only_an_unknown_kind_refusal_of_restart_lkg_is_resent() {
        use reqwest::StatusCode;
        let unknown = "an unknown kind; a snapshot's kind is one of restart, spawn, build";
        assert!(refused_for_restart_lkg(
            "restart_lkg",
            StatusCode::BAD_REQUEST,
            unknown
        ));
        assert!(!refused_for_restart_lkg(
            "restart",
            StatusCode::BAD_REQUEST,
            unknown
        ));
        assert!(!refused_for_restart_lkg(
            "restart_lkg",
            StatusCode::BAD_REQUEST,
            "metadata of 5000 bytes"
        ));
        assert!(!refused_for_restart_lkg(
            "restart_lkg",
            StatusCode::INTERNAL_SERVER_ERROR,
            unknown
        ));
    }

    /// A coord that does not accept `restart_lkg` yet still keeps the trial:
    /// the refused snapshot is re-sent once as `restart`, marked
    /// `refused_kind`, with `use_lkg: true` in its stamp. A coord that accepts
    /// the kind gets one POST under it. Drives the real POST against a planted
    /// coord, which refuses `restart_lkg` exactly as coord's `snapshot_refusal`
    /// does when `toggle` reads false.
    #[tokio::test]
    async fn a_coord_without_restart_lkg_keeps_the_trial_as_a_marked_restart() {
        use axum::extract::Json as AxumJson;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let accepts_lkg = Arc::new(AtomicBool::new(false));
        let (recorder, toggle) = (seen.clone(), accepts_lkg.clone());
        let app = axum::Router::new().route(
            "/coord/dev-actions/snapshot",
            post(move |AxumJson(body): AxumJson<serde_json::Value>| {
                let (recorder, toggle) = (recorder.clone(), toggle.clone());
                async move {
                    let kind = body["kind"].as_str().unwrap_or_default().to_string();
                    recorder.lock().expect("seen").push(body);
                    if kind == "restart_lkg" && !toggle.load(Ordering::SeqCst) {
                        (
                            StatusCode::BAD_REQUEST,
                            AxumJson(serde_json::json!({
                                "error": "an unknown kind; a snapshot's kind is one of \
                                          restart, spawn, build, deploy, migrate"
                            })),
                        )
                    } else {
                        (StatusCode::OK, AxumJson(serde_json::json!({"ok": true})))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let record = restart_with_shadow();
        assert!(record.rekey_restart_to_lkg());
        post_snapshot_to_coord(&base, &record, &confirmed(), None, None).await;
        let sent = std::mem::take(&mut *seen.lock().expect("seen"));
        assert_eq!(sent.len(), 2, "refused once, then re-sent: {sent:?}");
        assert_eq!(sent[0]["kind"], "restart_lkg");
        assert!(sent[0]["metadata"].get("refused_kind").is_none());
        assert_eq!(sent[1]["kind"], "restart");
        assert_eq!(sent[1]["metadata"]["refused_kind"], "restart_lkg");
        assert_eq!(sent[1]["metadata"]["route_lkg_shadow"]["use_lkg"], true);
        assert_eq!(sent[1]["action_id"], sent[0]["action_id"]);

        accepts_lkg.store(true, Ordering::SeqCst);
        post_snapshot_to_coord(&base, &record, &confirmed(), None, None).await;
        let sent = std::mem::take(&mut *seen.lock().expect("seen"));
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["kind"], "restart_lkg");
    }
}

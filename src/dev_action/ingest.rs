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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
///
/// Epoch 7 (#292, S3): a `restart` that ran the last-known-good exe is re-keyed to
/// `restart_lkg`, so a `restart` row stamped 7 is a restart of the fresh exe. A row
/// stamped 6 or below can be either: the LKG route was pooled into `restart`, which
/// credits the LKG exe's clean boots to "restart under these states" (the
/// performativity the kind split removes). One exception at 7: while coord still
/// refuses `restart_lkg`, an LKG restart is resent as `restart` carrying
/// `metadata.refused_kind: "restart_lkg"`, which a reader that keeps the two apart
/// must exclude from `restart`. #292 landed still stamping 6, so a 6 from a build
/// containing it carries the split too; a row stamped 7 always does.
pub const ATTRIBUTION_EPOCH: u32 = 7;

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
/// `restart_lkg`). Snapshot metadata itself is pruned at 30 days.
///
/// A minted record also carries `served` (Phase 5, S2): what the initiator
/// was served at the mint ([`served_block`]). Coord does not copy either block
/// into its forecast ledger as of coord `79429fa20` (its `forecast_ledger`
/// issues from its own expectations read); that copy is the coord half of
/// Phase 6c.
///
/// The whole map is held under [`METADATA_BUDGET_BYTES`] ([`fit_metadata`]),
/// because coord refuses a snapshot whose metadata passes
/// `MAX_SNAPSHOT_METADATA_BYTES` and the refusal would lose the trial.
fn snapshot_metadata(record: &ActionRecord) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(shadow) = route_lkg_shadow_block(record) {
        metadata.insert(ROUTE_LKG_SHADOW_KEY.to_string(), shadow);
    }
    if let Some(served) = served_block(record) {
        metadata.insert(SERVED_KEY.to_string(), served);
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
    fit_metadata(&mut metadata);
    metadata
}

/// The metadata key of the Phase 5 served stamp.
const SERVED_KEY: &str = "served";

/// The size ([`metadata_len`], coord's own measure) this build keeps snapshot
/// metadata under: coord's `MAX_SNAPSHOT_METADATA_BYTES` (4096) less room for
/// the restart_lkg re-key's `refused_kind` and for slack. coord strips and re-adds its own
/// `ingest_auth` after its check, so that key costs nothing here.
pub const METADATA_BUDGET_BYTES: usize = 3584;

/// `metadata.served`, for a record a mint prepared:
///
/// - `predicted`: the ACK's `predicted` rows exactly as served;
/// - `predicted_truncated`: `true` when [`fit_metadata`] cut rows to stay in
///   budget (then `predicted` is a prefix of what was served);
/// - `expectations_basis`: coord's top-level basis at the mint (`null` = coord
///   gave no answer, never "no rows");
/// - `route_lkg`: the live rule's advice;
/// - `use_lkg`: whether the restart ran the last-known-good exe (the kind is
///   `restart_lkg`), and `null` for a kind that is not a restart, where the
///   question does not arise.
fn served_block(record: &ActionRecord) -> Option<serde_json::Value> {
    let served = record.served.as_ref()?;
    let use_lkg = match record.kind() {
        ActionKind::Restart => serde_json::Value::Bool(false),
        ActionKind::RestartLkg => serde_json::Value::Bool(true),
        ActionKind::Spawn | ActionKind::Build => serde_json::Value::Null,
    };
    Some(serde_json::json!({
        "predicted": served.predicted,
        "predicted_truncated": false,
        "expectations_basis": record.expectations_basis,
        "route_lkg": served.route_lkg,
        "use_lkg": use_lkg,
    }))
}

/// The metadata's size as coord's `snapshot_refusal` measures it: the JSON as
/// Postgres prints it back from a jsonb column ([`rendered_jsonb_len`]), which
/// is longer than compact serde JSON (`", "` / `": "` separators, every number
/// in full decimal).
fn metadata_len(metadata: &serde_json::Map<String, serde_json::Value>) -> usize {
    metadata
        .iter()
        .fold(2 + 2 * metadata.len().saturating_sub(1), |acc, (k, v)| {
            acc.saturating_add(escaped_len(k))
                .saturating_add(2)
                .saturating_add(rendered_jsonb_len(v))
        })
}

/// The length of `value` as Postgres prints it back from a jsonb column
/// (`jsonb_out`): `", "` between elements, `": "` after a key, strings escaped
/// as JSON escapes them, and every number in full decimal. A port of coord's
/// `dev_actions::rendered_jsonb_len` (qontinui-coord `00737e193`), the measure
/// its `MAX_SNAPSHOT_METADATA_BYTES` refusal applies; keep the two in step.
fn rendered_jsonb_len(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    let frame = |n: usize| 2 + 2 * n.saturating_sub(1);
    let total = |lens: &mut dyn Iterator<Item = usize>| lens.fold(0, usize::saturating_add);
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => rendered_numeric_len(&n.to_string()),
        Value::String(s) => escaped_len(s),
        Value::Array(items) => {
            frame(items.len()).saturating_add(total(&mut items.iter().map(rendered_jsonb_len)))
        }
        Value::Object(map) => {
            frame(map.len()).saturating_add(total(&mut map.iter().map(|(k, v)| {
                escaped_len(k)
                    .saturating_add(2)
                    .saturating_add(rendered_jsonb_len(v))
            })))
        }
    }
}

/// A string's length as a JSON literal, quotes and escapes included.
fn escaped_len(s: &str) -> usize {
    serde_json::to_string(s).map_or(usize::MAX, |e| e.len())
}

/// The length Postgres's `numeric_out` prints for the JSON number `token`: no
/// exponent, the integer digits the value needs (at least one), as many
/// decimals as the token carries past its point, and a minus sign unless the
/// value is zero.
fn rendered_numeric_len(token: &str) -> usize {
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

/// Hold `metadata` under [`METADATA_BUDGET_BYTES`]: drop `served.predicted`
/// rows from the tail (lowest posterior first, since the ACK is sorted) and
/// mark `predicted_truncated`; if that is not enough, drop the `served` block,
/// then the `route_lkg_shadow` block. A trial with less metadata beats a
/// refused one.
fn fit_metadata(metadata: &mut serde_json::Map<String, serde_json::Value>) {
    while metadata_len(metadata) > METADATA_BUDGET_BYTES {
        let popped = metadata
            .get_mut(SERVED_KEY)
            .and_then(|served| {
                let rows = served.get_mut("predicted")?.as_array_mut()?;
                rows.pop()?;
                Some(served)
            })
            .map(|served| served["predicted_truncated"] = serde_json::Value::Bool(true))
            .is_some();
        if popped {
            continue;
        }
        // No rows left to cut: what is still over is coord's uncut basis in
        // `served`, so that block goes before the small, fixed-shape shadow.
        if metadata.remove(SERVED_KEY).is_some() {
            warn!(
                "dev_action::ingest: dropped metadata.served to keep the snapshot's metadata \
                 under {METADATA_BUDGET_BYTES} bytes"
            );
            continue;
        }
        if metadata.remove(ROUTE_LKG_SHADOW_KEY).is_some() {
            warn!(
                "dev_action::ingest: dropped metadata.route_lkg_shadow to keep the snapshot's \
                 metadata under {METADATA_BUDGET_BYTES} bytes"
            );
            continue;
        }
        // Nothing left that this build may cut; send it and let coord answer.
        return;
    }
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

// ---------------------------------------------------------------------------
// Delivery: device bearer, bounded retry, drop counter
// ---------------------------------------------------------------------------

/// Attempts per snapshot, the first included. A snapshot whose every attempt
/// met a retryable failure ([`Disposition::Retry`]) is dropped and counted.
pub const INGEST_MAX_ATTEMPTS: u32 = 4;

/// The waits before attempts 2, 3 and 4 (production). Spread over ~2.5 min so
/// a coord deploy roll or a short outage is ridden out, while no snapshot
/// waits long enough to need a durable queue.
#[cfg(not(test))]
const INGEST_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// Tests run the real retry loop with millisecond waits.
#[cfg(test)]
const INGEST_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
];

const _: () = assert!(INGEST_RETRY_BACKOFF.len() == (INGEST_MAX_ATTEMPTS - 1) as usize);

/// Snapshots in delivery (in flight or waiting out a backoff) at once. This
/// in-memory set is the buffer: a snapshot arriving when it is full is dropped
/// and counted rather than queued without bound. It is NOT durable: a
/// supervisor exit loses what it holds, and no later process counts that.
pub const INGEST_MAX_PENDING: usize = 64;

/// Process-lifetime counters of snapshot delivery, served on `GET /health` as
/// `supervisor.dev_action_ingest` ([`ingest_counters`]). Reset on supervisor
/// restart, so read them as counts since this process started.
static INGEST_STATS: IngestStats = IngestStats::new();

/// The live counters of one delivery buffer ([`DeliveryBuffer::stats`]).
struct IngestStats {
    delivered: AtomicU64,
    retried: AtomicU64,
    refused: AtomicU64,
    dropped_retries_exhausted: AtomicU64,
    dropped_buffer_full: AtomicU64,
    pending: AtomicUsize,
}

impl IngestStats {
    const fn new() -> Self {
        Self {
            delivered: AtomicU64::new(0),
            retried: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            dropped_retries_exhausted: AtomicU64::new(0),
            dropped_buffer_full: AtomicU64::new(0),
            pending: AtomicUsize::new(0),
        }
    }

    fn snapshot(&self) -> IngestCounters {
        IngestCounters {
            delivered: self.delivered.load(Ordering::Relaxed),
            retried: self.retried.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            dropped_retries_exhausted: self.dropped_retries_exhausted.load(Ordering::Relaxed),
            dropped_buffer_full: self.dropped_buffer_full.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Relaxed),
        }
    }
}

/// A point read of the delivery counters.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct IngestCounters {
    /// Snapshots coord answered 2xx.
    pub delivered: u64,
    /// Retry attempts sent (a snapshot delivered on its third attempt adds 2).
    pub retried: u64,
    /// Snapshots coord refused with a status a resend cannot cure (a 4xx other
    /// than 408 / 429). Not a drop: coord answered. The `warn!` names its body.
    pub refused: u64,
    /// Snapshots dropped after [`INGEST_MAX_ATTEMPTS`] retryable failures.
    pub dropped_retries_exhausted: u64,
    /// Snapshots dropped because [`INGEST_MAX_PENDING`] were already in delivery.
    pub dropped_buffer_full: u64,
    /// Snapshots in delivery right now.
    pub pending: usize,
}

/// The delivery counters, for `GET /health`.
pub fn ingest_counters() -> IngestCounters {
    INGEST_STATS.snapshot()
}

/// A delivery buffer: its counters (the pending gauge among them) and its
/// capacity. Production uses [`DELIVERY_BUFFER`]; a test bounds and counts its
/// own, so no other test's delivery can move what it asserts.
struct DeliveryBuffer {
    stats: &'static IngestStats,
    capacity: usize,
}

/// The process's delivery buffer: [`INGEST_STATS`] under [`INGEST_MAX_PENDING`].
const DELIVERY_BUFFER: DeliveryBuffer = DeliveryBuffer {
    stats: &INGEST_STATS,
    capacity: INGEST_MAX_PENDING,
};

/// A slot in a delivery buffer, released on drop (every exit of the loop, a
/// panic or a cancelled task included).
struct PendingSlot {
    pending: &'static AtomicUsize,
}

impl PendingSlot {
    /// Take a slot of `buffer`, or `None` when its capacity is taken.
    fn take(buffer: &DeliveryBuffer) -> Option<Self> {
        let capacity = buffer.capacity;
        buffer
            .stats
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < capacity).then_some(n + 1)
            })
            .ok()
            .map(|_| PendingSlot {
                pending: &buffer.stats.pending,
            })
    }
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What one attempt's answer means for the snapshot.
#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    Delivered,
    /// Worth another attempt: no answer (a connect error, a timeout), a 5xx, a
    /// 408 or a 429. A resend is safe at coord: a re-POST of the same action
    /// id fills in and never rewrites (coord's dev-action ingest since plan
    /// `2026-10-27-bayesian-consequence-model-for-the-dev-twin` Phase 4), so
    /// an attempt that timed out after coord committed it costs nothing when
    /// it is sent again.
    Retry,
    /// coord answered, and a resend would get the same answer.
    Refused,
}

fn disposition(status: Option<reqwest::StatusCode>) -> Disposition {
    match status {
        None => Disposition::Retry,
        Some(s) if s.is_success() => Disposition::Delivered,
        Some(s)
            if s.is_server_error()
                || s == reqwest::StatusCode::REQUEST_TIMEOUT
                || s == reqwest::StatusCode::TOO_MANY_REQUESTS =>
        {
            Disposition::Retry
        }
        Some(_) => Disposition::Refused,
    }
}

/// Deliver a completed snapshot to the coord at `base`. Fail-open: every
/// failure is logged and counted ([`ingest_counters`]), none propagates, and it
/// never panics, so the attribution watcher can `tokio::spawn` it.
///
/// Each attempt carries this box's coord device JWT when one resolves from
/// `sources` (re-resolved per attempt, so a token that expired during a backoff
/// is replaced); coord observes it and never requires it. A retryable failure
/// ([`Disposition::Retry`]) is retried up to [`INGEST_MAX_ATTEMPTS`] with
/// [`INGEST_RETRY_BACKOFF`], at most [`INGEST_MAX_PENDING`] snapshots at once;
/// past either bound the snapshot is dropped and counted.
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
/// coord lags, at about one LKG restart a month. The re-key does not use up a
/// retry attempt.
pub(crate) async fn post_snapshot_to_coord(
    base: &str,
    record: &ActionRecord,
    outcome: &ActionOutcome,
    device_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
    sources: &crate::fleet::DeviceSources,
) {
    let body = SnapshotIngestBody::from_record(record, outcome, device_id, tenant_id);
    deliver_snapshot(base, record.action_id, body, || {
        crate::fleet::resolve_device_bearer_cached_for(base, sources)
    })
    .await;
}

async fn deliver_snapshot<F, Fut>(
    base: &str,
    action_id: Uuid,
    body: SnapshotIngestBody,
    resolve_bearer: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = crate::fleet::ResolvedBearer>,
{
    deliver_snapshot_in(&DELIVERY_BUFFER, base, action_id, body, resolve_bearer).await;
}

async fn deliver_snapshot_in<F, Fut>(
    buffer: &DeliveryBuffer,
    base: &str,
    action_id: Uuid,
    body: SnapshotIngestBody,
    resolve_bearer: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = crate::fleet::ResolvedBearer>,
{
    let (capacity, stats) = (buffer.capacity, buffer.stats);
    let Some(_slot) = PendingSlot::take(buffer) else {
        stats.dropped_buffer_full.fetch_add(1, Ordering::Relaxed);
        warn!(
            "dev_action::ingest: DROPPED snapshot {action_id} ({}): {capacity} \
             snapshots are already in delivery (dropped_buffer_full)",
            body.kind
        );
        return;
    };
    let url = format!("{base}/coord/dev-actions/snapshot");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(INGEST_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            // No attempt can be made at all, so it is a drop like any other.
            stats
                .dropped_retries_exhausted
                .fetch_add(1, Ordering::Relaxed);
            warn!("dev_action::ingest: DROPPED snapshot {action_id}: reqwest builder failed: {e}");
            return;
        }
    };

    let mut body = body;
    let mut attempt: u32 = 1;
    let mut rekeyed = false;
    loop {
        let bearer = resolve_bearer().await;
        let sent = crate::fleet::attach_bearer_quietly(client.post(&url).json(&body), bearer)
            .send()
            .await;
        let (status, detail) = match sent {
            Ok(resp) => {
                let status = resp.status();
                let detail = if status.is_success() {
                    String::new()
                } else {
                    resp.text().await.unwrap_or_default()
                };
                (Some(status), detail)
            }
            Err(e) => (None, e.to_string()),
        };
        match (disposition(status), status) {
            (Disposition::Delivered, _) => {
                stats.delivered.fetch_add(1, Ordering::Relaxed);
                debug!(
                    "dev_action::ingest: persisted snapshot {action_id} ({}) to coord \
                     (attempt {attempt})",
                    body.kind
                );
                return;
            }
            (Disposition::Refused, Some(status)) => {
                // Only coord's unknown-kind 400 on a `restart_lkg` snapshot is
                // cured by a re-key, and only once.
                if !rekeyed && refused_for_restart_lkg(body.kind, status, &detail) {
                    warn!(
                        "dev_action::ingest: coord refused kind restart_lkg for snapshot \
                         {action_id} ({detail}); re-sending it as restart with \
                         metadata.refused_kind=restart_lkg"
                    );
                    body = body.rekeyed_to_restart_for_a_coord_without_restart_lkg();
                    rekeyed = true;
                    continue;
                }
                stats.refused.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "dev_action::ingest: coord returned {status} for POST {url} \
                     (snapshot {action_id}, not retried): {detail}"
                );
                return;
            }
            (Disposition::Refused, None) | (Disposition::Retry, _) => {
                let what = match status {
                    Some(s) => format!("coord returned {s}: {detail}"),
                    None => format!("no answer: {detail}"),
                };
                if attempt >= INGEST_MAX_ATTEMPTS {
                    stats
                        .dropped_retries_exhausted
                        .fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "dev_action::ingest: DROPPED snapshot {action_id} ({}) after \
                         {attempt} attempts (dropped_retries_exhausted); last: {what}",
                        body.kind
                    );
                    return;
                }
                let wait = INGEST_RETRY_BACKOFF[(attempt - 1) as usize];
                debug!(
                    "dev_action::ingest: POST {url} for snapshot {action_id} failed on \
                     attempt {attempt} ({what}); retrying in {wait:?}"
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                stats.retried.fetch_add(1, Ordering::Relaxed);
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
        // report it could not place, 5 reads a held-open verdict off the port, and 6
        // pools LKG-routed restarts into `restart`. A constant, so it is checked at
        // compile time.
        const _: () = assert!(ATTRIBUTION_EPOCH >= 7);
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

    /// Sources that resolve no bearer and read no identity.
    fn no_sources() -> crate::fleet::DeviceSources {
        crate::fleet::DeviceSources::for_access(&crate::config::AmbientCoord::Ignore)
    }

    fn served_row(signature: &str, posterior_mean: f64) -> PredictedSignature {
        PredictedSignature {
            signature: signature.to_string(),
            posterior_mean,
            occurrences: 1,
            trials: 10,
            evidence_basis: "marginal".to_string(),
            exact_trials: None,
            exact_occurrences: None,
        }
    }

    /// A coord stand-in that answers POSTs from a script of statuses (the last
    /// one repeating) and records each request's `Authorization` header.
    async fn scripted_coord(
        script: Vec<u16>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let recorder = seen.clone();
        let script = Arc::new(script);
        let app = axum::Router::new().route(
            "/coord/dev-actions/snapshot",
            post(move |headers: HeaderMap| {
                let (recorder, script) = (recorder.clone(), script.clone());
                async move {
                    let mut seen = recorder.lock().expect("seen");
                    seen.push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                    );
                    let code = script[(seen.len() - 1).min(script.len() - 1)];
                    StatusCode::from_u16(code).expect("status")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        (base, seen)
    }

    fn body_for(record: &ActionRecord) -> SnapshotIngestBody {
        SnapshotIngestBody::from_record(record, &confirmed(), None, None)
    }

    /// Which answers are retried: none, a 5xx, a 408, a 429. A 2xx delivers;
    /// any other 4xx is a refusal a resend would meet again.
    #[test]
    fn only_no_answer_5xx_408_and_429_are_retried() {
        use reqwest::StatusCode;
        assert_eq!(disposition(None), Disposition::Retry);
        for code in [500, 502, 503, 504, 408, 429] {
            let s = StatusCode::from_u16(code).expect("status");
            assert_eq!(disposition(Some(s)), Disposition::Retry, "{code}");
        }
        for code in [200, 201, 204] {
            let s = StatusCode::from_u16(code).expect("status");
            assert_eq!(disposition(Some(s)), Disposition::Delivered, "{code}");
        }
        for code in [400, 401, 403, 404, 409, 413, 422] {
            let s = StatusCode::from_u16(code).expect("status");
            assert_eq!(disposition(Some(s)), Disposition::Refused, "{code}");
        }
    }

    /// A delivery buffer with counters of its own, so no other test's delivery
    /// (the attribution watchers' among them) can move what a test asserts.
    fn private_buffer(capacity: usize) -> DeliveryBuffer {
        DeliveryBuffer {
            stats: Box::leak(Box::new(IngestStats::new())),
            capacity,
        }
    }

    fn no_bearer() -> crate::fleet::ResolvedBearer {
        (None, crate::fleet::SOURCE_NONE)
    }

    /// A snapshot coord answers 503 twice is delivered on the third attempt,
    /// and every attempt carries the device bearer the resolver hands it.
    #[tokio::test]
    async fn a_transient_failure_is_retried_with_the_bearer_on_every_attempt() {
        let buffer = private_buffer(INGEST_MAX_PENDING);
        let (base, seen) = scripted_coord(vec![503, 503, 200]).await;
        let record = restart_with_shadow();
        deliver_snapshot_in(
            &buffer,
            &base,
            record.action_id,
            body_for(&record),
            || async { (Some("header.payload.sig".to_string()), "test") },
        )
        .await;
        let seen = seen.lock().expect("seen").clone();
        assert_eq!(seen.len(), 3, "two retries, then delivered: {seen:?}");
        assert!(
            seen.iter()
                .all(|a| a.as_deref() == Some("Bearer header.payload.sig")),
            "{seen:?}"
        );
        let c = buffer.stats.snapshot();
        assert_eq!((c.delivered, c.retried, c.pending), (1, 2, 0));
        assert_eq!(
            (
                c.refused,
                c.dropped_retries_exhausted,
                c.dropped_buffer_full
            ),
            (0, 0, 0)
        );
    }

    /// A snapshot that never gets an answer worth keeping is sent
    /// INGEST_MAX_ATTEMPTS times, then dropped and counted; without a bearer
    /// no `Authorization` header is sent at all.
    #[tokio::test]
    async fn a_snapshot_failing_every_attempt_is_dropped_and_counted() {
        let buffer = private_buffer(INGEST_MAX_PENDING);
        let (base, seen) = scripted_coord(vec![503]).await;
        let record = restart_with_shadow();
        deliver_snapshot_in(
            &buffer,
            &base,
            record.action_id,
            body_for(&record),
            || async { no_bearer() },
        )
        .await;
        let seen = seen.lock().expect("seen").clone();
        assert_eq!(seen.len(), INGEST_MAX_ATTEMPTS as usize, "{seen:?}");
        assert!(seen.iter().all(Option::is_none), "{seen:?}");
        let c = buffer.stats.snapshot();
        assert_eq!(c.dropped_retries_exhausted, 1);
        assert_eq!(c.retried, u64::from(INGEST_MAX_ATTEMPTS - 1));
        assert_eq!((c.delivered, c.pending), (0, 0));
    }

    /// A refused connection is retried the same way (no answer is retryable).
    /// Port 1 never listens, so no other test's stand-in can answer it.
    #[tokio::test]
    async fn an_unreachable_coord_is_retried_then_dropped() {
        let buffer = private_buffer(INGEST_MAX_PENDING);
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let record = restart_with_shadow();
        deliver_snapshot_in(
            &buffer,
            "http://127.0.0.1:1",
            record.action_id,
            body_for(&record),
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { no_bearer() }
            },
        )
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), INGEST_MAX_ATTEMPTS);
        let c = buffer.stats.snapshot();
        assert_eq!(
            (c.dropped_retries_exhausted, c.delivered, c.pending),
            (1, 0, 0)
        );
    }

    /// A 400 is not retried: one attempt, counted refused, not dropped.
    #[tokio::test]
    async fn a_refusal_is_sent_once_and_counted_refused() {
        let buffer = private_buffer(INGEST_MAX_PENDING);
        let (base, seen) = scripted_coord(vec![400]).await;
        let record = restart_with_shadow();
        deliver_snapshot_in(
            &buffer,
            &base,
            record.action_id,
            body_for(&record),
            || async { no_bearer() },
        )
        .await;
        assert_eq!(seen.lock().expect("seen").len(), 1);
        let c = buffer.stats.snapshot();
        assert_eq!(
            (c.refused, c.retried, c.dropped_retries_exhausted, c.pending),
            (1, 0, 0, 0)
        );
    }

    /// A slot raises the pending gauge by one and its drop lowers it again; a
    /// full buffer refuses the next slot and a delivery past it is dropped,
    /// counted, and sends nothing.
    #[tokio::test]
    async fn the_delivery_buffer_is_bounded_and_its_slots_are_released() {
        let buffer = private_buffer(2);
        let pending = || buffer.stats.pending.load(Ordering::SeqCst);
        let slot = PendingSlot::take(&buffer).expect("a slot");
        assert_eq!(pending(), 1);
        drop(slot);
        assert_eq!(pending(), 0);

        let held: Vec<PendingSlot> = std::iter::from_fn(|| PendingSlot::take(&buffer)).collect();
        assert_eq!(held.len(), 2);
        assert!(PendingSlot::take(&buffer).is_none(), "the buffer is full");
        let (base, seen) = scripted_coord(vec![200]).await;
        let record = restart_with_shadow();
        deliver_snapshot_in(
            &buffer,
            &base,
            record.action_id,
            body_for(&record),
            || async { no_bearer() },
        )
        .await;
        assert!(
            seen.lock().expect("seen").is_empty(),
            "a dropped snapshot is not sent"
        );
        assert_eq!(buffer.stats.snapshot().dropped_buffer_full, 1);
        drop(held);
        assert_eq!(pending(), 0);
        // A slot freed, the next delivery goes out and releases its own slot.
        deliver_snapshot_in(
            &buffer,
            &base,
            record.action_id,
            body_for(&record),
            || async { no_bearer() },
        )
        .await;
        assert_eq!(seen.lock().expect("seen").len(), 1);
        assert_eq!(pending(), 0);
        assert_eq!(buffer.stats.snapshot().delivered, 1);
    }

    /// `/health` reads the process's own buffer, the one production delivers
    /// through.
    #[test]
    fn ingest_counters_read_the_process_buffer() {
        assert!(std::ptr::eq(DELIVERY_BUFFER.stats, &INGEST_STATS));
        assert_eq!(DELIVERY_BUFFER.capacity, INGEST_MAX_PENDING);
        assert_eq!(ingest_counters(), INGEST_STATS.snapshot());
    }

    /// The size measure is coord's: the JSON as Postgres prints it back. The
    /// literals are coord's own fixtures (`metadata_is_measured_as_postgres_prints_it_back`).
    #[test]
    fn metadata_is_measured_the_way_coord_measures_it() {
        use serde_json::json;
        for (value, printed) in [
            (json!({}), "{}"),
            (json!([]), "[]"),
            (json!(null), "null"),
            (json!(false), "false"),
            (
                json!({"a": 1, "b": [true, null]}),
                r#"{"a": 1, "b": [true, null]}"#,
            ),
            (json!("é\"\n\u{1}"), r#""é\"\n\u0001""#),
            (json!(-5), "-5"),
            (json!(1.0), "1.0"),
            (json!(0.1), "0.1"),
            (json!(123.456), "123.456"),
            (json!(1.5e-7), "0.00000015"),
            (json!(-0.0), "0.0"),
        ] {
            assert_eq!(rendered_jsonb_len(&value), printed.len(), "{value}");
        }
        assert_eq!(rendered_jsonb_len(&json!(1e308)), 309);
        assert_eq!(rendered_jsonb_len(&json!(5e-324)), 326);
        assert_eq!(rendered_numeric_len("1E+21"), 22);
        let map = json!({"a": 1, "b": 0.00000015})
            .as_object()
            .expect("map")
            .clone();
        assert_eq!(metadata_len(&map), r#"{"a": 1, "b": 0.00000015}"#.len());
    }

    /// With a shadow present, an oversized coord basis costs the `served`
    /// block only: the small shadow is S3's evidence and survives.
    #[test]
    fn an_oversized_basis_keeps_the_shadow() {
        use crate::dev_action::record::ServedAtMint;
        let mut record = restart_with_shadow();
        record.expectations_basis = Some("b".repeat(5000));
        record.served = Some(ServedAtMint {
            predicted: vec![served_row("DEV-A", 0.4)],
            route_lkg: false,
        });
        let json = serde_json::to_value(body_for(&record)).expect("serialize");
        let metadata = json["metadata"].as_object().expect("object").clone();
        assert!(metadata.get("served").is_none());
        assert!(metadata.get("route_lkg_shadow").is_some());
        assert!(metadata_len(&metadata) <= METADATA_BUDGET_BYTES);
    }

    /// A coord basis too long to fit drops the whole `served` block rather
    /// than the trial.
    #[test]
    fn an_oversized_coord_basis_drops_the_served_block_not_the_trial() {
        use crate::dev_action::record::ServedAtMint;
        let mut record = ActionRecord::new(ActionKind::Restart, None, "x".into(), &[]);
        record.expectations_basis = Some("b".repeat(5000));
        record.served = Some(ServedAtMint {
            predicted: vec![],
            route_lkg: false,
        });
        let json = serde_json::to_value(body_for(&record)).expect("serialize");
        let metadata = json["metadata"].as_object().expect("object").clone();
        assert!(metadata.get("served").is_none());
        assert!(metadata_len(&metadata) <= METADATA_BUDGET_BYTES);
        assert_eq!(json["metadata"]["attribution_epoch"], ATTRIBUTION_EPOCH);
    }

    /// A minted record stamps what it served: the ACK rows, coord's basis, the
    /// live advice, and `use_lkg` read off the kind (`false` for a restart,
    /// `true` once re-keyed, `null` for a kind that is not a restart). A record
    /// no mint prepared stamps no `served` block.
    #[test]
    fn a_minted_record_stamps_what_it_served() {
        use crate::dev_action::record::ServedAtMint;
        let mut record = restart_with_shadow();
        record.expectations_basis = Some("marginal".to_string());
        record.served = Some(ServedAtMint {
            predicted: vec![served_row("DEV-A", 0.4), served_row("DEV-B", 0.1)],
            route_lkg: true,
        });
        let json = serde_json::to_value(body_for(&record)).expect("serialize");
        let served = &json["metadata"]["served"];
        assert_eq!(served["predicted"][0]["signature"], "DEV-A");
        assert_eq!(served["predicted"][1]["signature"], "DEV-B");
        assert_eq!(served["predicted"].as_array().map(Vec::len), Some(2));
        assert_eq!(served["predicted_truncated"], false);
        assert_eq!(served["expectations_basis"], "marginal");
        assert_eq!(served["route_lkg"], true);
        assert_eq!(served["use_lkg"], false);

        assert!(record.rekey_restart_to_lkg());
        let json = serde_json::to_value(body_for(&record)).expect("serialize");
        assert_eq!(json["metadata"]["served"]["use_lkg"], true);

        let mut spawn = ActionRecord::new(ActionKind::Spawn, None, "spawn".into(), &[]);
        spawn.served = Some(ServedAtMint {
            predicted: vec![],
            route_lkg: false,
        });
        let json = serde_json::to_value(body_for(&spawn)).expect("serialize");
        assert!(json["metadata"]["served"]["use_lkg"].is_null());
        assert!(json["metadata"]["served"]["expectations_basis"].is_null());
        assert_eq!(json["metadata"]["served"]["route_lkg"], false);

        let unminted = ActionRecord::new(ActionKind::Restart, None, "x".into(), &[]);
        let json = serde_json::to_value(body_for(&unminted)).expect("serialize");
        assert!(json["metadata"].get("served").is_none());
    }

    /// Five served rows at coord's longest signature (128 bytes) plus a shadow
    /// would pass coord's 4 KiB metadata bound; the stamp cuts served rows from
    /// the tail, says so, and stays under the budget, so the trial is kept.
    #[test]
    fn oversized_served_rows_are_cut_to_keep_the_metadata_under_budget() {
        use crate::dev_action::record::ServedAtMint;
        let mut record = restart_with_shadow();
        let long = |c: char| format!("DEV-{}", c.to_string().repeat(124));
        record.served = Some(ServedAtMint {
            predicted: ['A', 'B', 'C', 'D', 'E']
                .iter()
                .map(|c| {
                    let mut row = served_row(&long(*c), 0.123_456_789_012_345_67);
                    row.exact_trials = Some(u64::MAX);
                    row.exact_occurrences = Some(u64::MAX);
                    row.evidence_basis = "x".repeat(600);
                    row
                })
                .collect(),
            route_lkg: true,
        });
        // The input alone is over budget, so the test cannot pass vacuously.
        let untrimmed = served_block(&record).expect("served");
        assert!(rendered_jsonb_len(&untrimmed) > METADATA_BUDGET_BYTES);
        let json = serde_json::to_value(body_for(&record)).expect("serialize");
        let metadata = json["metadata"].as_object().expect("object").clone();
        assert!(metadata_len(&metadata) <= METADATA_BUDGET_BYTES);
        let served = &json["metadata"]["served"];
        assert_eq!(served["predicted_truncated"], true);
        let rows = served["predicted"].as_array().expect("rows");
        assert!(rows.len() < 5, "rows were cut: {}", rows.len());
        // The cut keeps the head of the ACK (highest posterior first).
        if let Some(first) = rows.first() {
            assert_eq!(first["signature"], long('A'));
        }
        // Coord's bound, by coord's measure.
        assert!(metadata_len(&metadata) <= 4096);
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
        post_snapshot_to_coord(&base, &record, &confirmed(), None, None, &no_sources()).await;
        let sent = std::mem::take(&mut *seen.lock().expect("seen"));
        assert_eq!(sent.len(), 2, "refused once, then re-sent: {sent:?}");
        assert_eq!(sent[0]["kind"], "restart_lkg");
        assert!(sent[0]["metadata"].get("refused_kind").is_none());
        assert_eq!(sent[1]["kind"], "restart");
        assert_eq!(sent[1]["metadata"]["refused_kind"], "restart_lkg");
        assert_eq!(sent[1]["metadata"]["route_lkg_shadow"]["use_lkg"], true);
        assert_eq!(sent[1]["action_id"], sent[0]["action_id"]);

        accepts_lkg.store(true, Ordering::SeqCst);
        post_snapshot_to_coord(&base, &record, &confirmed(), None, None, &no_sources()).await;
        let sent = std::mem::take(&mut *seen.lock().expect("seen"));
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["kind"], "restart_lkg");
    }
}

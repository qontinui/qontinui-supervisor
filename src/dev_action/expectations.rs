//! Phase-4 (supervisor side): state-conditioned action *expectations*.
//!
//! When the supervisor mints a dev action (restart / spawn-test / spawn-named /
//! fix-and-rebuild) it already evaluates the active dev-state set and stamps
//! `action_id` + `states_active` + `outcome_url` into the ACK. Phase 4 ALSO
//! fetches coord's state-conditioned expectations for this `(action_kind,
//! states)` and stamps a `predicted` array into the SAME ACK body, so the
//! initiating agent is warned *before* the action completes (e.g.
//! "LEGACY_EXE_FALLBACK active — 3/5 of restarts from this state white-screened").
//!
//! # Wire contract (coord, already built, public, no JWT)
//!
//! `GET <coord_base>/coord/dev-actions/expectations?action_kind=<kind>&state_ids=A,B,C&top_k=5`
//!
//! ```json
//! { "action_kind":"restart", "state_ids":["LEGACY_EXE_FALLBACK","SLOTS_EMPTY"],
//!   "evidence_basis":"exact",
//!   "expectations":[ {"signature":"DEV-TAURI-ASSET-MISSING","posterior_mean":0.6,
//!                     "occurrences":3,"trials":5,"evidence_basis":"exact",
//!                     "last_seen":"2026-06-07T..."} ] }
//! ```
//!
//! # Posture: best-effort, fail-open, SHORT timeout
//!
//! This runs on the **synchronous ACK path** (inside the mint helper, before the
//! action's 202/200 is returned), so the timeout MUST be short. On ANY failure
//! (unresolvable coord base, network error, non-2xx, parse error) we return an
//! EMPTY `Vec` and log a single `debug!`/`warn!`. We NEVER block beyond the
//! timeout, NEVER panic, NEVER fail the action path. An empty array is the
//! correct, honest default when coord is unreachable or has no history.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::dev_action::ingest::coord_http_base;

/// Short timeout for the outbound coord expectations GET. This is on the
/// synchronous ACK path, so it MUST be short — 2s, far below the 5s ingest
/// budget (ingest runs off the green path, this does not).
const EXPECTATIONS_TIMEOUT_SECS: u64 = 2;

/// One state-conditioned expectation, as stamped into the action ACK's
/// `predicted` array. Mirrors the per-signature shape coord returns in its
/// `expectations` array. Serializes with these exact field names so the agent
/// reading the ACK sees the coord vocabulary verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PredictedSignature {
    /// Canonical `DEV-*` signature code (e.g. `DEV-TAURI-ASSET-MISSING`).
    pub signature: String,
    /// Beta posterior mean P(signature | action_kind, states) in [0, 1].
    pub posterior_mean: f64,
    /// Times this signature was observed for this (kind, states) cell.
    pub occurrences: u64,
    /// Total trials (actions) in this (kind, states) cell.
    pub trials: u64,
    /// `"exact" | "partial" | ...` — how the cell was matched on the coord side.
    pub evidence_basis: String,
}

/// The coord `expectations` response shape. Private — mirrors the wire contract
/// just enough to extract the `expectations` array. We deliberately decode only
/// the fields we stamp (`last_seen`, top-level `action_kind`/`state_ids` are
/// ignored) so a coord-side additive change never breaks the parse.
#[derive(Debug, Deserialize)]
struct ExpectationsResponse {
    #[serde(default)]
    expectations: Vec<PredictedSignature>,
}

/// Fetch state-conditioned expectations from coord for `(action_kind,
/// state_ids)`. Best-effort + fail-open: returns an EMPTY `Vec` on any failure
/// (unresolved coord base, network, non-2xx, parse) after a single
/// `debug!`/`warn!`. NEVER blocks beyond [`EXPECTATIONS_TIMEOUT_SECS`], NEVER
/// panics. Safe to `await` directly on the synchronous ACK path.
///
/// `action_kind` is the `ActionKind::as_str()` value (`"restart" | "spawn" |
/// "build"`); `state_ids` are the canonical SCREAMING_SNAKE active-state ids
/// (the same `states_active` already stamped into the ACK); `top_k` bounds how
/// many signatures coord returns.
pub async fn fetch_expectations(
    action_kind: &str,
    state_ids: &[&str],
    top_k: usize,
) -> Vec<PredictedSignature> {
    let base = match coord_http_base() {
        Some(b) => b,
        None => {
            debug!(
                "dev_action::expectations: coord base unresolved (no COORD_HTTP_URL / \
                 profiles.json coord_url) — returning empty predicted set for kind={action_kind}"
            );
            return Vec::new();
        }
    };

    // `state_ids` is comma-joined; the canonical ids are SCREAMING_SNAKE so no
    // escaping is needed. An empty active set sends `state_ids=` (coord answers
    // with the unconditioned cell).
    let joined = state_ids.join(",");
    let url = format!(
        "{base}/coord/dev-actions/expectations?action_kind={action_kind}&state_ids={joined}&top_k={top_k}"
    );

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(EXPECTATIONS_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("dev_action::expectations: reqwest builder failed: {e}");
            return Vec::new();
        }
    };

    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                debug!(
                    "dev_action::expectations: coord returned {status} for GET {url} \
                     (non-fatal) — empty predicted set"
                );
                return Vec::new();
            }
            match resp.json::<ExpectationsResponse>().await {
                Ok(parsed) => parsed.expectations,
                Err(e) => {
                    warn!(
                        "dev_action::expectations: failed to parse coord expectations \
                         response from {url} (non-fatal): {e}"
                    );
                    Vec::new()
                }
            }
        }
        Err(e) => {
            debug!("dev_action::expectations: GET {url} failed (non-fatal, fail-open): {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PredictedSignature` serializes with the EXACT field names the agent
    /// reads off the ACK's `predicted` array — `signature`, `posterior_mean`,
    /// `occurrences`, `trials`, `evidence_basis` — matching coord's vocabulary.
    #[test]
    fn predicted_signature_serializes_with_expected_field_names() {
        let pred = PredictedSignature {
            signature: "DEV-TAURI-ASSET-MISSING".into(),
            posterior_mean: 0.6,
            occurrences: 3,
            trials: 5,
            evidence_basis: "exact".into(),
        };
        let json = serde_json::to_value(&pred).expect("serialize");
        assert_eq!(json["signature"], "DEV-TAURI-ASSET-MISSING");
        assert_eq!(json["posterior_mean"], 0.6);
        assert_eq!(json["occurrences"], 3);
        assert_eq!(json["trials"], 5);
        assert_eq!(json["evidence_basis"], "exact");
        // No stray fields beyond the five contract fields.
        assert_eq!(
            json.as_object().expect("object").len(),
            5,
            "PredictedSignature must serialize exactly the five contract fields"
        );
    }

    /// A sample coord `expectations` JSON response decodes into
    /// `Vec<PredictedSignature>`, extracting the signature + posterior_mean (and
    /// ignoring the top-level / per-row extra fields coord includes, e.g.
    /// `last_seen`). No network — pure parse test.
    #[test]
    fn parses_sample_coord_expectations_response() {
        let sample = r#"{
            "action_kind": "restart",
            "state_ids": ["LEGACY_EXE_FALLBACK", "SLOTS_EMPTY"],
            "evidence_basis": "exact",
            "expectations": [
                {
                    "signature": "DEV-TAURI-ASSET-MISSING",
                    "posterior_mean": 0.6,
                    "occurrences": 3,
                    "trials": 5,
                    "evidence_basis": "exact",
                    "last_seen": "2026-06-07T12:00:00Z"
                },
                {
                    "signature": "DEV-UI-ERROR-BOUNDARY",
                    "posterior_mean": 0.2,
                    "occurrences": 1,
                    "trials": 5,
                    "evidence_basis": "partial",
                    "last_seen": "2026-06-06T09:30:00Z"
                }
            ]
        }"#;

        let parsed: ExpectationsResponse =
            serde_json::from_str(sample).expect("decode coord expectations response");
        assert_eq!(parsed.expectations.len(), 2);

        let first = &parsed.expectations[0];
        assert_eq!(first.signature, "DEV-TAURI-ASSET-MISSING");
        assert_eq!(first.posterior_mean, 0.6);
        assert_eq!(first.occurrences, 3);
        assert_eq!(first.trials, 5);
        assert_eq!(first.evidence_basis, "exact");

        let second = &parsed.expectations[1];
        assert_eq!(second.signature, "DEV-UI-ERROR-BOUNDARY");
        assert_eq!(second.posterior_mean, 0.2);
        assert_eq!(second.evidence_basis, "partial");
    }

    /// A response with no `expectations` key (or an empty array) decodes to an
    /// empty Vec — the honest "no history" default, never an error.
    #[test]
    fn parses_empty_and_missing_expectations() {
        let empty_array = r#"{ "action_kind": "spawn", "state_ids": [], "expectations": [] }"#;
        let parsed: ExpectationsResponse = serde_json::from_str(empty_array).expect("decode empty");
        assert!(parsed.expectations.is_empty());

        let missing_key = r#"{ "action_kind": "spawn", "state_ids": [] }"#;
        let parsed: ExpectationsResponse =
            serde_json::from_str(missing_key).expect("decode missing key");
        assert!(parsed.expectations.is_empty());
    }
}

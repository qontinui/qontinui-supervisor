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
//! # Wire contract (coord, public; the device bearer is observed, never required)
//!
//! `GET <coord_base>/coord/dev-actions/expectations?action_kind=<kind>&state_ids=A,B,C&top_k=100`
//!
//! ```json
//! { "action_kind":"restart", "state_ids":["LEGACY_EXE_FALLBACK","SLOTS_EMPTY"],
//!   "evidence_basis":"exact",
//!   "expectations":[ {"signature":"DEV-TAURI-ASSET-MISSING","posterior_mean":0.6,
//!                     "occurrences":3,"trials":5,"evidence_basis":"exact",
//!                     "last_seen":"2026-06-07T..."} ] }
//! ```
//!
//! The fetch asks for coord's top 100 rows, its ceiling ([`RISK_FETCH_TOP_K`]),
//! and no caller chooses fewer. [`assess_then_cut`] hands that whole fetch to
//! the risk assessment ([`crate::dev_action::policy`]) and only then cuts the
//! ACK's `predicted` to the top [`ACK_PREDICTED_TOP_K`] by `posterior_mean` —
//! the same list a `top_k=5` fetch returns.
//!
//! # Posture: best-effort, fail-open, SHORT timeout
//!
//! This runs on the **synchronous ACK path** (inside the mint helper, before the
//! action's 202/200 is returned), so the timeout MUST be short. On ANY failure
//! (unresolvable coord base, network error, non-2xx, parse error) we return no
//! rows and no basis, and log a single `debug!`/`warn!`. We NEVER block beyond the
//! timeout, NEVER panic, NEVER fail the action path. An empty array is the
//! correct, honest default when coord is unreachable or has no history.
//!
//! # The device bearer
//!
//! The GET carries this box's coord device JWT when one resolves (plan
//! `2026-10-27-bayesian-consequence-model-for-the-dev-twin` Phase 5).
//! coord observes it and never requires it (that plan's Phase 4: route
//! `dev_actions_expectations` of `coord_data_plane_auth_requests_total`), so
//! an absent or refused bearer changes nothing a caller reads; what it buys is
//! the evidence Phase 7 needs before the anonymous arms can close. The caller
//! resolves it ([`resolve_expectations_bearer`]) under its own short budget,
//! because the cascade's last rung is a runner mint whose timeout is longer
//! than this whole fetch's.
//!
//! An empty array alone cannot say WHICH of those it is, and neither can the
//! `risk: null` it folds to: coord being down and coord having no rows read
//! alike, and the LKG restart guard fails open on both. So the fetch also keeps
//! coord's top-level `evidence_basis` ([`FetchedExpectations::basis`]), and the
//! ACK serves it as `expectations_basis`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::dev_action::policy::{assess_action_risk, ActionRisk};
use crate::fleet::{DeviceSources, ResolvedBearer};

/// Short timeout for the outbound coord expectations GET. This is on the
/// synchronous ACK path, so it MUST be short — 2s, far below the 5s ingest
/// budget (ingest runs off the green path, this does not).
#[cfg(not(test))]
const EXPECTATIONS_TIMEOUT_SECS: u64 = 2;

#[cfg(not(test))]
fn expectations_timeout() -> Duration {
    Duration::from_secs(EXPECTATIONS_TIMEOUT_SECS)
}

/// A test's coord is a planted loopback listener, so the ACK-path budget would
/// measure the box's load rather than the code.
#[cfg(test)]
fn expectations_timeout() -> Duration {
    crate::test_clock::poll_budget(Duration::from_secs(30))
}

/// How long [`resolve_expectations_bearer`] may spend on the device-JWT
/// cascade before the fetch goes out anonymously. The seeded rungs are file
/// and env reads; only the runner mint can take long, and its own timeout
/// (3 s) is longer than the whole ACK-path fetch.
pub const EXPECTATIONS_BEARER_BUDGET: Duration = Duration::from_millis(750);

/// Source label when [`EXPECTATIONS_BEARER_BUDGET`] ran out before a bearer
/// resolved: the fetch is sent anonymously, which coord still serves.
pub const SOURCE_BUDGET_SPENT: &str =
    "none (the device-JWT cascade outlasted the expectations bearer budget)";

/// The device bearer for an expectations fetch to `base`, resolved through
/// [`crate::fleet::resolve_device_bearer_cached_for`] (transport guard first) within
/// [`EXPECTATIONS_BEARER_BUDGET`]. Never fails: no base, no credential, a
/// withheld transport or a spent budget all resolve to no token with the
/// reason as its label, and coord serves an anonymous fetch exactly as before.
pub(crate) async fn resolve_expectations_bearer(
    base: Option<&str>,
    sources: &DeviceSources,
) -> ResolvedBearer {
    let Some(base) = base else {
        return (None, crate::fleet::SOURCE_NONE);
    };
    match tokio::time::timeout(
        EXPECTATIONS_BEARER_BUDGET,
        crate::fleet::resolve_device_bearer_cached_for(base, sources),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(_) => {
            debug!("dev_action::expectations: {SOURCE_BUDGET_SPENT}");
            (None, SOURCE_BUDGET_SPENT)
        }
    }
}

/// `top_k` for the fetch the risk assessment reads: coord's ceiling (its
/// `MAX_SERVED_ROWS`). coord ranks rows by its pooled `posterior_mean` before
/// cutting to `top_k`, and the boot-failure row the exact-cell arm of
/// [`crate::dev_action::policy`] exists for is the one whose pooled mean lags,
/// so a smaller `top_k` could drop it before the policy sees it.
pub const RISK_FETCH_TOP_K: usize = 100;

/// Rows the ACK's `predicted` array carries: the top by `posterior_mean`.
pub const ACK_PREDICTED_TOP_K: usize = 5;

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
    /// Times this signature was observed in the cell `evidence_basis` names
    /// (the exact state set, the active state with the most trials, or
    /// global). coord's earlier reader summed a `marginal` row's counts over
    /// the active states instead.
    pub occurrences: u64,
    /// Total trials (actions) in the cell `evidence_basis` names (the exact
    /// state set, the active state with the most trials, or global). coord's
    /// earlier reader summed a `marginal` row's trials over the active states
    /// the signature fired in.
    pub trials: u64,
    /// The cell that dominates the row on the coord side: `"exact"`,
    /// `"marginal"` or `"prior_only"` (coord's top-level `"quarantined"` never
    /// reaches a row: a quarantined kind's list is empty).
    pub evidence_basis: String,
    /// The exact active-state cell's OWN trials and occurrences. coord's
    /// `hierarchical-eb-v1` reader serves them beside its shrunk
    /// `posterior_mean` (which pools the cell toward the kind's other state
    /// sets); [`crate::dev_action::policy`] reads them so that pooling can
    /// neither delay the LKG bias nor, once the set has enough trials of its
    /// own, trip it against a healthy record. `None` when coord predates them —
    /// then absent from the ACK too, rather than a `0` that would read as
    /// "never seen".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_trials: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_occurrences: Option<u64>,
}

/// The coord `expectations` response shape. Private — mirrors the wire contract
/// just enough to extract the `expectations` array and its top-level basis. We
/// deliberately decode only the fields we stamp (`last_seen`, top-level `action_kind`/`state_ids` are
/// ignored) so a coord-side additive change never breaks the parse.
#[derive(Debug, Deserialize)]
struct ExpectationsResponse {
    #[serde(default)]
    expectations: Vec<PredictedSignature>,
    #[serde(default)]
    evidence_basis: Option<String>,
}

/// One expectations read: coord's rows, and the basis it says they rest on.
#[derive(Debug, Default)]
pub struct FetchedExpectations {
    /// Every row coord served, uncut.
    pub rows: Vec<PredictedSignature>,
    /// coord's top-level `evidence_basis`, verbatim: the WEAKEST basis among all
    /// of [`Self::rows`], not only the five the ACK keeps. `prior_only` with no
    /// rows means no trials yet, or trials that never emitted a signature.
    ///
    /// It is not parsed into an enum, so a value coord adds (plan
    /// `2026-11-03-supervisor-dev-action-residuals-coord-resolvers-restart-door-and-pre-fix-ledger-rows`
    /// D3's `re-accruing`) reaches the ACK without a supervisor change. It decides
    /// nothing here: [`crate::dev_action::policy`] reads the rows themselves.
    ///
    /// `None` is UNKNOWN: coord gave no answer (none configured, unreachable,
    /// non-2xx, unparseable) or predates the field.
    pub basis: Option<String>,
}

/// Fetch state-conditioned expectations from the coord at `base` for
/// `(action_kind, state_ids)`. `base` comes from
/// [`crate::coord_client::coord_base`]; `None` makes no request.
/// Best-effort and fail-open: returns no rows and no basis on any failure (no
/// base, network, non-2xx, parse) after a single `debug!`/`warn!`. NEVER blocks beyond
/// `EXPECTATIONS_TIMEOUT_SECS`, NEVER panics. Safe to `await` directly on the
/// synchronous ACK path.
///
/// `action_kind` is the `ActionKind::as_str()` value (`"restart" | "spawn" |
/// "build"`); `state_ids` are the canonical SCREAMING_SNAKE active-state ids
/// (the same `states_active` already stamped into the ACK). It takes no
/// `top_k`: `expectations_url` always asks for [`RISK_FETCH_TOP_K`] rows.
///
/// `bearer` is attached when it carries a token
/// ([`crate::fleet::attach_bearer_quietly`]); resolve it with
/// [`resolve_expectations_bearer`].
pub async fn fetch_expectations(
    base: Option<&str>,
    bearer: ResolvedBearer,
    action_kind: &str,
    state_ids: &[&str],
) -> FetchedExpectations {
    let Some(base) = base else {
        debug!(
            "dev_action::expectations: no coord base — returning empty predicted set for \
             kind={action_kind}"
        );
        return FetchedExpectations::default();
    };

    let url = expectations_url(base, action_kind, state_ids);

    let client = match reqwest::Client::builder()
        .timeout(expectations_timeout())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("dev_action::expectations: reqwest builder failed: {e}");
            return FetchedExpectations::default();
        }
    };

    match crate::fleet::attach_bearer_quietly(client.get(&url), bearer)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                debug!(
                    "dev_action::expectations: coord returned {status} for GET {url} \
                     (non-fatal) — empty predicted set"
                );
                return FetchedExpectations::default();
            }
            match resp.json::<ExpectationsResponse>().await {
                Ok(parsed) => FetchedExpectations {
                    rows: parsed.expectations,
                    basis: parsed.evidence_basis,
                },
                Err(e) => {
                    warn!(
                        "dev_action::expectations: failed to parse coord expectations \
                         response from {url} (non-fatal): {e}"
                    );
                    FetchedExpectations::default()
                }
            }
        }
        Err(e) => {
            debug!("dev_action::expectations: GET {url} failed (non-fatal, fail-open): {e}");
            FetchedExpectations::default()
        }
    }
}

/// The expectations GET for `(action_kind, state_ids)` against coord's `base`.
/// It always asks for [`RISK_FETCH_TOP_K`] rows, so no call site can fetch
/// fewer than the risk assessment needs.
fn expectations_url(base: &str, action_kind: &str, state_ids: &[&str]) -> String {
    // `state_ids` is comma-joined; the canonical ids are SCREAMING_SNAKE so no
    // escaping is needed. An empty active set sends `state_ids=` (coord answers
    // with the unconditioned cell).
    let joined = state_ids.join(",");
    format!(
        "{base}/coord/dev-actions/expectations?action_kind={action_kind}&state_ids={joined}&top_k={RISK_FETCH_TOP_K}"
    )
}

/// The mint flow's `(predicted, risk)` from one fetch: `risk` is
/// [`assess_action_risk`] over every fetched row, and `predicted` is the ACK's
/// cut (`ack_predicted`), taken only after. Cutting first could drop the
/// boot row the exact-cell arm reads (coord's pooled mean can rank it low),
/// so the one mint path (`prepare_action`) comes through here rather than
/// order the two itself.
pub fn assess_then_cut(
    fetched: Vec<PredictedSignature>,
    states_active: &[&str],
) -> (Vec<PredictedSignature>, ActionRisk) {
    let risk = assess_action_risk(&fetched, states_active);
    (ack_predicted(fetched), risk)
}

/// The ACK's `predicted` array from the whole fetch: the
/// [`ACK_PREDICTED_TOP_K`] rows with the highest `posterior_mean`, signature
/// ascending on ties (coord's own order, so this is the list a `top_k=5`
/// fetch returns, whatever order the rows arrived in).
fn ack_predicted(mut predicted: Vec<PredictedSignature>) -> Vec<PredictedSignature> {
    predicted.sort_by(|a, b| {
        b.posterior_mean
            .total_cmp(&a.posterior_mean)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    predicted.truncate(ACK_PREDICTED_TOP_K);
    predicted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_bearer() -> ResolvedBearer {
        (None, crate::fleet::SOURCE_NONE)
    }

    /// The fetch carries the device bearer it was handed, and a fetch handed
    /// none sends no `Authorization` header at all (coord counts it
    /// `anonymous`, never `invalid_token`).
    #[tokio::test]
    async fn the_fetch_sends_the_device_bearer_it_was_handed_and_none_without_one() {
        let coord = crate::dev_action::test_support::serve_planted_answer(serde_json::json!({
            "evidence_basis": "prior_only",
            "expectations": [],
        }))
        .await;
        let fetched = fetch_expectations(
            Some(&coord.base),
            (Some("header.payload.sig".to_string()), "test"),
            "restart",
            &[],
        )
        .await;
        assert_eq!(fetched.basis.as_deref(), Some("prior_only"));
        let fetched = fetch_expectations(Some(&coord.base), no_bearer(), "restart", &[]).await;
        assert_eq!(fetched.basis.as_deref(), Some("prior_only"));
        let auth: Vec<Option<String>> = coord.seen().into_iter().map(|r| r.authorization).collect();
        assert_eq!(
            auth,
            [Some("Bearer header.payload.sig".to_string()), None],
            "first fetch authed, second anonymous"
        );
    }

    /// No base, or a base the transport guard refuses, resolves no bearer and
    /// reads nothing: a cleartext non-loopback coord never gets the JWT.
    /// Sources that read no identity (a planted or ignored coord) resolve no
    /// bearer even over https, so the cache can never hand one this box's token.
    #[tokio::test]
    async fn sources_that_read_no_identity_resolve_no_bearer_over_https() {
        for access in [
            crate::config::AmbientCoord::Ignore,
            crate::config::AmbientCoord::At("https://coord.example.com".into()),
        ] {
            let sources = DeviceSources::for_access(&access);
            let (token, label) =
                resolve_expectations_bearer(Some("https://coord.example.com"), &sources).await;
            assert!(token.is_none(), "{access:?}");
            assert_eq!(label, crate::fleet::SOURCE_NONE, "{access:?}");
        }
    }

    #[tokio::test]
    async fn no_base_or_a_cleartext_remote_base_resolves_no_bearer() {
        let sources = DeviceSources::fixture("http://127.0.0.1:1");
        let (token, label) = resolve_expectations_bearer(None, &sources).await;
        assert!(token.is_none());
        assert_eq!(label, crate::fleet::SOURCE_NONE);
        let (token, label) =
            resolve_expectations_bearer(Some("http://coord.example.com"), &sources).await;
        assert!(token.is_none());
        assert_eq!(label, crate::fleet::SOURCE_WITHHELD);
    }

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
            exact_trials: None,
            exact_occurrences: None,
        };
        let json = serde_json::to_value(&pred).expect("serialize");
        assert_eq!(json["signature"], "DEV-TAURI-ASSET-MISSING");
        assert_eq!(json["posterior_mean"], 0.6);
        assert_eq!(json["occurrences"], 3);
        assert_eq!(json["trials"], 5);
        assert_eq!(json["evidence_basis"], "exact");
        // No stray fields beyond the five contract fields (the exact-cell
        // counts appear only when coord sent them).
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
        // A coord that predates the exact-cell counts leaves them unset.
        assert_eq!((first.exact_trials, first.exact_occurrences), (None, None));
    }

    /// coord's `hierarchical-eb-v1` row carries the exact cell's own counts
    /// beside the shrunk mean (plus fields the supervisor ignores); they
    /// decode, and they ride into the ACK only when coord sent them.
    #[test]
    fn decodes_the_exact_cell_counts_of_a_hierarchical_eb_row() {
        let sample = r#"{
            "action_kind": "restart",
            "estimator": "hierarchical-eb-v1",
            "expectations": [{
                "signature": "DEV-TAURI-ASSET-MISSING",
                "posterior_mean": 0.2345,
                "occurrences": 5,
                "trials": 12,
                "evidence_basis": "marginal",
                "ci_low": 0.05,
                "ci_high": 0.61,
                "exact_trials": 5,
                "exact_occurrences": 5,
                "kappa_exact": 64.0
            }]
        }"#;
        let parsed: ExpectationsResponse = serde_json::from_str(sample).expect("decode");
        let row = &parsed.expectations[0];
        assert_eq!(
            (row.exact_trials, row.exact_occurrences),
            (Some(5), Some(5))
        );
        let json = serde_json::to_value(row).expect("serialize");
        assert_eq!(json["exact_trials"], 5);
        assert_eq!(json["exact_occurrences"], 5);
        assert_eq!(json.as_object().expect("object").len(), 7);
    }

    /// The fetch asks coord for its ceiling, never the ACK's five: coord ranks
    /// by its pooled mean before cutting to `top_k`, so fewer rows could drop
    /// the boot row the exact-cell arm reads before the policy sees it. The
    /// URL is the only place the fetch size is set; no caller passes one.
    #[test]
    fn the_fetch_asks_coord_for_its_top_100_rows() {
        assert_eq!(
            expectations_url(
                "https://coord.example",
                "restart",
                &["LEGACY_EXE_FALLBACK", "SLOTS_EMPTY"]
            ),
            "https://coord.example/coord/dev-actions/expectations\
             ?action_kind=restart&state_ids=LEGACY_EXE_FALLBACK,SLOTS_EMPTY&top_k=100"
        );
        // An empty active set sends `state_ids=` (the unconditioned cell).
        assert_eq!(
            expectations_url("https://coord.example", "spawn", &[]),
            "https://coord.example/coord/dev-actions/expectations\
             ?action_kind=spawn&state_ids=&top_k=100"
        );
    }

    /// The mint path's `risk` reads the whole fetch and its ACK keeps the
    /// top five by `posterior_mean` (signature ascending on ties), whatever
    /// order the rows arrived in. coord's pooled mean can rank the incident row
    /// sixth — 4 of 4 from this exact state set, below five other signatures —
    /// and it must still trip the exact-cell arm and be echoed in `risk`,
    /// though the ACK's five leave it out.
    #[test]
    fn the_risk_reads_the_whole_fetch_and_the_ack_keeps_the_top_five() {
        let row = |sig: &str, posterior_mean: f64, exact: Option<(u64, u64)>| PredictedSignature {
            signature: sig.to_string(),
            posterior_mean,
            occurrences: 1,
            trials: 10,
            evidence_basis: "marginal".to_string(),
            exact_trials: exact.map(|(_, n)| n),
            exact_occurrences: exact.map(|(y, _)| y),
        };
        let fetched = vec![
            row("DEV-B", 0.29, None),
            row("DEV-TAURI-ASSET-MISSING", 0.2, Some((4, 4))),
            row("DEV-E", 0.29, None),
            row("DEV-A", 0.29, None),
            row("DEV-D", 0.29, None),
            row("DEV-C", 0.29, None),
        ];
        let (predicted, risk) = assess_then_cut(fetched, &["SLOTS_EMPTY"]);
        assert!(risk.route_lkg, "the sixth-ranked boot row must still trip");
        assert_eq!(
            risk.top_signature.map(|p| p.signature).as_deref(),
            Some("DEV-TAURI-ASSET-MISSING")
        );
        let ack: Vec<String> = predicted.into_iter().map(|p| p.signature).collect();
        assert_eq!(ack, ["DEV-A", "DEV-B", "DEV-C", "DEV-D", "DEV-E"]);
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

    /// coord's top-level basis comes through verbatim beside an empty list, so
    /// a basis this build has never heard of still reaches the ACK.
    #[tokio::test]
    async fn the_fetch_keeps_coords_top_level_basis_verbatim() {
        let coord = crate::dev_action::test_support::serve_planted_answer(serde_json::json!({
            "evidence_basis": "re-accruing",
            "expectations": [],
        }))
        .await;
        let fetched = fetch_expectations(Some(&coord.base), no_bearer(), "restart", &[]).await;
        assert!(fetched.rows.is_empty());
        assert_eq!(fetched.basis.as_deref(), Some("re-accruing"));
    }

    /// A coord that predates the field still serves its rows, and its basis
    /// reads UNKNOWN rather than a default.
    #[tokio::test]
    async fn a_coord_with_no_top_level_basis_keeps_its_rows_and_reads_unknown() {
        let coord = crate::dev_action::test_support::serve_planted_answer(serde_json::json!({
            "expectations": [{
                "signature": "DEV-X",
                "posterior_mean": 0.1,
                "occurrences": 1,
                "trials": 10,
                "evidence_basis": "marginal",
            }],
        }))
        .await;
        let fetched = fetch_expectations(Some(&coord.base), no_bearer(), "restart", &[]).await;
        assert_eq!(fetched.rows.len(), 1);
        assert_eq!(fetched.basis, None);
    }

    /// Every way of getting no answer reads UNKNOWN: no coord configured, a
    /// non-2xx, an unparseable body, and a refused connection. None of them may
    /// read as a basis, or an outage would look like an answer.
    #[tokio::test]
    async fn no_answer_from_coord_has_no_basis() {
        use axum::http::StatusCode;
        let fetched = fetch_expectations(None, no_bearer(), "restart", &[]).await;
        assert!(fetched.rows.is_empty());
        assert_eq!(fetched.basis, None);

        let refused = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let base = format!("http://{}", listener.local_addr().expect("addr"));
            drop(listener);
            base
        };
        // The 500 carries a well-formed answer, so it is the status check that
        // refuses it, not the parse.
        let failing = crate::dev_action::test_support::serve_planted_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"evidence_basis":"exact","expectations":[]}"#,
        )
        .await;
        let garbled =
            crate::dev_action::test_support::serve_planted_status(StatusCode::OK, "not json").await;
        for base in [refused, failing, garbled] {
            let fetched = fetch_expectations(Some(&base), no_bearer(), "restart", &[]).await;
            assert!(fetched.rows.is_empty(), "{base}");
            assert_eq!(fetched.basis, None, "{base}");
        }
    }
}

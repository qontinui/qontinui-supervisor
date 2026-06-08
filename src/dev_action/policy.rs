//! Act on state-conditioned predictions — the "do something with the forecast"
//! layer (the next step after Phase 4 RECORDS / PREDICTS).
//!
//! Phases 1-5 made the supervisor *record* every dev action's state→outcome,
//! *predict* per-`(kind, states)` outcomes (coord's `predicted[]` in the ACK,
//! [`crate::dev_action::expectations::fetch_expectations`]), and *mock-replay*
//! that corpus ([`crate::dev_action::mock`]). NOTHING acted on the prediction.
//! This module is the pure decision core that the restart/spawn mint flow calls
//! so the supervisor finally REACTS to a high-risk forecast — warn loudly and,
//! for the primary restart, bias toward the LKG path instead of white-screening
//! (the 2026-06-07 `{SLOTS_EMPTY, LEGACY_EXE_FALLBACK}` incident).
//!
//! # Conservative + non-breaking
//!
//! [`assess_action_risk`] is pure (no I/O, no `SharedState`) and FAIL-OPEN: an
//! empty / low-confidence / non-boot-class prediction set returns
//! [`ActionRisk::none`] — the common healthy case leaves behavior unchanged.
//! It NEVER refuses an action; it only advises (`route_lkg`) and narrates
//! (`warn`). The caller decides how to use the advice — and an explicit operator
//! opt-out (an exe/provenance selector on the request) always wins over the
//! `route_lkg` bias. A biased-then-warned restart is recoverable; a refused one
//! could strand the operator, so we never refuse.
//!
//! # What counts as "high risk"
//!
//! A boot/asset/webview-failure-class `DEV-*` signature (the white-screen
//! family — see [`is_boot_failure_signature`]) whose coord posterior_mean is at
//! or above [`RISK_POSTERIOR_THRESHOLD`] AND whose evidence rests on at least
//! [`RISK_MIN_TRIALS`] trials. The trials floor keeps a single unlucky sample
//! (1/1 = posterior 1.0) from tripping the bias — we only act when the recorded
//! cell has enough history to mean something.

use crate::dev_action::expectations::PredictedSignature;

/// Posterior-mean threshold at or above which a boot-failure-class signature is
/// treated as high-risk. Tunable; 0.5 = "history says this state set fails the
/// boot/asset/webview check at least half the time."
pub const RISK_POSTERIOR_THRESHOLD: f64 = 0.5;

/// Minimum trials backing a prediction before it can trip the risk bias. Guards
/// against a single unlucky sample (1/1 ⇒ posterior 1.0) auto-routing to LKG on
/// one data point. Small by design — a handful of recorded restarts from a state
/// set is enough signal to bias-and-warn (we never refuse, so the cost of a
/// false positive is just an LKG-pinned restart + a warning).
pub const RISK_MIN_TRIALS: u64 = 3;

/// The boot/asset/webview-failure-class `DEV-*` signatures — the white-screen
/// family the LKG route exists to prevent. A prediction for any of these, with
/// enough confidence + trials, is what flips `route_lkg`. Kept as an explicit
/// allowlist (not a substring heuristic) so an unrelated `DEV-*` signature can
/// never silently trip the bias; extend deliberately as the vocabulary grows.
pub const BOOT_FAILURE_SIGNATURES: &[&str] = &[
    "DEV-TAURI-ASSET-MISSING",
    "DEV-WEBVIEW-CONN-REFUSED",
    "DEV-PANIC-STARTUP",
];

/// Whether a signature code is in the boot/asset/webview-failure class.
pub fn is_boot_failure_signature(signature: &str) -> bool {
    BOOT_FAILURE_SIGNATURES.contains(&signature)
}

/// Whether one prediction is high-risk: a boot-failure-class signature, at or
/// above the posterior threshold, backed by at least the trials floor.
fn is_high_risk(pred: &PredictedSignature) -> bool {
    is_boot_failure_signature(&pred.signature)
        && pred.posterior_mean >= RISK_POSTERIOR_THRESHOLD
        && pred.trials >= RISK_MIN_TRIALS
}

/// The assessment the policy returns for an action — pure advice the mint flow
/// stamps into the ACK and (for the primary restart) uses to bias the exe pick.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRisk {
    /// Advise routing the action to the last-known-good binary instead of the
    /// freshest slot exe. `true` only when a boot-failure-class prediction
    /// cleared the threshold + trials floor. The caller may still override this
    /// when the operator explicitly opted out (provenance/exe selector).
    pub route_lkg: bool,
    /// A loud, human-readable warning to log (`tracing::warn!`) and stamp into
    /// the ACK so the initiator sees the risk BEFORE the outcome lands. `None`
    /// when there is no high-risk prediction (the healthy / no-history case).
    pub warn: Option<String>,
    /// The single highest-posterior boot-failure-class prediction that drove the
    /// assessment, echoed so the ACK / logs can show the exact evidence
    /// (signature + occurrences/trials). `None` when `route_lkg` is `false`.
    pub top_signature: Option<PredictedSignature>,
}

impl ActionRisk {
    /// The no-action assessment: proceed unchanged, no warning. The fail-open
    /// default for an empty / low-confidence / non-boot-class prediction set.
    pub fn none() -> Self {
        Self {
            route_lkg: false,
            warn: None,
            top_signature: None,
        }
    }

    /// Whether this assessment carries any signal (warn or route advice).
    pub fn is_actionable(&self) -> bool {
        self.route_lkg || self.warn.is_some()
    }
}

/// Assess action risk from coord's state-conditioned predictions + the active
/// dev-state set. PURE + FAIL-OPEN.
///
/// Returns [`ActionRisk::none`] when `predicted` is empty or no prediction is
/// high-risk (the common healthy case → behavior unchanged). When at least one
/// boot/asset/webview-failure-class signature clears
/// [`RISK_POSTERIOR_THRESHOLD`] with `trials >= `[`RISK_MIN_TRIALS`], returns an
/// assessment with `route_lkg = true`, a loud `warn` naming the top signature +
/// its evidence + the active states, and `top_signature` echoing that
/// prediction.
///
/// The "top" signature is the highest-posterior high-risk prediction (ties
/// broken by trials, then occurrences) — the strongest evidence in the cell.
pub fn assess_action_risk(predicted: &[PredictedSignature], states_active: &[&str]) -> ActionRisk {
    // Pick the strongest high-risk prediction. Compare by posterior, then by
    // trials, then by occurrences so the "loudest, best-evidenced" one wins.
    let top = predicted.iter().filter(|p| is_high_risk(p)).max_by(|a, b| {
        a.posterior_mean
            .total_cmp(&b.posterior_mean)
            .then_with(|| a.trials.cmp(&b.trials))
            .then_with(|| a.occurrences.cmp(&b.occurrences))
    });

    let Some(top) = top else {
        return ActionRisk::none();
    };

    let states_desc = if states_active.is_empty() {
        "no active dev-states".to_string()
    } else {
        states_active.join(", ")
    };
    let warn = format!(
        "HIGH-RISK dev action: history predicts boot-failure signature {} \
         (posterior {:.2}, {}/{} trials, evidence={}) under active states [{}]. \
         Biasing toward the last-known-good binary to avoid a white-screen; \
         pass an explicit exe/provenance selector to override.",
        top.signature,
        top.posterior_mean,
        top.occurrences,
        top.trials,
        top.evidence_basis,
        states_desc,
    );

    ActionRisk {
        route_lkg: true,
        warn: Some(warn),
        top_signature: Some(top.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(sig: &str, posterior: f64, occ: u64, trials: u64) -> PredictedSignature {
        PredictedSignature {
            signature: sig.to_string(),
            posterior_mean: posterior,
            occurrences: occ,
            trials,
            evidence_basis: "exact".to_string(),
        }
    }

    const INCIDENT_STATES: &[&str] = &["SLOTS_EMPTY", "LEGACY_EXE_FALLBACK"];

    /// The 2026-06-07 incident cell: a boot-failure signature above threshold
    /// with enough trials ⇒ route to LKG + a loud warning naming the evidence.
    #[test]
    fn high_risk_boot_failure_routes_to_lkg_and_warns() {
        let predicted = vec![pred("DEV-TAURI-ASSET-MISSING", 0.6, 3, 5)];
        let risk = assess_action_risk(&predicted, INCIDENT_STATES);
        assert!(
            risk.route_lkg,
            "boot-failure above threshold must route_lkg"
        );
        assert!(risk.is_actionable());
        let top = risk.top_signature.clone().expect("top_signature echoed");
        let warn = risk
            .warn
            .expect("a high-risk assessment must carry a warning");
        assert!(
            warn.contains("DEV-TAURI-ASSET-MISSING"),
            "warn names signature"
        );
        assert!(warn.contains("SLOTS_EMPTY"), "warn names the active states");
        assert!(warn.contains("last-known-good"), "warn explains the bias");
        assert_eq!(top.signature, "DEV-TAURI-ASSET-MISSING");
    }

    /// Empty prediction set (coord unreachable / no history) ⇒ no action,
    /// unchanged behavior — the fail-open healthy default.
    #[test]
    fn empty_predictions_is_no_action() {
        let risk = assess_action_risk(&[], INCIDENT_STATES);
        assert_eq!(risk, ActionRisk::none());
        assert!(!risk.route_lkg);
        assert!(risk.warn.is_none());
        assert!(risk.top_signature.is_none());
        assert!(!risk.is_actionable());
    }

    /// A healthy cell — predictions exist but none are boot-failure class ⇒ no
    /// action (a non-boot signature, however confident, never trips the bias).
    #[test]
    fn non_boot_signature_never_trips_even_when_confident() {
        let predicted = vec![
            pred("DEV-UI-ERROR-BOUNDARY", 0.95, 19, 20),
            pred("DEV-SLOW-START", 0.8, 8, 10),
        ];
        let risk = assess_action_risk(&predicted, INCIDENT_STATES);
        assert_eq!(risk, ActionRisk::none());
    }

    /// Threshold gating: a boot-failure signature just BELOW the posterior
    /// threshold (but with ample trials) does NOT trip; at/above it does.
    #[test]
    fn posterior_threshold_gates_the_bias() {
        // Below threshold ⇒ no action.
        let below = vec![pred("DEV-WEBVIEW-CONN-REFUSED", 0.49, 49, 100)];
        assert_eq!(
            assess_action_risk(&below, INCIDENT_STATES),
            ActionRisk::none()
        );

        // Exactly at threshold ⇒ trips (>=).
        let at = vec![pred("DEV-WEBVIEW-CONN-REFUSED", 0.50, 50, 100)];
        assert!(assess_action_risk(&at, INCIDENT_STATES).route_lkg);

        // Above threshold ⇒ trips.
        let above = vec![pred("DEV-WEBVIEW-CONN-REFUSED", 0.51, 51, 100)];
        assert!(assess_action_risk(&above, INCIDENT_STATES).route_lkg);
    }

    /// Trials gating: a confident boot-failure prediction backed by too few
    /// trials (1/1 = posterior 1.0) does NOT trip — a single unlucky sample
    /// must not auto-route. At the trials floor it does.
    #[test]
    fn trials_floor_gates_a_single_unlucky_sample() {
        // 1 trial, posterior 1.0 — below the trials floor ⇒ no action.
        let one_shot = vec![pred("DEV-PANIC-STARTUP", 1.0, 1, 1)];
        assert_eq!(
            assess_action_risk(&one_shot, INCIDENT_STATES),
            ActionRisk::none(),
            "a single unlucky sample must not trip the bias"
        );

        // Exactly RISK_MIN_TRIALS trials, above threshold ⇒ trips.
        let at_floor = vec![pred("DEV-PANIC-STARTUP", 0.67, 2, RISK_MIN_TRIALS)];
        assert!(assess_action_risk(&at_floor, INCIDENT_STATES).route_lkg);
    }

    /// When several high-risk predictions are present, the strongest (highest
    /// posterior, then trials, then occurrences) is chosen as `top_signature`.
    #[test]
    fn strongest_high_risk_prediction_wins_as_top() {
        let predicted = vec![
            pred("DEV-WEBVIEW-CONN-REFUSED", 0.55, 11, 20),
            pred("DEV-TAURI-ASSET-MISSING", 0.80, 16, 20),
            pred("DEV-PANIC-STARTUP", 0.60, 12, 20),
            // A confident non-boot signature is ignored entirely.
            pred("DEV-UI-ERROR-BOUNDARY", 0.99, 99, 100),
        ];
        let risk = assess_action_risk(&predicted, INCIDENT_STATES);
        assert!(risk.route_lkg);
        let top = risk.top_signature.expect("top");
        assert_eq!(
            top.signature, "DEV-TAURI-ASSET-MISSING",
            "highest-posterior boot-failure signature must win"
        );
    }

    /// `is_boot_failure_signature` is an exact allowlist match — an unrelated
    /// signature, even a near-miss substring, is not boot-failure class.
    #[test]
    fn boot_failure_classification_is_exact_allowlist() {
        assert!(is_boot_failure_signature("DEV-TAURI-ASSET-MISSING"));
        assert!(is_boot_failure_signature("DEV-WEBVIEW-CONN-REFUSED"));
        assert!(is_boot_failure_signature("DEV-PANIC-STARTUP"));
        assert!(!is_boot_failure_signature("DEV-TAURI-ASSET-MISSING-EXTRA"));
        assert!(!is_boot_failure_signature("DEV-UI-ERROR-BOUNDARY"));
        assert!(!is_boot_failure_signature(""));
    }

    /// Empty active-states still produces a coherent warning (the prediction
    /// can be unconditioned) — the warn names "no active dev-states".
    #[test]
    fn high_risk_with_empty_states_still_warns_coherently() {
        let predicted = vec![pred("DEV-PANIC-STARTUP", 0.7, 7, 10)];
        let risk = assess_action_risk(&predicted, &[]);
        assert!(risk.route_lkg);
        assert!(risk.warn.unwrap().contains("no active dev-states"));
    }
}

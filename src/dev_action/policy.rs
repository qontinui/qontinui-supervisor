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
//! family — see [`is_boot_failure_signature`]) that trips either of two arms.
//!
//! **The served arm:** coord's `posterior_mean` is at or above
//! [`RISK_POSTERIOR_THRESHOLD`] AND its evidence rests on at least
//! [`RISK_MIN_TRIALS`] trials. The trials floor keeps a single unlucky sample
//! (1/1 = posterior 1.0) from tripping the bias — we only act when the recorded
//! cell has enough history to mean something.
//!
//! **The exact-cell arm:** the EXACT state set's own posterior
//! ([`exact_cell_posterior`]) is at or above the same threshold. coord's
//! `hierarchical-eb-v1` reader serves a `posterior_mean` that pools the exact
//! cell toward the kind's other state sets — better calibrated on average, but
//! slow to move on the incident shape this guard exists for. Before coord's
//! fit, the run of consecutive white-screens a new state set needs to reach a
//! pooled 0.5 grows with the kind's healthy history: 3 on an empty kind, about
//! 14 on restart's ledger as it stood on 2026-10-27 (39 trials, one boot
//! failure), about 18 just before the 100-trial fit. The exact cell's own
//! posterior (the formula the earlier reader served once a state set had 5
//! trials of its own) reaches it at the 4th. This arm needs no trials floor
//! of its own: under its Beta(0.4, 3.6) prior no record of fewer than 4
//! trials reaches 0.5, so it already takes 4 consecutive failures. A false
//! positive here costs one LKG-pinned restart and a warning; a late trip
//! costs a white-screen per restart in between.
//!
//! **The veto:** pooling errs the other way too. Before coord's fit, a state
//! set whose own record is healthy can be served a posterior above 0.5 (a
//! served 0.62 beside an exact record of 0 of 10), which the earlier reader
//! could not do: once a state set had [`N_MIN_EXACT`] trials of its own, it
//! served that cell alone. So once the exact state set has [`N_MIN_EXACT`]
//! trials and its own posterior is below the threshold, the served arm alone
//! does not trip; from there the exact record decides, as it did under the
//! earlier reader. Against a coord that sends no exact-cell counts (a reader
//! that predates them) only the served arm exists, and its rule is unchanged.

use serde::Serialize;

use crate::dev_action::expectations::PredictedSignature;

/// Posterior-mean threshold at or above which a boot-failure-class signature is
/// treated as high-risk. Tunable; 0.5 = "history says this state set fails the
/// boot/asset/webview check at least half the time."
pub const RISK_POSTERIOR_THRESHOLD: f64 = 0.5;

/// Minimum trials behind coord's served posterior before it can trip the risk
/// bias (the served arm; the exact-cell arm's floor comes from its prior — see
/// the module doc). Guards against a single unlucky sample (1/1 ⇒ posterior
/// 1.0) auto-routing to LKG on one data point. Small by design — a handful of
/// recorded restarts from a state set is enough signal to bias-and-warn (we
/// never refuse, so the cost of a false positive is just an LKG-pinned restart
/// + a warning).
pub const RISK_MIN_TRIALS: u64 = 3;

/// Exact-cell trials from which a healthy exact record vetoes the served arm
/// (see the module doc). Named after coord's pre-hierarchical reader's
/// `N_MIN_EXACT`: from this many trials of its own, that reader served the
/// exact state set's own rate alone, so a pooled posterior could not outvote
/// the set's own record.
pub const N_MIN_EXACT: u64 = 5;

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

/// Prior occurrences and prior trials of [`exact_cell_posterior`]: the
/// Beta(0.4, 3.6) of coord's dev-signature prior (mean 0.1, strength 4).
const EXACT_PRIOR_OCCURRENCES: f64 = 0.4;
const EXACT_PRIOR_TRIALS: f64 = 4.0;

/// The exact state set's OWN posterior mean, `(y + 0.4) / (n + 4)` over the
/// cell's `exact_occurrences` / `exact_trials`, with no pooling toward other
/// state sets. `None` when coord sent no exact-cell counts (an older reader)
/// or the cell has no trials.
pub fn exact_cell_posterior(pred: &PredictedSignature) -> Option<f64> {
    let n = pred.exact_trials.filter(|&n| n > 0)?;
    let y = pred.exact_occurrences.unwrap_or(0).min(n);
    Some((y as f64 + EXACT_PRIOR_OCCURRENCES) / (n as f64 + EXACT_PRIOR_TRIALS))
}

/// Which arm made a prediction high-risk (see the module doc), so the warning
/// can name the posterior that crossed the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RiskArm {
    /// coord's served posterior alone.
    Served,
    /// The exact state set's own posterior alone.
    ExactCell,
    /// Both posteriors.
    Both,
}

/// Which arm, if any, makes one prediction high-risk. `None` unless the
/// signature is boot-failure class and either coord's served posterior clears
/// the threshold over at least [`RISK_MIN_TRIALS`] trials without a healthy
/// exact record of [`N_MIN_EXACT`] or more trials vetoing it, or the exact
/// state set's own posterior clears the threshold (see the module doc).
fn high_risk_arm(pred: &PredictedSignature) -> Option<RiskArm> {
    if !is_boot_failure_signature(&pred.signature) {
        return None;
    }
    // No trials floor here: even at all failures, (n + 0.4) / (n + 4) >= 0.5
    // needs n >= 3.2, so the Beta(0.4, 3.6) prior already takes 4 consecutive
    // failures, more than the served arm's RISK_MIN_TRIALS.
    let exact_cell = exact_cell_posterior(pred).is_some_and(|p| p >= RISK_POSTERIOR_THRESHOLD);
    // From N_MIN_EXACT trials (so a posterior exists), the set's own record
    // outvotes the pooled one: below the threshold, the served arm cannot trip.
    let vetoed = !exact_cell && pred.exact_trials.is_some_and(|n| n >= N_MIN_EXACT);
    let served = pred.posterior_mean >= RISK_POSTERIOR_THRESHOLD
        && pred.trials >= RISK_MIN_TRIALS
        && !vetoed;
    match (served, exact_cell) {
        (true, true) => Some(RiskArm::Both),
        (true, false) => Some(RiskArm::Served),
        (false, true) => Some(RiskArm::ExactCell),
        (false, false) => None,
    }
}

/// A posterior as the warning prints it: two decimals, or as many more as it
/// takes for one below [`RISK_POSTERIOR_THRESHOLD`] to print below it. A served
/// 0.4968 reads "0.497", not a "0.50" that looks like it met the `>= 0.50`
/// printed beside the other arm.
fn posterior_text(p: f64) -> String {
    (2..=17)
        .map(|digits| format!("{p:.digits$}"))
        .find(|text| {
            p >= RISK_POSTERIOR_THRESHOLD
                || text
                    .parse::<f64>()
                    .is_ok_and(|shown| shown < RISK_POSTERIOR_THRESHOLD)
        })
        .unwrap_or_else(|| p.to_string())
}

/// The warning's evidence clause for the prediction that tripped. When coord
/// sent the exact-cell counts there are two arms to tell apart, so
/// `>= <threshold> (<arm>)` follows each posterior that crossed it, and every
/// posterior below the threshold prints below it ([`posterior_text`]).
/// Without them only the served arm exists and the clause reads as it always
/// did.
fn evidence_clause(top: &PredictedSignature, arm: RiskArm) -> String {
    let crossed = |arm_name: &str| format!(" >= {RISK_POSTERIOR_THRESHOLD:.2} ({arm_name})");
    let served_mark = if top.exact_trials.is_some() && arm != RiskArm::ExactCell {
        crossed("served arm")
    } else {
        String::new()
    };
    let own_posterior = exact_cell_posterior(top);
    let exact_desc = match (top.exact_occurrences, top.exact_trials, own_posterior) {
        (Some(y), Some(n), Some(own)) => {
            let exact_mark = if arm == RiskArm::Served {
                String::new()
            } else {
                crossed("exact-cell arm")
            };
            format!(
                "; this exact state set: {y} of {n} trials, own posterior {}{exact_mark}",
                posterior_text(own)
            )
        }
        _ => String::new(),
    };
    format!(
        "posterior {}{served_mark}, {}/{} trials, evidence={}{exact_desc}",
        posterior_text(top.posterior_mean),
        top.occurrences,
        top.trials,
        top.evidence_basis,
    )
}

/// The assessment the policy returns for an action — pure advice the mint flow
/// stamps into the ACK and (for the primary restart) uses to bias the exe pick.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRisk {
    /// Advise routing the action to the last-known-good binary instead of the
    /// freshest slot exe. `true` only when a boot-failure-class prediction
    /// tripped the served or the exact-cell arm (see the module doc). The
    /// caller may still override this when the operator explicitly opted out
    /// (provenance/exe selector).
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
/// boot/asset/webview-failure-class prediction trips the served arm (its
/// posterior clears [`RISK_POSTERIOR_THRESHOLD`] over at least
/// [`RISK_MIN_TRIALS`] trials, unless a healthy exact record of
/// [`N_MIN_EXACT`] or more trials vetoes it) or the exact-cell arm (the exact
/// state set's own [`exact_cell_posterior`] clears the same threshold), returns
/// an assessment with `route_lkg = true`, a loud `warn` naming the top
/// signature + its evidence (and, when coord sent the exact-cell counts, the
/// arm that crossed) + the active states, and `top_signature` echoing that
/// prediction.
///
/// The "top" signature is the highest-posterior high-risk prediction (ties
/// broken by trials, then occurrences, then the smaller signature) — the
/// strongest evidence in the cell. The last key is coord's own tie order, so
/// the pick depends on which rows qualify and not on the order they arrived
/// in: a smaller fetch that still holds the winner names the same signature.
pub fn assess_action_risk(predicted: &[PredictedSignature], states_active: &[&str]) -> ActionRisk {
    // Pick the strongest high-risk prediction. Compare by posterior, then by
    // trials, then by occurrences so the "loudest, best-evidenced" one wins; a
    // full tie goes to the smaller signature (`max_by` alone would keep
    // whichever tied row came last).
    let top = predicted
        .iter()
        .filter_map(|p| high_risk_arm(p).map(|arm| (p, arm)))
        .max_by(|(a, _), (b, _)| {
            a.posterior_mean
                .total_cmp(&b.posterior_mean)
                .then_with(|| a.trials.cmp(&b.trials))
                .then_with(|| a.occurrences.cmp(&b.occurrences))
                .then_with(|| b.signature.cmp(&a.signature))
        });

    let Some((top, arm)) = top else {
        return ActionRisk::none();
    };

    let states_desc = if states_active.is_empty() {
        "no active dev-states".to_string()
    } else {
        states_active.join(", ")
    };
    let warn = format!(
        "HIGH-RISK dev action: history predicts boot-failure signature {} \
         ({}) under active states [{}]. \
         Biasing toward the last-known-good binary to avoid a white-screen; \
         pass an explicit exe/provenance selector to override.",
        top.signature,
        evidence_clause(top, arm),
        states_desc,
    );

    ActionRisk {
        route_lkg: true,
        warn: Some(warn),
        top_signature: Some(top.clone()),
    }
}

// ---------------------------------------------------------------------------
// S3: `route_lkg` by expected loss, in shadow
// ---------------------------------------------------------------------------

/// The version of the shadow rule [`assess_route_lkg_shadow`] stamps as
/// `metadata.route_lkg_shadow.rule_version`. Bump it whenever the rule, its
/// loss model or the meaning of a stamped field changes, so a scorer can tell
/// rows recorded under the two apart.
pub const ROUTE_LKG_SHADOW_RULE_VERSION: u32 = 1;

/// The environment variable that declares the shadow rule's cost ratio
/// `c_LKG / c_fail` ([`route_lkg_ratio`]).
pub const ROUTE_LKG_RATIO_ENV: &str = "SUPERVISOR_ROUTE_LKG_RATIO";

/// The default cost ratio: the live threshold [`RISK_POSTERIOR_THRESHOLD`],
/// kept for continuity alone, since neither cost is measured (the research
/// record's theory §6.1).
pub const DEFAULT_ROUTE_LKG_RATIO: f64 = RISK_POSTERIOR_THRESHOLD;

/// Parse a declared cost ratio: a finite number in `(0, 1]`. `None` (the
/// variable unset) is the default with no complaint; anything else that is
/// not such a number is the default plus the warning to log. A ratio of 0 or
/// below would route every restart with any eligible evidence and one above 1
/// none, so neither is a cost ratio this rule can mean.
pub fn parse_route_lkg_ratio(raw: Option<&str>) -> (f64, Option<String>) {
    let Some(raw) = raw else {
        return (DEFAULT_ROUTE_LKG_RATIO, None);
    };
    match raw.trim().parse::<f64>() {
        Ok(ratio) if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 => (ratio, None),
        _ => (
            DEFAULT_ROUTE_LKG_RATIO,
            Some(format!(
                "{ROUTE_LKG_RATIO_ENV}={raw:?} is not a cost ratio in (0, 1]; \
                 the route_lkg shadow uses the default {DEFAULT_ROUTE_LKG_RATIO}"
            )),
        ),
    }
}

/// The declared cost ratio from [`ROUTE_LKG_RATIO_ENV`], read at each mint so
/// it needs no restart to take effect. An invalid value falls back to
/// [`DEFAULT_ROUTE_LKG_RATIO`] with a warning ([`parse_route_lkg_ratio`]).
pub fn route_lkg_ratio() -> f64 {
    let raw = std::env::var(ROUTE_LKG_RATIO_ENV).ok();
    let (ratio, warning) = parse_route_lkg_ratio(raw.as_deref());
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    ratio
}

/// The posterior the shadow rule decides one prediction on: the failure
/// probability `p` its expected loss is computed from. `None` when the row is
/// not boot-failure class or no arm has eligible evidence, so the rule takes
/// the fresh exe at every ratio.
///
/// It is [`high_risk_arm`]'s two arms and veto, folded into one number:
///
/// - from [`N_MIN_EXACT`] exact trials the exact cell's own posterior alone
///   ([`exact_cell_posterior`]): below the ratio the veto silences the served
///   arm, so the exact arm decides, and at or above it the exact arm trips;
/// - below that, the larger of the exact cell's own posterior (when coord sent
///   the exact-cell counts) and coord's served mean (over at least
///   [`RISK_MIN_TRIALS`] trials), since either arm reaching the ratio routes.
///
/// None of it reads the ratio, so a row stamped with this `p` can be
/// re-scored at any other ratio.
fn decision_posterior(pred: &PredictedSignature) -> Option<f64> {
    if !is_boot_failure_signature(&pred.signature) {
        return None;
    }
    let exact = exact_cell_posterior(pred);
    if pred.exact_trials.is_some_and(|n| n >= N_MIN_EXACT) {
        return exact;
    }
    let served = (pred.trials >= RISK_MIN_TRIALS).then_some(pred.posterior_mean);
    match (exact, served) {
        (Some(exact), Some(served)) => Some(exact.max(served)),
        (exact, served) => exact.or(served),
    }
}

/// Whether the shadow rule routes one prediction to the LKG exe at `ratio`.
///
/// This is [`high_risk_arm`] with `ratio` in place of
/// [`RISK_POSTERIOR_THRESHOLD`], veto included (it vetoes the served arm when
/// the exact cell's own posterior is below `ratio`), written as the
/// expected-loss comparison of [`RouteLkgShadow`]: route when
/// `p · c_fail >= c_LKG`, i.e. `p >= ratio`, with `p` the
/// [`decision_posterior`]. At `ratio` 0.5 it equals the live arm on every
/// prediction (`route_lkg_shadow_equals_the_live_arm_at_ratio_one_half`).
pub fn route_lkg_shadow(pred: &PredictedSignature, ratio: f64) -> bool {
    decision_posterior(pred).is_some_and(|p| p >= ratio)
}

/// Which exe a rule sends a restart to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The freshest slot exe.
    Fresh,
    /// The last-known-good exe.
    Lkg,
}

impl Route {
    fn from_lkg(lkg: bool) -> Self {
        if lkg {
            Route::Lkg
        } else {
            Route::Fresh
        }
    }
}

/// The `route_lkg` shadow over one restart's expectations: what the
/// expected-loss rule would decide at the declared ratio beside what the live
/// rule decided, with the evidence that lets the row be re-scored at another
/// ratio. Stamped into the snapshot as `metadata.route_lkg_shadow`, with
/// `use_lkg` (which exe the restart actually ran) added at the stamp
/// (`dev_action::ingest`).
///
/// # The loss model (the research record's theory §6.1)
///
/// Losses are in units of `c_fail`, the cost of a restart that white-screens,
/// and `ratio = c_LKG / c_fail`, the declared cost of running the
/// last-known-good exe instead (older code, a pinned restart), which is paid
/// whether or not the fresh exe would have failed:
///
/// - `expected_loss_fresh = p`: the fresh exe fails with probability `p`, the
///   [`decision_posterior`] of the strongest boot-failure row;
/// - `expected_loss_lkg = ratio`.
///
/// The rule takes the LKG exe iff `expected_loss_fresh >= expected_loss_lkg`
/// (`>=`, as the live arm's `>=`). The LKG exe's own failure rate is taken as
/// negligible, as the record's rule does; nothing measures it until
/// `restart_lkg` rows accrue. `expected_loss_fresh` is `null` when no row has
/// eligible evidence, and the rule then takes the fresh exe at every ratio.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteLkgShadow {
    /// [`ROUTE_LKG_SHADOW_RULE_VERSION`].
    pub rule_version: u32,
    /// The declared `c_LKG / c_fail` ([`route_lkg_ratio`]).
    pub ratio: f64,
    /// What the expected-loss rule decides at `ratio`.
    pub shadow_route: Route,
    /// What the live rule ([`assess_action_risk`]) advised over the same rows.
    /// Not which exe ran: an operator's `use_lkg` overrides the advice, and an
    /// absent LKG exe defeats it.
    pub live_route: Route,
    /// The boot-failure row the shadow decided on: the largest
    /// [`decision_posterior`], ties broken as the live rule breaks them
    /// (served posterior, trials, occurrences, then the smaller signature).
    /// `None` when coord served no boot-failure row.
    pub top_signature: Option<String>,
    /// That row's served posterior (coord's `posterior_mean`).
    pub p_served: Option<f64>,
    /// That row's served trials.
    pub trials: Option<u64>,
    /// That row's exact cell's own posterior ([`exact_cell_posterior`]),
    /// `None` when coord sent no exact-cell counts or the cell is empty.
    pub p_exact: Option<f64>,
    /// That row's exact-cell trials, as coord sent them.
    pub exact_trials: Option<u64>,
    /// `p` in units of `c_fail` (see the loss model above).
    pub expected_loss_fresh: Option<f64>,
    /// `ratio` in units of `c_fail`.
    pub expected_loss_lkg: f64,
}

/// The `route_lkg` shadow over every expectation coord served for a restart
/// (the same uncut rows the live assessment reads), at `ratio`. Pure.
pub fn assess_route_lkg_shadow(predicted: &[PredictedSignature], ratio: f64) -> RouteLkgShadow {
    let live = predicted.iter().any(|p| high_risk_arm(p).is_some());
    let top = predicted
        .iter()
        .filter(|p| is_boot_failure_signature(&p.signature))
        .map(|p| (p, decision_posterior(p)))
        .max_by(|(a, pa), (b, pb)| {
            let by_p = match (pa, pb) {
                (Some(x), Some(y)) => x.total_cmp(y),
                (x, y) => x.is_some().cmp(&y.is_some()),
            };
            by_p.then_with(|| a.posterior_mean.total_cmp(&b.posterior_mean))
                .then_with(|| a.trials.cmp(&b.trials))
                .then_with(|| a.occurrences.cmp(&b.occurrences))
                .then_with(|| b.signature.cmp(&a.signature))
        });
    let p = top.and_then(|(_, p)| p);
    let top = top.map(|(row, _)| row);
    RouteLkgShadow {
        rule_version: ROUTE_LKG_SHADOW_RULE_VERSION,
        ratio,
        // Any row routing routes the restart. The top row carries the largest
        // decision posterior, so this is `expected_loss_fresh >= ratio`
        // (pinned by `assess_route_lkg_shadow_agrees_with_the_live_rule_at_one_half`).
        shadow_route: Route::from_lkg(predicted.iter().any(|row| route_lkg_shadow(row, ratio))),
        live_route: Route::from_lkg(live),
        top_signature: top.map(|row| row.signature.clone()),
        p_served: top.map(|row| row.posterior_mean),
        trials: top.map(|row| row.trials),
        p_exact: top.and_then(exact_cell_posterior),
        exact_trials: top.and_then(|row| row.exact_trials),
        expected_loss_fresh: p,
        expected_loss_lkg: ratio,
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
            exact_trials: None,
            exact_occurrences: None,
        }
    }

    /// A `hierarchical-eb-v1` row: a pooled `posterior_mean` over the top
    /// active state's marginal, beside the exact state set's own counts.
    fn pooled(sig: &str, posterior: f64, exact_occ: u64, exact_trials: u64) -> PredictedSignature {
        PredictedSignature {
            signature: sig.to_string(),
            posterior_mean: posterior,
            occurrences: exact_occ,
            trials: 12,
            evidence_basis: "marginal".to_string(),
            exact_trials: Some(exact_trials),
            exact_occurrences: Some(exact_occ),
        }
    }

    /// A `hierarchical-eb-v1` row whose served side clears the threshold by
    /// itself: a pooled 0.62 over the top active state's marginal 27 of 40
    /// (what coord serves before its fit for {A} 27/30, {B} 27/30, {A,B} 0/10),
    /// beside the exact state set's own `(y, n)` — `None` for a coord that
    /// predates the exact-cell counts.
    fn served_062(sig: &str, exact: Option<(u64, u64)>) -> PredictedSignature {
        PredictedSignature {
            signature: sig.to_string(),
            posterior_mean: 0.62,
            occurrences: 27,
            trials: 40,
            evidence_basis: "marginal".to_string(),
            exact_trials: exact.map(|(_, n)| n),
            exact_occurrences: exact.map(|(y, _)| y),
        }
    }

    /// The incident shape under coord's `hierarchical-eb-v1` reader: the
    /// pooled posterior stays low (before the fit, the kind's healthy history
    /// holds it near the global rate) while every restart from this exact
    /// state set has white-screened. The exact cell's own record trips the
    /// bias at the 4th consecutive failure; the pooled 0.23 alone would not.
    #[test]
    fn the_exact_state_sets_own_record_trips_the_bias_when_pooling_lags() {
        // 3/3 white-screens: (3 + 0.4) / (3 + 4) = 0.486 — not yet.
        let three = vec![pooled("DEV-TAURI-ASSET-MISSING", 0.21, 3, 3)];
        assert_eq!(
            assess_action_risk(&three, INCIDENT_STATES),
            ActionRisk::none()
        );
        // 4/4: (4 + 0.4) / (4 + 4) = 0.55 — route to LKG.
        let four = vec![pooled("DEV-TAURI-ASSET-MISSING", 0.23, 4, 4)];
        let risk = assess_action_risk(&four, INCIDENT_STATES);
        assert!(risk.route_lkg, "exact cell 4/4 must route_lkg");
        let warn = risk.warn.expect("warns");
        // The warning names the arm and the posterior that crossed, beside the
        // served 0.23 that did not.
        assert!(
            warn.contains(
                "(posterior 0.23, 4/12 trials, evidence=marginal; this exact state set: \
                 4 of 4 trials, own posterior 0.55 >= 0.50 (exact-cell arm))"
            ),
            "{warn}"
        );
        assert!(!warn.contains("served arm"), "{warn}");
        // A served posterior just under the threshold prints under it, never as
        // a "0.50" that reads as meeting the `>= 0.50` the exact-cell arm shows.
        for (served, printed) in [(0.4968, "0.497"), (0.4996, "0.4996")] {
            let near = vec![pooled("DEV-TAURI-ASSET-MISSING", served, 4, 4)];
            let warn = assess_action_risk(&near, INCIDENT_STATES)
                .warn
                .expect("warns");
            assert!(
                warn.contains(&format!(
                    "(posterior {printed}, 4/12 trials, evidence=marginal; this exact state \
                     set: 4 of 4 trials, own posterior 0.55 >= 0.50 (exact-cell arm))"
                )),
                "{warn}"
            );
        }
        // A non-boot signature never trips, whatever its exact record.
        let other = vec![pooled("DEV-UI-ERROR-BOUNDARY", 0.2, 9, 9)];
        assert_eq!(
            assess_action_risk(&other, INCIDENT_STATES),
            ActionRisk::none()
        );
        // A healthy exact record leaves a pooled low posterior alone.
        let healthy = vec![pooled("DEV-TAURI-ASSET-MISSING", 0.2, 1, 9)];
        assert_eq!(
            assess_action_risk(&healthy, INCIDENT_STATES),
            ActionRisk::none()
        );
    }

    /// The exact cell's own posterior: absent counts (an older coord) or an
    /// empty cell give `None`; occurrences are clamped into the trials.
    #[test]
    fn exact_cell_posterior_reads_only_what_coord_sent() {
        assert_eq!(
            exact_cell_posterior(&pred("DEV-PANIC-STARTUP", 0.1, 1, 9)),
            None
        );
        let empty = pooled("DEV-PANIC-STARTUP", 0.1, 0, 0);
        assert_eq!(exact_cell_posterior(&empty), None);
        let four = pooled("DEV-PANIC-STARTUP", 0.1, 4, 4);
        assert_eq!(exact_cell_posterior(&four), Some(4.4 / 8.0));
        let mut over = pooled("DEV-PANIC-STARTUP", 0.1, 4, 4);
        over.exact_occurrences = Some(9);
        assert_eq!(exact_cell_posterior(&over), Some(4.4 / 8.0));
    }

    /// A healthy exact record vetoes the served arm once the state set has
    /// [`N_MIN_EXACT`] trials of its own. Before coord's fit a served 0.62 can
    /// sit beside an exact record of 0 of 10, where the earlier reader served
    /// that cell alone and listed no row. Below [`N_MIN_EXACT`] trials, and
    /// against a coord that sends no exact-cell counts, the served arm routes
    /// as before.
    #[test]
    fn a_healthy_exact_record_vetoes_the_served_arm_from_n_min_exact_trials() {
        let asset = "DEV-TAURI-ASSET-MISSING";
        // 0 of 10: own posterior 0.4 / 14 = 0.03 — vetoed.
        assert_eq!(
            assess_action_risk(&[served_062(asset, Some((0, 10)))], INCIDENT_STATES),
            ActionRisk::none()
        );
        // 2 of 5, exactly N_MIN_EXACT: own posterior 2.4 / 9 = 0.27 — vetoed.
        assert_eq!(
            assess_action_risk(&[served_062(asset, Some((2, 5)))], INCIDENT_STATES),
            ActionRisk::none()
        );
        // 1 of 4: too few trials to veto, so the served arm routes.
        let one_of_four = assess_action_risk(&[served_062(asset, Some((1, 4)))], INCIDENT_STATES);
        assert!(one_of_four.route_lkg, "4 exact trials cannot veto");
        // No exact-cell counts (an older coord): routes, as before.
        let older = assess_action_risk(&[served_062(asset, None)], INCIDENT_STATES);
        assert!(older.route_lkg, "an older coord's served arm is unchanged");
        // A non-boot signature never routes, whichever arms it clears.
        let other = served_062("DEV-UI-ERROR-BOUNDARY", Some((9, 9)));
        assert_eq!(
            assess_action_risk(&[other], INCIDENT_STATES),
            ActionRisk::none()
        );
    }

    /// When coord sends the exact-cell counts the warning names the arm that
    /// decided: `>= 0.50 (<arm>)` follows each posterior that crossed. Without
    /// them only the served arm exists and the text reads as it always did.
    #[test]
    fn the_warning_names_the_served_arm_when_it_decided() {
        let warn_for = |pred: PredictedSignature| {
            assess_action_risk(&[pred], INCIDENT_STATES)
                .warn
                .expect("warns")
        };
        // The served arm alone: 2 of 4 is too few trials to veto, so the set's
        // own posterior (2.4 / 8 = 0.30) is context, not a trip.
        let served = warn_for(served_062("DEV-TAURI-ASSET-MISSING", Some((2, 4))));
        assert!(
            served.contains(
                "(posterior 0.62 >= 0.50 (served arm), 27/40 trials, evidence=marginal; \
                 this exact state set: 2 of 4 trials, own posterior 0.30)"
            ),
            "{served}"
        );
        assert!(!served.contains("exact-cell arm"), "{served}");
        // Both arms: each posterior that crossed is marked.
        let both = warn_for(served_062("DEV-TAURI-ASSET-MISSING", Some((6, 6))));
        assert!(
            both.contains("posterior 0.62 >= 0.50 (served arm)"),
            "{both}"
        );
        assert!(
            both.contains("6 of 6 trials, own posterior 0.64 >= 0.50 (exact-cell arm)"),
            "{both}"
        );
        // An older coord: one arm, and the text from before the exact-cell arm.
        let older = warn_for(served_062("DEV-TAURI-ASSET-MISSING", None));
        assert!(
            older.contains("(posterior 0.62, 27/40 trials, evidence=marginal) under active"),
            "{older}"
        );
    }

    /// The exact-cell arm needs no trials floor of its own: under the
    /// Beta(0.4, 3.6) prior no record of fewer than 4 trials reaches the
    /// threshold (even 3 of 3 is 3.4 / 7 = 0.486), and 4 is above
    /// [`RISK_MIN_TRIALS`]. Fails if the prior, the threshold or the served
    /// floor move far enough that a separate floor would change a verdict.
    #[test]
    fn the_exact_cell_arm_needs_no_trials_floor_of_its_own() {
        for n in 1..RISK_MIN_TRIALS.max(4) {
            for y in 0..=n {
                let own = exact_cell_posterior(&pooled("DEV-PANIC-STARTUP", 0.1, y, n));
                assert!(
                    own.is_some_and(|p| p < RISK_POSTERIOR_THRESHOLD),
                    "{y} of {n}: {own:?}"
                );
            }
        }
        let four = exact_cell_posterior(&pooled("DEV-PANIC-STARTUP", 0.1, 4, 4));
        assert!(four.is_some_and(|p| p >= RISK_POSTERIOR_THRESHOLD));
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

    /// A full tie on posterior, trials and occurrences goes to the smaller
    /// signature, so `top_signature` depends only on which rows qualify. Here
    /// coord's order (posterior, then signature) puts one tied boot row fifth
    /// and the other sixth: its top five, the whole list and the list in
    /// reverse all name the same signature.
    #[test]
    fn a_full_tie_names_the_smaller_signature_whatever_was_fetched() {
        let named = |rows: &[PredictedSignature]| {
            assess_action_risk(rows, INCIDENT_STATES)
                .top_signature
                .map(|p| p.signature)
        };
        let fetched = vec![
            pred("DEV-A", 0.9, 9, 10),
            pred("DEV-B", 0.9, 9, 10),
            pred("DEV-C", 0.9, 9, 10),
            pred("DEV-D", 0.9, 9, 10),
            pred("DEV-PANIC-STARTUP", 0.6, 6, 10),
            pred("DEV-TAURI-ASSET-MISSING", 0.6, 6, 10),
        ];
        assert_eq!(named(&fetched[..5]).as_deref(), Some("DEV-PANIC-STARTUP"));
        assert_eq!(named(&fetched).as_deref(), Some("DEV-PANIC-STARTUP"));
        let reversed: Vec<PredictedSignature> = fetched.into_iter().rev().collect();
        assert_eq!(named(&reversed).as_deref(), Some("DEV-PANIC-STARTUP"));
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

    // -- S3: the route_lkg shadow --------------------------------------------

    /// Every prediction shape the arms distinguish: boot and non-boot
    /// signatures, served posteriors around the thresholds, served trials
    /// around [`RISK_MIN_TRIALS`], and every exact record up to 12 trials, plus
    /// none (an older coord).
    fn prediction_grid() -> Vec<PredictedSignature> {
        let mut grid = Vec::new();
        let exacts: Vec<Option<(u64, u64)>> = std::iter::once(None)
            .chain((0..=12u64).flat_map(|n| (0..=n).map(move |y| Some((y, n)))))
            .collect();
        for sig in ["DEV-TAURI-ASSET-MISSING", "DEV-UI-ERROR-BOUNDARY"] {
            for posterior in [0.0, 0.1, 0.15, 0.2, 0.3, 0.4968, 0.5, 0.51, 0.62, 0.9, 1.0] {
                for trials in [0, 1, 2, 3, 4, 12, 40] {
                    for exact in &exacts {
                        grid.push(PredictedSignature {
                            signature: sig.to_string(),
                            posterior_mean: posterior,
                            occurrences: trials / 2,
                            trials,
                            evidence_basis: "marginal".to_string(),
                            exact_trials: exact.map(|(_, n)| n),
                            exact_occurrences: exact.map(|(y, _)| y),
                        });
                    }
                }
            }
        }
        grid
    }

    /// The research record's S3 rule written out literally: [`high_risk_arm`]
    /// with `ratio` in place of the 0.5 threshold, veto at `ratio`. The
    /// reference [`route_lkg_shadow`]'s expected-loss form is held to.
    fn arm_at(pred: &PredictedSignature, ratio: f64) -> bool {
        if !is_boot_failure_signature(&pred.signature) {
            return false;
        }
        let exact_cell = exact_cell_posterior(pred).is_some_and(|p| p >= ratio);
        let vetoed = !exact_cell && pred.exact_trials.is_some_and(|n| n >= N_MIN_EXACT);
        let served = pred.posterior_mean >= ratio && pred.trials >= RISK_MIN_TRIALS && !vetoed;
        served || exact_cell
    }

    /// At ratio 0.5 the shadow routes exactly where the live arm trips, on
    /// every prediction in the grid. Fails if the expected-loss form drops an
    /// arm, the veto, the trials floor or the `>=`, or if the live arm moves
    /// without the shadow.
    #[test]
    fn route_lkg_shadow_equals_the_live_arm_at_ratio_one_half() {
        let grid = prediction_grid();
        assert!(
            grid.len() > 10_000,
            "the grid covers the arms: {}",
            grid.len()
        );
        let mut routed = 0;
        for pred in &grid {
            let live = high_risk_arm(pred).is_some();
            assert_eq!(
                route_lkg_shadow(pred, DEFAULT_ROUTE_LKG_RATIO),
                live,
                "{pred:?}"
            );
            routed += usize::from(live);
        }
        assert!(routed > 0 && routed < grid.len(), "both routes occur");
    }

    /// At every other ratio the shadow is the literal substitution, so the
    /// stamped expected loss is enough to re-score a row at any ratio.
    #[test]
    fn route_lkg_shadow_is_the_live_arm_with_the_ratio_substituted() {
        let grid = prediction_grid();
        for ratio in [0.05, 0.1, 0.15, 0.3, 0.45, 0.55, 0.7, 0.9, 1.0] {
            for pred in &grid {
                assert_eq!(
                    route_lkg_shadow(pred, ratio),
                    arm_at(pred, ratio),
                    "{ratio} {pred:?}"
                );
                // The decision posterior reads no ratio: routing at `ratio`
                // is exactly the expected-loss comparison on it.
                assert_eq!(
                    route_lkg_shadow(pred, ratio),
                    decision_posterior(pred).is_some_and(|p| p >= ratio),
                );
            }
        }
    }

    /// Over a set of rows the shadow at 0.5 agrees with the live assessment,
    /// and its expected losses are `p` against `ratio`.
    #[test]
    fn assess_route_lkg_shadow_agrees_with_the_live_rule_at_one_half() {
        let grid = prediction_grid();
        // Pairs of rows: any one routing routes the restart, in both rules.
        for pair in grid.chunks(2).step_by(7) {
            let shadow = assess_route_lkg_shadow(pair, DEFAULT_ROUTE_LKG_RATIO);
            let live = assess_action_risk(pair, INCIDENT_STATES).route_lkg;
            assert_eq!(shadow.shadow_route, Route::from_lkg(live), "{pair:?}");
            assert_eq!(shadow.live_route, Route::from_lkg(live), "{pair:?}");
            assert_eq!(shadow.expected_loss_lkg, DEFAULT_ROUTE_LKG_RATIO);
            if let Some(fresh) = shadow.expected_loss_fresh {
                assert_eq!(fresh >= shadow.expected_loss_lkg, live, "{pair:?}");
            } else {
                assert!(!live, "no eligible evidence never routes: {pair:?}");
            }
            // At any ratio the stamped expected losses re-score the route.
            for ratio in [0.15, 0.7] {
                let shadow = assess_route_lkg_shadow(pair, ratio);
                let by_loss = shadow
                    .expected_loss_fresh
                    .is_some_and(|fresh| fresh >= shadow.expected_loss_lkg);
                assert_eq!(shadow.shadow_route, Route::from_lkg(by_loss), "{pair:?}");
            }
        }
    }

    /// The incident replica (theory §3.3: a served 0.152 over 39 trials, the
    /// exact cell 3 of 5): the live rule takes the fresh exe, and so does the
    /// shadow at 0.5, but a declared ratio of 0.15 routes it. The stamp names
    /// the row and every input, and the exact cell decides (from 5 trials, the
    /// veto regime), so `expected_loss_fresh` is its own posterior.
    #[test]
    fn a_lower_ratio_routes_the_incident_replica_the_live_rule_does_not() {
        let replica = PredictedSignature {
            signature: "DEV-TAURI-ASSET-MISSING".to_string(),
            posterior_mean: 0.152,
            occurrences: 3,
            trials: 39,
            evidence_basis: "marginal".to_string(),
            exact_trials: Some(5),
            exact_occurrences: Some(3),
        };
        let rows = vec![pred("DEV-UI-ERROR-BOUNDARY", 0.9, 9, 10), replica];
        let own = 3.4 / 9.0;

        let at_half = assess_route_lkg_shadow(&rows, 0.5);
        assert_eq!(at_half.shadow_route, Route::Fresh);
        assert_eq!(at_half.live_route, Route::Fresh);

        let low = assess_route_lkg_shadow(&rows, 0.15);
        assert_eq!(
            low,
            RouteLkgShadow {
                rule_version: ROUTE_LKG_SHADOW_RULE_VERSION,
                ratio: 0.15,
                shadow_route: Route::Lkg,
                live_route: Route::Fresh,
                top_signature: Some("DEV-TAURI-ASSET-MISSING".to_string()),
                p_served: Some(0.152),
                trials: Some(39),
                p_exact: Some(own),
                exact_trials: Some(5),
                expected_loss_fresh: Some(own),
                expected_loss_lkg: 0.15,
            }
        );
    }

    /// The shadow decides on the boot row with the largest decision
    /// posterior, not the largest served one: a served 0.9 over 2 trials is
    /// not eligible, and an exact record of 6 of 6 is. With no boot row the
    /// stamp names none and holds no expected fresh loss.
    #[test]
    fn the_shadow_decides_on_the_strongest_eligible_boot_row() {
        let rows = vec![
            pred("DEV-PANIC-STARTUP", 0.9, 2, 2),
            pooled("DEV-WEBVIEW-CONN-REFUSED", 0.3, 6, 6),
            pred("DEV-UI-ERROR-BOUNDARY", 0.99, 99, 100),
        ];
        let shadow = assess_route_lkg_shadow(&rows, 0.5);
        assert_eq!(
            shadow.top_signature.as_deref(),
            Some("DEV-WEBVIEW-CONN-REFUSED")
        );
        assert_eq!(shadow.expected_loss_fresh, Some(6.4 / 10.0));
        assert_eq!(shadow.shadow_route, Route::Lkg);

        // Only an ineligible boot row: it is named, with no `p`, and never routes.
        let ineligible = assess_route_lkg_shadow(&rows[..1], 0.05);
        assert_eq!(
            ineligible.top_signature.as_deref(),
            Some("DEV-PANIC-STARTUP")
        );
        assert_eq!(ineligible.expected_loss_fresh, None);
        assert_eq!(ineligible.shadow_route, Route::Fresh);

        let none = assess_route_lkg_shadow(&rows[2..], 0.05);
        assert_eq!(none.top_signature, None);
        assert_eq!(none.p_served, None);
        assert_eq!(none.expected_loss_fresh, None);
        assert_eq!(none.shadow_route, Route::Fresh);
        assert_eq!(none.live_route, Route::Fresh);
    }

    /// The declared ratio: unset is the default silently; a number in (0, 1]
    /// is taken; anything else is the default with a warning naming the value.
    #[test]
    fn the_declared_ratio_falls_back_to_one_half_with_a_warning() {
        assert_eq!(parse_route_lkg_ratio(None), (0.5, None));
        assert_eq!(parse_route_lkg_ratio(Some("0.15")), (0.15, None));
        assert_eq!(parse_route_lkg_ratio(Some(" 1 ")), (1.0, None));
        for bad in ["", "abc", "0", "-0.2", "1.5", "NaN", "inf"] {
            let (ratio, warning) = parse_route_lkg_ratio(Some(bad));
            assert_eq!(ratio, 0.5, "{bad:?}");
            let warning = warning.expect("an invalid ratio warns");
            assert!(warning.contains(ROUTE_LKG_RATIO_ENV), "{warning}");
            assert!(warning.contains(&format!("{bad:?}")), "{warning}");
        }
    }

    /// The stamp's wire names and route spellings.
    #[test]
    fn the_shadow_serializes_with_its_stamped_field_names() {
        let json = serde_json::to_value(assess_route_lkg_shadow(&[], 0.5)).expect("serialize");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("an object")
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
            ]
        );
        assert_eq!(json["shadow_route"], "fresh");
        assert_eq!(serde_json::to_value(Route::Lkg).expect("serialize"), "lkg");
    }
}

//! Mock replay of recorded dev-action outcomes — Phase 5 (the paper's §11
//! testability payoff).
//!
// The mock-replay harness is a deliberate test/orchestration-testing surface:
// its public items are exercised by the `#[cfg(test)]` regression suites in this
// module (and are the substrate a future `--mock` orchestration mode would wire
// in) but have no live caller in the shipped binary yet. In a *binary* crate,
// `pub` items with no in-crate caller are still `dead_code`, so allow it here
// until a non-test consumer lands — without this the CI `clippy -- -D warnings`
// (non-test build) would reject the harness purely for not being called outside
// tests.
#![allow(dead_code)]
//!
//! `plans/2026-06-07-twin-dev-event-cause-effect-ledger.md` §11 / Spinak 2025
//! §11.1: once dev actions are recorded as Action Snapshots
//! (`(o_a^h, S_Ξ^h, r_a^h)` — kind, the active dev-state set, the outcome), the
//! recorded ledger *is* a replay corpus. Orchestration logic (the
//! "should I take the LKG route before this restart?" policy) can then be
//! integration-tested with **no real builds or restarts** by sampling recorded
//! outcomes conditioned on the active state set.
//!
//! # Why a mock executor (computational isomorphism)
//!
//! [`MockActionExecutor`] returns a [`MockOutcome`] carrying the SAME
//! decision-relevant shape a live attribution result carries — the D3
//! `category` and the `DEV-*` `signatures` — so the policy under test cannot
//! distinguish a mock from a live execution. We deliberately return a *lighter*
//! [`MockOutcome`] rather than the full [`crate::dev_action::record::ActionOutcome`]:
//! the live outcome carries `ended_at` / `duration_ms` / `evidence_ref` /
//! `late_signatures` timing fields that have no meaning for a replayed sample
//! and would be noise (or worse, an invitation to assert on a fabricated
//! timestamp). The decision-relevant projection is exactly `(category,
//! signatures)`, plus an `evidence_basis` breadcrumb so a test can see whether
//! the sample came from an exact or a marginal match.
//!
//! # Preserved nondeterminism, deterministic CI (resolved Q7)
//!
//! A pinned fixture set encodes the *recorded distribution*: if 60% of the
//! historical snapshots for a state set were `Contradiction`, sampling
//! uniformly from those fixtures reproduces a ~60% Contradiction rate. The
//! nondeterminism is real (it is the recorded world's), but it is made
//! byte-reproducible in CI by injecting a seedable [`StdRng`] — two executors
//! built from the same fixtures + same seed emit the identical sample sequence.
//!
//! # Matching tiers (§11.1)
//!
//! Given `(kind, query_state_ids)` the matcher selects candidate fixtures in
//! two tiers, preferring the more specific:
//!
//! 1. **exact** — same `kind` AND the same state *set* (order-independent).
//! 2. **marginal** — fallback when no exact match: same `kind`, sharing at
//!    least one state id (the marginal conditional, when the exact cell of the
//!    contingency table is empty).
//!
//! No fixture at either tier ⇒ [`MockActionExecutor::sample`] returns `None`:
//! the honest "no recorded history for this situation" case (never a fabricated
//! outcome).

use std::collections::BTreeSet;

use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::SeedableRng;

use crate::dev_action::record::{ActionKind, D3Category};

/// A pinned historical Action Snapshot — the "ledger slice" a test pins.
///
/// This is intentionally NOT the live `ActionStore` table: a test pins a fixed
/// `Vec<SnapshotFixture>` so the replay corpus is stable and reviewable. Each
/// fixture is one recorded `(kind, active-state-set) → (category, signatures)`
/// observation; the historical *distribution* is encoded by how many fixtures
/// of each outcome you pin (e.g. 3 Contradiction + 2 Confirmed = a 3/5 rate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFixture {
    /// The action kind this snapshot recorded.
    pub kind: ActionKind,
    /// The active dev-state set at execution time, by canonical id. Compared as
    /// a *set* (order-independent) by the matcher.
    pub state_ids: Vec<String>,
    /// The recorded D3 verdict.
    pub category: D3Category,
    /// The `DEV-*` signatures observed within the attribution window.
    pub signatures: Vec<String>,
}

impl SnapshotFixture {
    /// Convenience constructor taking `&str` slices for the id/signature lists.
    pub fn new(
        kind: ActionKind,
        state_ids: &[&str],
        category: D3Category,
        signatures: &[&str],
    ) -> Self {
        Self {
            kind,
            state_ids: state_ids.iter().map(|s| s.to_string()).collect(),
            category,
            signatures: signatures.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Which matching tier produced a sampled fixture — returned for transparency
/// so a test (or an advisory policy) can tell an exact-cell sample from a
/// marginal-fallback one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceBasis {
    /// Sampled from fixtures with the same kind AND the same state set.
    Exact,
    /// Sampled from the marginal fallback: same kind, overlapping (but not
    /// identical) state set.
    Marginal,
}

impl EvidenceBasis {
    /// Stable string form (`"exact"` / `"marginal"`).
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceBasis::Exact => "exact",
            EvidenceBasis::Marginal => "marginal",
        }
    }
}

/// The mock outcome of a replayed action — the decision-relevant projection of
/// a live attribution result. Carries the same `(category, signatures)` shape
/// the policy under test would see from a real execution, plus the
/// [`EvidenceBasis`] breadcrumb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockOutcome {
    /// The replayed D3 verdict.
    pub category: D3Category,
    /// The replayed `DEV-*` signatures.
    pub signatures: Vec<String>,
    /// Which matching tier the sampled fixture came from.
    pub evidence_basis: EvidenceBasis,
}

/// Normalize a state-id list into an order-independent set for comparison.
fn state_set(ids: &[String]) -> BTreeSet<&str> {
    ids.iter().map(String::as_str).collect()
}

/// The matching function (§11.1): given `(kind, query_state_ids)`, return the
/// candidate fixtures and the tier they matched at.
///
/// - Exact tier first: same `kind` AND the same state SET (order-independent).
/// - Marginal fallback only if exact is empty: same `kind` sharing ≥1 state id.
///
/// Returns `None` when no fixture matches at either tier.
fn match_candidates<'a>(
    fixtures: &'a [SnapshotFixture],
    kind: ActionKind,
    query_state_ids: &[String],
) -> Option<(Vec<&'a SnapshotFixture>, EvidenceBasis)> {
    let query = state_set(query_state_ids);

    // Exact tier: same kind, identical state set.
    let exact: Vec<&SnapshotFixture> = fixtures
        .iter()
        .filter(|f| f.kind == kind && state_set(&f.state_ids) == query)
        .collect();
    if !exact.is_empty() {
        return Some((exact, EvidenceBasis::Exact));
    }

    // Marginal fallback: same kind, share at least one state id with the query.
    // (When the query set is empty there is nothing to share, so this stays
    // empty — an empty query that misses the exact tier is honestly "no
    // history".)
    let marginal: Vec<&SnapshotFixture> = fixtures
        .iter()
        .filter(|f| {
            f.kind == kind
                && state_set(&f.state_ids)
                    .intersection(&query)
                    .next()
                    .is_some()
        })
        .collect();
    if !marginal.is_empty() {
        return Some((marginal, EvidenceBasis::Marginal));
    }

    None
}

/// Replays recorded dev-action outcomes conditioned on the active state set,
/// using an injected seedable RNG for byte-reproducible sampling.
pub struct MockActionExecutor {
    fixtures: Vec<SnapshotFixture>,
    rng: StdRng,
}

impl MockActionExecutor {
    /// Build an executor over a pinned fixture set, seeded for determinism. Two
    /// executors built with the same `fixtures` and the same `seed` produce the
    /// identical sequence of [`MockActionExecutor::sample`] results.
    pub fn new(fixtures: Vec<SnapshotFixture>, seed: u64) -> Self {
        Self {
            fixtures,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Sample one mock outcome for `(kind, state_ids)`.
    ///
    /// Matches candidate fixtures (exact tier, then marginal fallback) and then
    /// samples ONE uniformly using the injected RNG — so the recorded outcome
    /// distribution is preserved while the draw is deterministic for a fixed
    /// seed. Returns `None` when no fixture matches at all (the honest "no
    /// recorded history" case — never a fabricated outcome).
    pub fn sample(&mut self, kind: ActionKind, state_ids: &[String]) -> Option<MockOutcome> {
        let (candidates, basis) = match_candidates(&self.fixtures, kind, state_ids)?;
        // Uniform selection from the matched candidates via the injected RNG.
        // `choose` returns `None` only on an empty slice, which `match_candidates`
        // never yields (it returns `None` instead of an empty candidate list).
        let chosen = candidates.choose(&mut self.rng)?;
        Some(MockOutcome {
            category: chosen.category,
            signatures: chosen.signatures.clone(),
            evidence_basis: basis,
        })
    }
}

/// A tiny advisory policy under test (§11): given the mock executor, draw
/// `samples` outcomes for `(kind, state_ids)` and return `true` when the
/// observed Contradiction-or-Failure rate exceeds 0.5 — i.e. recommend taking
/// the LKG route before this action because history says it tends to go wrong.
///
/// This is the *fixed* policy the 2026-06-07 incident regression exercises. The
/// OLD implicit policy was "always proceed" (equivalent to always returning
/// `false` here); this policy instead consults the recorded ledger and catches
/// the white-screen-prone state set. A query with no recorded history (every
/// sample `None`) yields a 0.0 rate ⇒ `false` (no evidence to override the
/// default).
pub fn recommend_lkg_route(
    executor: &mut MockActionExecutor,
    kind: ActionKind,
    state_ids: &[String],
    samples: usize,
) -> bool {
    if samples == 0 {
        return false;
    }
    let mut bad = 0usize;
    for _ in 0..samples {
        if let Some(outcome) = executor.sample(kind, state_ids) {
            if matches!(
                outcome.category,
                D3Category::Contradiction | D3Category::Failure
            ) {
                bad += 1;
            }
        }
    }
    (bad as f64) / (samples as f64) > 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical dev-state ids for the 2026-06-07 white-screen incident, matching
    // `qontinui_types::dev_states::DevState::as_str` (SLOTS_EMPTY /
    // LEGACY_EXE_FALLBACK).
    const SLOTS_EMPTY: &str = "SLOTS_EMPTY";
    const LEGACY_EXE_FALLBACK: &str = "LEGACY_EXE_FALLBACK";
    const LKG_FRESH: &str = "LKG_FRESH";

    const DEV_TAURI_ASSET_MISSING: &str = "DEV-TAURI-ASSET-MISSING";

    // A fixed seed used in EVERY test so the suite is deterministic.
    const SEED: u64 = 0x5EED_C0FF_u64;

    /// The pinned ledger slice for the 2026-06-07 incident: a restart taken with
    /// `{SLOTS_EMPTY, LEGACY_EXE_FALLBACK}` active. The historical white-screen
    /// rate was 3/5 — 3 Contradiction (`DEV-TAURI-ASSET-MISSING`) and 2
    /// Confirmed (clean restart). 3/5 = 0.6, the frequency the mock preserves.
    fn incident_fixtures() -> Vec<SnapshotFixture> {
        let incident_states = &[SLOTS_EMPTY, LEGACY_EXE_FALLBACK];
        vec![
            SnapshotFixture::new(
                ActionKind::Restart,
                incident_states,
                D3Category::Contradiction,
                &[DEV_TAURI_ASSET_MISSING],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                incident_states,
                D3Category::Contradiction,
                &[DEV_TAURI_ASSET_MISSING],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                incident_states,
                D3Category::Contradiction,
                &[DEV_TAURI_ASSET_MISSING],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                incident_states,
                D3Category::Confirmed,
                &[],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                incident_states,
                D3Category::Confirmed,
                &[],
            ),
        ]
    }

    fn incident_query() -> Vec<String> {
        vec![SLOTS_EMPTY.to_string(), LEGACY_EXE_FALLBACK.to_string()]
    }

    /// Determinism: two executors over the same fixtures + same seed produce the
    /// IDENTICAL sequence of samples (byte-reproducible CI, resolved Q7).
    #[test]
    fn same_seed_produces_identical_sample_sequence() {
        let query = incident_query();
        let mut a = MockActionExecutor::new(incident_fixtures(), SEED);
        let mut b = MockActionExecutor::new(incident_fixtures(), SEED);
        let seq_a: Vec<_> = (0..50)
            .map(|_| a.sample(ActionKind::Restart, &query))
            .collect();
        let seq_b: Vec<_> = (0..50)
            .map(|_| b.sample(ActionKind::Restart, &query))
            .collect();
        assert_eq!(seq_a, seq_b, "same seed must replay the same sequence");
    }

    /// Frequency preserved: over N=1000 seeded samples the Contradiction rate is
    /// ~0.6 (the recorded 3/5), within tolerance. The nondeterminism is real but
    /// reproducible.
    #[test]
    fn contradiction_frequency_matches_recorded_rate() {
        let query = incident_query();
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        let n = 1000;
        let contradictions = (0..n)
            .filter_map(|_| exec.sample(ActionKind::Restart, &query))
            .filter(|o| o.category == D3Category::Contradiction)
            .count();
        let rate = contradictions as f64 / n as f64;
        assert!(
            (0.5..0.7).contains(&rate),
            "Contradiction rate {rate} not within 0.5..0.7 of the recorded 0.6"
        );
    }

    /// The incident replays: at least one sample yields the white-screen verdict
    /// (`Contradiction` + `DEV-TAURI-ASSET-MISSING`).
    #[test]
    fn incident_white_screen_outcome_replays() {
        let query = incident_query();
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        let replayed = (0..50)
            .filter_map(|_| exec.sample(ActionKind::Restart, &query))
            .any(|o| {
                o.category == D3Category::Contradiction
                    && o.signatures.iter().any(|s| s == DEV_TAURI_ASSET_MISSING)
            });
        assert!(
            replayed,
            "the 2026-06-07 white-screen outcome must replay at least once"
        );
    }

    /// Every exact-tier sample for the incident state set is tagged `Exact`.
    #[test]
    fn incident_samples_are_exact_basis() {
        let query = incident_query();
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        for _ in 0..20 {
            let outcome = exec
                .sample(ActionKind::Restart, &query)
                .expect("incident state set has exact fixtures");
            assert_eq!(outcome.evidence_basis, EvidenceBasis::Exact);
        }
    }

    /// Old vs fixed policy (SS8.5). The FIXED `recommend_lkg_route` policy
    /// consults the recorded ledger and returns `true` for the incident state set
    /// (≥0.6 bad-rate > 0.5 threshold) — catching the white-screen the OLD
    /// "always proceed" policy (which would always return `false`) walked into on
    /// 2026-06-07. For a clean state set whose history is all-Confirmed it
    /// returns `false`, so the policy only diverges from "always proceed" exactly
    /// where the incident lived.
    #[test]
    fn fixed_policy_catches_incident_but_not_clean_state() {
        // Incident state set: fixed policy says "take the LKG route" (true).
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        let incident_route =
            recommend_lkg_route(&mut exec, ActionKind::Restart, &incident_query(), 200);
        assert!(
            incident_route,
            "fixed policy MUST recommend the LKG route for the incident state set \
             (the old always-proceed policy would NOT have — and white-screened)"
        );

        // Clean state set with all-Confirmed history: fixed policy says "proceed"
        // (false) — matching what always-proceed would (correctly) do here.
        let clean_fixtures = vec![
            SnapshotFixture::new(
                ActionKind::Restart,
                &[LKG_FRESH],
                D3Category::Confirmed,
                &[],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                &[LKG_FRESH],
                D3Category::Confirmed,
                &[],
            ),
            SnapshotFixture::new(
                ActionKind::Restart,
                &[LKG_FRESH],
                D3Category::Confirmed,
                &[],
            ),
        ];
        let mut clean_exec = MockActionExecutor::new(clean_fixtures, SEED);
        let clean_route = recommend_lkg_route(
            &mut clean_exec,
            ActionKind::Restart,
            &[LKG_FRESH.to_string()],
            200,
        );
        assert!(
            !clean_route,
            "fixed policy must NOT recommend the LKG route for an all-Confirmed clean state"
        );
    }

    /// Marginal fallback: a query with no exact fixture but an overlapping state
    /// still samples, tagged `Marginal`.
    #[test]
    fn marginal_fallback_samples_on_overlap() {
        let query = incident_query();
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        // {SLOTS_EMPTY, PRIMARY_DOWN} shares SLOTS_EMPTY with the pinned
        // {SLOTS_EMPTY, LEGACY_EXE_FALLBACK} fixtures but is not an exact set.
        let overlapping = vec![SLOTS_EMPTY.to_string(), "PRIMARY_DOWN".to_string()];
        assert_ne!(state_set(&overlapping), state_set(&query));
        let outcome = exec
            .sample(ActionKind::Restart, &overlapping)
            .expect("overlapping state set must fall back to a marginal sample");
        assert_eq!(outcome.evidence_basis, EvidenceBasis::Marginal);
    }

    /// No history at all ⇒ `None` (honest), and the wrong kind never matches the
    /// incident fixtures.
    #[test]
    fn no_match_returns_none() {
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        // Disjoint state set, same kind ⇒ no exact, no overlap ⇒ None.
        let disjoint = vec!["SOME_UNRELATED_STATE".to_string()];
        assert!(exec.sample(ActionKind::Restart, &disjoint).is_none());
        // Right state set but a kind we have no fixtures for ⇒ None.
        assert!(exec.sample(ActionKind::Build, &incident_query()).is_none());
        // Empty query set misses the exact tier and shares nothing ⇒ None.
        assert!(exec.sample(ActionKind::Restart, &[]).is_none());
    }

    /// `MockOutcome` carries the same decision-relevant shape a live
    /// `ActionOutcome` does (category + signatures) — the computational
    /// isomorphism the policy under test relies on.
    #[test]
    fn mock_outcome_mirrors_live_decision_shape() {
        let query = incident_query();
        let mut exec = MockActionExecutor::new(incident_fixtures(), SEED);
        let outcome = exec.sample(ActionKind::Restart, &query).expect("matches");
        // category is a real D3Category and signatures a real Vec<String> —
        // structurally indistinguishable from ActionOutcome's decision fields.
        let _category: D3Category = outcome.category;
        let _signatures: Vec<String> = outcome.signatures.clone();
        assert_eq!(outcome.evidence_basis.as_str(), "exact");
    }
}

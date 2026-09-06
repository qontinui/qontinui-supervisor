//! The one place a test's wall-clock number is decided.
//!
//! Plan `2026-09-06-supervisor-test-wall-clock-deadlines-fail-under-fleet-load`.
//!
//! A test that asserts *"this finished within N"* measures the box, not the
//! code. Sized against a quiet run it reddens under fleet load for reasons that
//! have nothing to do with the diff under review — measured twice on this
//! fleet, 11.46s and 13.99s against a 10s bound guarding a ~2.2s operation —
//! and the reader then learns to classify a red as "probably the flaky one",
//! which is how a real regression gets waved through.
//!
//! The cure is to assert the PROPERTY the deadline stood in for. Where that is
//! genuinely impossible, two rules apply, and they are why this module exists
//! rather than a scattering of larger literals:
//!
//! 1. **A residual bound is derived from the value it DISCRIMINATES AGAINST,
//!    never from a measured idle run.** A backstop separating "the guard fired
//!    at 2s" from "we waited out the child's own 60s sleep" belongs between
//!    those two numbers — not just above the quiet path. Say which value in the
//!    assertion message, so the next reader can re-derive it.
//! 2. **Widening is not the fix.** Multiplying every bound by ten turns a flaky
//!    test into one that can no longer fail. [`backstop`] therefore scales only
//!    bounds that already satisfy rule 1, and [`CLOCK_SCALE_ENV`] can only
//!    widen — a box slower than this fleet's can be accommodated without
//!    anybody editing a number in anger.
//!
//! ### What is NOT in this module's scope
//!
//! A **give-up budget on a poll-until-true loop** (`wait for the submission to
//! register`) is not a deadline assertion: the assertion is the property, and
//! the budget only decides how long to wait before calling it never. Widening
//! one weakens nothing, so those use [`poll_budget`] and are deliberately
//! generous. A **lower** bound (`elapsed >= 7s`, "the fixture really did outlive
//! the budget") cannot flake under load either — load only makes it hold — so
//! lower bounds are never scaled.

use std::time::Duration;

/// Widens every [`backstop`] and [`poll_budget`] on a box slower than the one
/// the defaults were derived on. Values below `1.0`, unparseable values and
/// non-finite values are ignored: this knob can only widen.
pub const CLOCK_SCALE_ENV: &str = "QONTINUI_SUPERVISOR_TEST_CLOCK_SCALE";

/// The active scale — `1.0` unless [`CLOCK_SCALE_ENV`] widens it.
pub fn scale() -> f64 {
    scale_from(std::env::var(CLOCK_SCALE_ENV).ok().as_deref())
}

/// [`scale`] with the environment read out, so the only-widens rule is
/// testable without mutating process-global state from a parallel test binary.
fn scale_from(raw: Option<&str>) -> f64 {
    raw.and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 1.0)
        .unwrap_or(1.0)
}

/// An UPPER wall-clock backstop that survived Phase 2 — i.e. one whose `base`
/// was derived from the value it discriminates against, per rule 1 above.
///
/// Do not reach for this to quiet a bound that is still sized against a quiet
/// run; re-derive the bound first, or assert the property instead.
pub fn backstop(base: Duration) -> Duration {
    base.mul_f64(scale())
}

/// The give-up budget for a poll-until-true loop.
///
/// Not an assertion about speed: the loop exits the instant the property holds,
/// so a generous budget costs a passing run nothing and only delays a genuine
/// failure. Deliberately far above any observed loaded path.
pub fn poll_budget(base: Duration) -> Duration {
    base.mul_f64(scale())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The knob only widens: a scale below 1, a non-number, a non-finite value
    /// and an absent variable all fall back to 1.0, so nobody can narrow a
    /// backstop into a flake by exporting a variable.
    #[test]
    fn scale_only_widens() {
        assert_eq!(scale_from(None), 1.0, "absent");
        assert_eq!(scale_from(Some("")), 1.0, "empty");
        assert_eq!(scale_from(Some("wide")), 1.0, "not a number");
        assert_eq!(scale_from(Some("0.5")), 1.0, "narrowing is refused");
        assert_eq!(scale_from(Some("-3")), 1.0, "negative is refused");
        assert_eq!(scale_from(Some("inf")), 1.0, "non-finite is refused");
        assert_eq!(scale_from(Some("NaN")), 1.0, "NaN is refused");
        assert_eq!(scale_from(Some(" 4 ")), 4.0, "a widening scale is honoured");
    }

    /// `backstop` and `poll_budget` are the same transform applied to two
    /// different KINDS of number; the split is documentation, not arithmetic,
    /// and this pins that they agree.
    #[test]
    fn backstop_and_poll_budget_agree_on_the_scale() {
        let base = Duration::from_secs(7);
        assert_eq!(backstop(base), poll_budget(base));
    }
}

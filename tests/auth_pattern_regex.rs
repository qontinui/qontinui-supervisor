//! Regex sanity tests for `AUTH_PATTERNS` (Item B of the supervisor
//! cleanup plan). The regex feeds `LastAuthResult` updates from runner
//! stdout/stderr — false positives leak unrelated log lines into the
//! `auth_state.rate_limit_hint` field, false negatives mean agents
//! never see auth failures.

use qontinui_supervisor::log_capture::AUTH_PATTERNS;

#[test]
fn matches_auto_login_failed_with_429() {
    assert!(AUTH_PATTERNS.is_match("auto-login failed: 429 rate-limited"));
}

#[test]
fn matches_backend_returned_429() {
    assert!(AUTH_PATTERNS.is_match("Backend returned 429 too many requests"));
}

#[test]
fn matches_rate_limit_exceeded() {
    assert!(AUTH_PATTERNS.is_match("rate-limit exceeded for 5 minutes"));
    // hyphen-free variant
    assert!(AUTH_PATTERNS.is_match("rate limit exceeded"));
}

#[test]
fn matches_autologin_no_hyphen_variant() {
    assert!(AUTH_PATTERNS.is_match("autologin failed badly"));
}

#[test]
fn matches_case_insensitive() {
    assert!(AUTH_PATTERNS.is_match("AUTO-LOGIN FAILED"));
    assert!(AUTH_PATTERNS.is_match("Rate-Limited"));
}

#[test]
fn matches_http_429_keyword() {
    assert!(AUTH_PATTERNS.is_match("response status: HTTP 429"));
}

#[test]
fn does_not_match_successful_login() {
    assert!(!AUTH_PATTERNS.is_match("user logged in successfully"));
}

#[test]
fn does_not_match_unrelated_latency_log() {
    assert!(!AUTH_PATTERNS.is_match("login latency: 250ms"));
}

#[test]
fn does_not_match_irrelevant_429_substrings() {
    // Should NOT trigger on a port like "TCP/429" or some unrelated number.
    assert!(!AUTH_PATTERNS.is_match("listening on TCP/4290"));
    assert!(!AUTH_PATTERNS.is_match("processed 4291 events"));
}

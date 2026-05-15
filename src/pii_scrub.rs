//! PII / secrets scrubbing for OpenTelemetry span attributes.
//!
//! Mirrors `../qontinui-coord/src/pii_scrub.rs` — see that file for
//! the design rationale (blocklist-vs-allowlist, where the call sites
//! live, why this is not a tracing-subscriber Layer).

#[allow(dead_code)] // referenced by docs; exposed for parity with the coord-side helper
pub const REDACT_KEY_SUBSTRINGS: &[&str] = &[
    "authorization",
    "token",
    "password",
    "passwd",
    "secret",
    "cookie",
    "set-cookie",
    "credential",
    "api_key",
    "apikey",
    "private_key",
    "jwt",
    "session_id",
    "session-id",
    "x-access-token",
    "bearer",
];

pub const REDACTED_VALUE: &str = "<redacted>";

#[allow(dead_code)] // exercised by tests + by sanitize_kv consumers
pub fn key_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    REDACT_KEY_SUBSTRINGS.iter().any(|p| lower.contains(p))
}

#[allow(dead_code)] // exercised by tests + by sanitize_value consumers
pub fn value_looks_sensitive(value: &str) -> bool {
    let trimmed = value.trim_start();
    if trimmed.len() > 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        return true;
    }
    let segs: Vec<&str> = value.split('.').collect();
    if segs.len() == 3 && segs.iter().all(|s| s.len() >= 4 && is_b64url(s)) {
        return true;
    }
    false
}

fn is_b64url(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
}

#[allow(dead_code)] // exercised by tests + by future span-attribute call sites
pub fn sanitize_kv(key: &str, value: &str) -> (String, String) {
    if key_is_sensitive(key) || value_looks_sensitive(value) {
        return (key.to_string(), REDACTED_VALUE.to_string());
    }
    (key.to_string(), value.to_string())
}

/// Same as [`sanitize_kv`] for a single value when the key is known
/// to be safe — only the value-side heuristic runs. Used by the
/// trace-propagation injector where the key is hard-coded
/// (`traceparent`, `tracestate`) and known non-sensitive.
pub fn sanitize_value(value: &str) -> String {
    if value_looks_sensitive(value) {
        return REDACTED_VALUE.to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_authorization_header() {
        let (_k, v) = sanitize_kv("Authorization", "Bearer abc.def.ghi");
        assert_eq!(v, REDACTED_VALUE);
    }

    #[test]
    fn redacts_jwt_body_by_shape() {
        let raw = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signaturepart";
        let (_k, v) = sanitize_kv("custom_field", raw);
        assert_eq!(v, REDACTED_VALUE);
    }

    #[test]
    fn passes_safe_attribute_through() {
        let (k, v) = sanitize_kv("repo", "qontinui-runner");
        assert_eq!(k, "repo");
        assert_eq!(v, "qontinui-runner");
    }

    #[test]
    fn cookie_value_redacts() {
        let (_k, v) = sanitize_kv("Cookie", "session=abc; theme=dark");
        assert_eq!(v, REDACTED_VALUE);
    }
}

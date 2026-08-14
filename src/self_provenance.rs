//! The supervisor's OWN build provenance — which commit THIS binary is made of.
//!
//! Phase 2.3 of
//! `plans/2026-08-04-landed-infra-fixes-not-in-effect-on-this-machine.md`.
//!
//! **Not to be confused with [`crate::git_provenance`]**, which is about the
//! RUNNER binary the build pool produces (`BuildProvenance`, origin/main drift,
//! the LKG's sha). Nothing there says anything about the supervisor process
//! itself. Before this module the only supervisor-side identity on `/health`
//! was `version: "0.1.0"` — a hardcoded crate version that has not changed in
//! the life of the repo, so "is the running supervisor new enough to contain
//! fix X?" had NO read-only answer. On 2026-08-04 that question was answered by
//! falling back to an exe mtime, which measured
//! `target/release/qontinui-supervisor.exe` while the process was in fact
//! running from `target/debug/copies/qontinui-supervisor.exe` — a wrong answer
//! from a plausible-looking signal.
//!
//! ## Wire shape (this is a consumed contract — do not change it casually)
//!
//! `GET /health` → `supervisor.built_from_sha` is **a full 40-character
//! lowercase-hex git commit sha, or JSON `null`**. Never a timestamp, never a
//! composite, never `""`. Downstream tooling answers "is the running supervisor
//! newer than fix X?" with:
//!
//! ```text
//! git merge-base --is-ancestor <fix-sha> <supervisor.built_from_sha>
//! ```
//!
//! so the field has to be something git can resolve. Two neighbouring surfaces
//! already spell "build id" two different ways — the supervisor reports the
//! runner's `buildId` as an ISO timestamp (`2026-08-12T10:12:09+00:00`) while
//! the runner's own `/health` reports `<sha>-<epoch-ms>`
//! (`c7c789571-1786171146043`). This is deliberately a THIRD shape only in the
//! sense of being the plain sha and nothing else; the dirty marker is carried
//! as a SEPARATE boolean (`supervisor.built_from_dirty`) precisely so no
//! consumer ever has to strip a `-dirty` suffix before handing the value to
//! git.
//!
//! `null` means UNKNOWN — built outside a git checkout, or with no git on PATH
//! — and must be read as "cannot tell", never as "clean" or "old". Same
//! absence-is-not-zero reading the fleet applies to a silent empty probe.
//!
//! The values are stamped by `build.rs` at COMPILE time (see
//! `emit_self_build_provenance`); a runtime `git rev-parse` would report what
//! the checkout says now, which diverges from what the running binary is made
//! of the moment anyone pulls or switches branch under a long-lived supervisor.

/// The sentinel `build.rs` stamps when it could not establish a commit.
const UNKNOWN: &str = "unknown";

/// Raw compile-time stamps. `build.rs` emits both unconditionally (before its
/// own early-return paths), so these `env!`s cannot fail to resolve.
const RAW_SHA: &str = env!("QONTINUI_SUPERVISOR_GIT_SHA");
const RAW_DIRTY: &str = env!("QONTINUI_SUPERVISOR_GIT_DIRTY");

/// The commit this binary was built from, or `None` when the build could not
/// establish one.
///
/// Re-validated here rather than trusted: `build.rs` runs in a different
/// process with a different env, and a field consumers feed to `git` must not
/// be able to carry a non-sha.
pub fn built_from_sha() -> Option<&'static str> {
    parse_sha(RAW_SHA)
}

/// Whether the working tree had modified TRACKED files when this binary was
/// built. `None` = unknown (no commit resolved, or git unavailable).
///
/// `Some(true)` means the sha is a lower bound on what is in the binary: the
/// commit is real, but the compiled sources also carried uncommitted edits, so
/// an `--is-ancestor` answer of "yes, contains fix X" is trustworthy while "no"
/// is not conclusive.
pub fn built_from_dirty() -> Option<bool> {
    parse_dirty(RAW_DIRTY)
}

/// Human-readable one-liner for the startup log: `"<sha>-dirty"`, `"<sha>"`, or
/// `"unknown"`.
///
/// LOG SURFACE ONLY. The `-dirty` suffix exists for eyeballs; machine consumers
/// read the two `/health` fields, which never glue the marker onto the sha.
pub fn describe() -> String {
    match (built_from_sha(), built_from_dirty()) {
        (Some(sha), Some(true)) => format!("{sha}-dirty"),
        (Some(sha), _) => sha.to_string(),
        (None, _) => UNKNOWN.to_string(),
    }
}

/// Validate a stamped sha: 7-40 hex characters, lowercased by git itself.
///
/// `""`, `"unknown"` and anything non-hex all collapse to `None` — the point of
/// the module is that an unresolvable commit is reported as UNKNOWN and not as
/// a blank string a consumer would read as "present and clean".
fn parse_sha(raw: &str) -> Option<&str> {
    let s = raw.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(UNKNOWN) {
        return None;
    }
    if (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(s)
    } else {
        None
    }
}

/// Validate the stamped dirty marker. Only the exact `true` / `false` spellings
/// `build.rs` emits are honoured; everything else (including `unknown`) is
/// UNKNOWN rather than a defaulted `false`, which would claim a clean tree
/// nobody measured.
fn parse_dirty(raw: &str) -> Option<bool> {
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{built_from_dirty, built_from_sha, describe, parse_dirty, parse_sha};

    /// A real sha survives; the shortest and longest accepted forms both pass.
    #[test]
    fn parse_sha_accepts_real_object_ids() {
        assert_eq!(
            parse_sha("6e89b17a1c0f9a2b3d4e5f60718293a4b5c6d7e8"),
            Some("6e89b17a1c0f9a2b3d4e5f60718293a4b5c6d7e8")
        );
        assert_eq!(parse_sha("6e89b17"), Some("6e89b17"));
        // Trailing newline from `git rev-parse` must not defeat the check.
        assert_eq!(parse_sha(" 6e89b17\n"), Some("6e89b17"));
    }

    /// The whole point of the module: an unresolvable commit reads as UNKNOWN,
    /// never as a present-but-blank value a consumer would treat as clean.
    #[test]
    fn parse_sha_rejects_every_non_sha_including_the_empty_string() {
        for bad in [
            "",
            "   ",
            "unknown",
            "UNKNOWN",
            // A timestamp — the exact shape the neighbouring `buildId` field
            // uses, and the one this field must never become.
            "2026-08-12T10:12:09+00:00",
            // The runner's `<sha>-<epoch-ms>` composite.
            "c7c789571-1786171146043",
            // Too short to disambiguate an object.
            "6e89b1",
            // Longer than a sha.
            "6e89b17a1c0f9a2b3d4e5f60718293a4b5c6d7e8f9",
            "not-hex-at-all",
        ] {
            assert_eq!(parse_sha(bad), None, "{bad:?} must not read as a commit");
        }
    }

    /// Only the two spellings build.rs emits are trusted; anything else is
    /// UNKNOWN, not a defaulted "clean".
    #[test]
    fn parse_dirty_is_tri_state() {
        assert_eq!(parse_dirty("true"), Some(true));
        assert_eq!(parse_dirty("false"), Some(false));
        assert_eq!(parse_dirty(" true\n"), Some(true));
        for bad in ["", "unknown", "True", "1", "yes"] {
            assert_eq!(parse_dirty(bad), None, "{bad:?} must read as unknown");
        }
    }

    /// The log-only rendering glues the marker on; the machine-read accessors
    /// never do.
    #[test]
    fn describe_is_consistent_with_the_accessors() {
        let d = describe();
        match built_from_sha() {
            Some(sha) => {
                assert!(d.starts_with(sha), "{d} should start with {sha}");
                assert_eq!(d.ends_with("-dirty"), built_from_dirty() == Some(true));
                // The machine-read field is the bare sha — no suffix.
                assert!(
                    !sha.contains('-'),
                    "built_from_sha must be a bare object id, got {sha}"
                );
            }
            None => assert_eq!(d, "unknown"),
        }
    }

    /// The compile-time stamps themselves must be well-formed on whatever
    /// machine ran the build: either a real sha or an explicit unknown — never
    /// an empty string, and never a value the accessor silently drops while
    /// the raw stamp claims otherwise.
    #[test]
    fn the_stamped_values_are_either_a_sha_or_an_explicit_unknown() {
        let raw = super::RAW_SHA.trim();
        assert!(
            !raw.is_empty(),
            "build.rs must always stamp something; empty reads as clean"
        );
        assert!(
            raw == super::UNKNOWN || parse_sha(raw).is_some(),
            "stamped sha {raw:?} is neither a sha nor the unknown sentinel"
        );
        let dirty = super::RAW_DIRTY.trim();
        assert!(
            dirty == "true" || dirty == "false" || dirty == super::UNKNOWN,
            "stamped dirty marker {dirty:?} is not one of true/false/unknown"
        );
    }
}

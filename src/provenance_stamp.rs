// MODULE DOCS ARE PLAIN `//` COMMENTS ON PURPOSE: `build.rs` splices this
// file in with `include!`, and an inner doc comment (`//!`) is a parse error
// anywhere but the head of a module. The rustdoc-visible summary therefore
// lives on the `mod provenance_stamp;` declarations in lib.rs / main.rs.
// Pure classification shared by `build.rs` and the crate it builds.
//
// A build script cannot import from its own crate, so this file is
// **`include!`d by `build.rs`** and *also* compiled as
// `crate::provenance_stamp`. One copy of the rules, two compilations — which
// is the whole point: a build script's logic is otherwise untestable, and
// untested is exactly how [`dirty_marker`]'s predecessor shipped a branch that
// could never be taken.
//
// **The bug this file exists to prevent (2026-08-15 review).** The first cut
// classified dirtiness from a `git status --porcelain` helper that collapsed
// empty-but-successful stdout to "no answer". A CLEAN tree is exactly the case
// that exits 0 with zero bytes, so `"false"` was unreachable: every clean build
// stamped `unknown`, the wire field degraded to `{true, null}`, and `null`
// stopped distinguishing "no git here" from "clean tree" — the
// plausible-looking-wrong-answer class this provenance was added to kill.
// [`dirty_marker`] therefore keys on the **exit status**, with the stdout
// bytes as data rather than as a success signal, and
// `dirty_marker_reports_clean_on_successful_empty_output` pins it.
//
// Nothing here may reference the rest of the crate — `build.rs` compiles it
// standalone.

/// The stamp meaning "this build could not establish the value".
///
/// A distinct sentinel, never an empty string: `""` reaches a consumer as a
/// present-but-blank field and reads as "clean/unset".
pub const UNKNOWN_STAMP: &str = "unknown";

/// Is `s` a plausible git object id (7-40 hex characters)?
///
/// Applied on the WRITE side (`build.rs`, to the `rev-parse` result and to any
/// build-env override) and again on the READ side
/// ([`crate::self_provenance::built_from_sha`]), because the two run in
/// different processes with different envs and the field is fed to `git`.
pub fn looks_like_sha(s: &str) -> bool {
    let s = s.trim();
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Classify `git status --porcelain --untracked-files=no` into the tri-state
/// dirty stamp: `"true"`, `"false"`, or [`UNKNOWN_STAMP`].
///
/// `status_ok` is whether the command **exited 0**. That is the only success
/// signal — `porcelain_stdout` is data, and its emptiness is the *positive*
/// evidence of a clean tree, never evidence that the probe failed.
///
/// Not called from the crate itself (only `build.rs`, through `include!`); it
/// lives here so the merge-blocking CI gate executes its tests.
#[allow(dead_code)]
pub fn dirty_marker(status_ok: bool, porcelain_stdout: &str) -> &'static str {
    if !status_ok {
        return UNKNOWN_STAMP;
    }
    if porcelain_stdout.trim().is_empty() {
        "false"
    } else {
        "true"
    }
}

#[cfg(test)]
mod tests {
    use super::{dirty_marker, looks_like_sha, UNKNOWN_STAMP};

    /// **The regression test for the shipped defect.** `git status --porcelain`
    /// on a clean tree exits 0 and writes ZERO bytes; if that reads as "no
    /// answer", `"false"` becomes unreachable and every clean build stamps
    /// `unknown`. Asserting the positive value is what catches it — a test that
    /// merely tolerates `unknown` (as the first three did) cannot.
    #[test]
    fn dirty_marker_reports_clean_on_successful_empty_output() {
        assert_eq!(dirty_marker(true, ""), "false");
        // Trailing newline / whitespace-only output is still clean.
        assert_eq!(dirty_marker(true, "\n"), "false");
        assert_eq!(dirty_marker(true, "   \n  "), "false");
    }

    /// Real porcelain output for a modified tracked file is dirty.
    #[test]
    fn dirty_marker_reports_dirty_on_any_tracked_modification() {
        assert_eq!(dirty_marker(true, " M src/build_monitor.rs\n"), "true");
        assert_eq!(
            dirty_marker(true, "A  src/sccache_guard.rs\nM  CLAUDE.md\n"),
            "true"
        );
        assert_eq!(dirty_marker(true, "D  src/gone.rs"), "true");
    }

    /// A FAILED status (no git, not a repo, broken index) is unknown — and is
    /// the ONLY thing that produces unknown, so `null` on the wire keeps
    /// meaning "cannot tell" rather than "clean".
    #[test]
    fn dirty_marker_reports_unknown_only_when_the_command_failed() {
        assert_eq!(dirty_marker(false, ""), UNKNOWN_STAMP);
        // Even with output, a non-zero exit is not a measurement.
        assert_eq!(dirty_marker(false, " M src/x.rs\n"), UNKNOWN_STAMP);
    }

    #[test]
    fn looks_like_sha_accepts_object_ids_and_rejects_everything_else() {
        assert!(looks_like_sha("6e89b1767d54d871aeb5324e3849b332374a4891"));
        assert!(looks_like_sha("6e89b17"));
        assert!(looks_like_sha(" 6e89b17\n"));
        for bad in [
            "",
            "unknown",
            "6e89b1",
            "6e89b1767d54d871aeb5324e3849b332374a4891f",
            "2026-08-12T10:12:09+00:00",
            "c7c789571-1786171146043",
        ] {
            assert!(!looks_like_sha(bad), "{bad:?} must not read as a sha");
        }
    }
}

//! Inherited Claude Code process-topology markers: the one place they are
//! named, and the one place they are stripped.
//!
//! Claude Code sets `CLAUDECODE` and `CLAUDE_CODE_CHILD_SESSION` to tell a
//! process about its position in a session tree. Both are inherited by the
//! whole process tree and **nothing clears them**. The supervisor is normally
//! launched FROM a Claude Code session, so it carries both and hands them to
//! every process it spawns — including the `claude` processes it launches
//! directly ([`crate::evaluation::judge`],
//! [`crate::velocity_improvement`]) and the runners that go on to launch more.
//! A genuine top-level session then claims to be the child of a session that
//! exited long ago.
//!
//! # Why this module exists
//!
//! The rule — *never hand an inherited Claude marker to a child we spawn* —
//! predates this module and was already documented on the runner side
//! (`qontinui-runner/CLAUDE.md`, "Claude CLI Spawning"). It was enforced by
//! copy-pasting `.env_remove("CLAUDECODE")` at each spawn site, which is
//! exactly how `CLAUDE_CODE_CHILD_SESSION` came to be missing from all of them
//! for months: a rule that lives only in prose and copy-paste is re-broken by
//! the next spawn site somebody adds.
//!
//! So the rule is code now. Every spawn site calls
//! [`StripInheritedClaudeMarkers::strip_inherited_claude_markers`], the marker
//! list lives in [`INHERITED_CLAUDE_MARKERS`], and
//! `every_spawn_site_goes_through_the_shared_strip` fails the build if a new
//! site open-codes `env_remove` for either marker. Adding a third marker is a
//! one-line change that covers every site at once.
//!
//! # This is not an `EnvForwarder`
//!
//! [`crate::process::env_forwarders`] is a registry of things to *set*, applied
//! after the base `Command` is built. The strip belongs on the base `Command`
//! so that `ExtraEnv` — which runs last — can still re-inject a marker via
//! `POST /runners/spawn-test {extra_env:{...}}`. That escape hatch is how the
//! marked-child case is tested deliberately, and it must be preserved.

use tracing::warn;

/// Claude Code's "this process is inside a Claude Code session" marker.
pub const CLAUDECODE_ENV: &str = "CLAUDECODE";

/// Claude Code's "you are a nested/child session" marker.
///
/// Correct for genuine subagents; a lie about process topology for anything the
/// supervisor spawns, which the CLI is entitled to act on however it likes.
pub const CLAUDE_CHILD_SESSION_ENV: &str = "CLAUDE_CODE_CHILD_SESSION";

/// Every inherited Claude Code marker stripped from spawned children.
///
/// **The single source of truth.** Add a marker here and every spawn site picks
/// it up; there is no per-site list to keep in sync.
pub const INHERITED_CLAUDE_MARKERS: &[&str] = &[CLAUDECODE_ENV, CLAUDE_CHILD_SESSION_ENV];

/// Removes every [`INHERITED_CLAUDE_MARKERS`] entry from a command's child env.
///
/// Implemented for both `tokio::process::Command` and `std::process::Command`
/// because the supervisor spawns with both — notably the `POST
/// /supervisor/restart` self-restart, which is a `std::process::Command` and is
/// the path by which a marker would otherwise survive every supervisor
/// generation.
///
/// Returns `&mut Self` so it drops into an existing builder chain in place of
/// the `.env_remove(...)` line it replaces.
pub trait StripInheritedClaudeMarkers {
    /// Strip the inherited markers. Call this on **every** spawn.
    fn strip_inherited_claude_markers(&mut self) -> &mut Self;
}

impl StripInheritedClaudeMarkers for tokio::process::Command {
    fn strip_inherited_claude_markers(&mut self) -> &mut Self {
        for marker in INHERITED_CLAUDE_MARKERS {
            self.env_remove(marker);
        }
        self
    }
}

impl StripInheritedClaudeMarkers for std::process::Command {
    fn strip_inherited_claude_markers(&mut self) -> &mut Self {
        for marker in INHERITED_CLAUDE_MARKERS {
            self.env_remove(marker);
        }
        self
    }
}

/// Log loudly when the supervisor process itself carries
/// [`CLAUDE_CHILD_SESSION_ENV`].
///
/// The supervisor is the ORIGIN of the leak for the whole fleet — it inherits
/// the marker from the session that launched it and would hand it to every
/// child. The spawn-side strip stops it propagating, but the supervisor's own
/// copy is the evidence of where it came in, and nothing surfaced it before:
/// the leak ran unnoticed for months and was then misdiagnosed, both
/// observability failures. Returns whether the marker was present so it can be
/// unit-tested without asserting on log output.
pub fn warn_if_child_session_marker_inherited() -> bool {
    // `var_os`, not `var`: PRESENCE is what matters. `var` also returns `Err`
    // for a non-UTF-8 value, which would report a set marker as absent — and
    // an empty value is still set (`env_remove` is what clears one).
    match std::env::var_os(CLAUDE_CHILD_SESSION_ENV) {
        Some(value) => {
            warn!(
                env_var = CLAUDE_CHILD_SESSION_ENV,
                value = %value.to_string_lossy(),
                "supervisor inherited Claude Code's child-session marker from its launching \
                 session; it is stripped from every process the supervisor spawns, but this \
                 process is mislabelled as a nested session. Launch the supervisor from a shell \
                 that does not carry the marker to clear it."
            );
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // Claude Code child-session marker strip (plan
    // 2026-07-28-runner-transcript-persistence-env-leak).

    #[test]
    fn marker_names_are_the_cli_markers() {
        // The strip and the startup warning are only correct if these are the
        // exact variables Claude Code sets. A typo here is silent: the spawn
        // would keep leaking the real marker while removing nothing.
        assert_eq!(CLAUDECODE_ENV, "CLAUDECODE");
        assert_eq!(CLAUDE_CHILD_SESSION_ENV, "CLAUDE_CODE_CHILD_SESSION");
        assert_eq!(
            INHERITED_CLAUDE_MARKERS,
            &["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION"],
            "the strip covers exactly the markers the spawn sites used to remove by hand"
        );
    }

    #[test]
    fn child_session_marker_detection_follows_env_presence() {
        // No env lock exists in this crate (see `stopped_cache` tests, which
        // sidestep it with unique var names). That trick can't work here — the
        // point is the REAL marker name — but this is the only test in the
        // crate that touches it, so nothing races it. `var_os` preserves a
        // non-UTF-8 pre-existing value across the restore.
        let restore = std::env::var_os(CLAUDE_CHILD_SESSION_ENV);

        std::env::remove_var(CLAUDE_CHILD_SESSION_ENV);
        assert!(
            !warn_if_child_session_marker_inherited(),
            "absent marker must not report as inherited"
        );

        std::env::set_var(CLAUDE_CHILD_SESSION_ENV, "1");
        assert!(
            warn_if_child_session_marker_inherited(),
            "present marker must report as inherited"
        );

        // An empty value is still an inherited marker — `env_remove` is what
        // clears it, not setting it to "".
        std::env::set_var(CLAUDE_CHILD_SESSION_ENV, "");
        assert!(
            warn_if_child_session_marker_inherited(),
            "empty-but-set marker is still inherited"
        );

        match restore {
            Some(v) => std::env::set_var(CLAUDE_CHILD_SESSION_ENV, v),
            None => std::env::remove_var(CLAUDE_CHILD_SESSION_ENV),
        }
    }

    /// Assert the builder records a REMOVAL (not a set, not an absence) for
    /// every marker.
    ///
    /// `get_envs` yields `(key, None)` for `env_remove` and `(key, Some(v))`
    /// for `env`. The distinction is the whole point: merely *not setting* a
    /// marker still lets the child inherit the supervisor's copy, which is the
    /// bug. Only an explicit removal blocks inheritance.
    fn assert_all_markers_removed(cmd: &std::process::Command, what: &str) {
        for marker in INHERITED_CLAUDE_MARKERS {
            let entry = cmd
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(marker));
            match entry {
                Some((_, None)) => {}
                Some((_, Some(v))) => panic!(
                    "{what}: {marker} is SET to {v:?} — a set marker is still inherited by the \
                     child; it must be removed"
                ),
                None => panic!(
                    "{what}: {marker} has no removal entry — the child would inherit the \
                     supervisor's copy"
                ),
            }
        }
    }

    /// Both `Command` flavours must mark every marker for removal.
    ///
    /// Inspects the builder rather than spawning a shell: deterministic, no
    /// process-global env mutation (so it cannot race
    /// `child_session_marker_detection_follows_env_presence`, which does mutate
    /// it), and identical on every platform.
    #[test]
    fn strip_marks_every_marker_for_removal_on_both_command_types() {
        let mut std_cmd = std::process::Command::new("echo");
        std_cmd.strip_inherited_claude_markers();
        assert_all_markers_removed(&std_cmd, "std::process::Command");

        let mut tokio_cmd = tokio::process::Command::new("echo");
        tokio_cmd.strip_inherited_claude_markers();
        assert_all_markers_removed(tokio_cmd.as_std(), "tokio::process::Command");
    }

    /// The strip must OVERRIDE an earlier explicit set on the same builder.
    ///
    /// Guards the ordering contract the spawn sites rely on: the strip sits in
    /// the middle of a builder chain, so it has to win against anything set
    /// before it. (Anything set *after* — notably `ExtraEnv`, which runs last —
    /// is meant to win, and that escape hatch is deliberate.)
    #[test]
    fn strip_overrides_a_marker_set_earlier_in_the_chain() {
        let mut cmd = std::process::Command::new("echo");
        for marker in INHERITED_CLAUDE_MARKERS {
            cmd.env(marker, "1");
        }
        cmd.strip_inherited_claude_markers();
        assert_all_markers_removed(&cmd, "marker set before the strip");
    }

    /// Collect every `.rs` file under `src/`.
    fn source_files() -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// No spawn site may open-code the strip — they must all call the helper.
    ///
    /// This is the durable half of the fix. The `CLAUDE_CODE_CHILD_SESSION`
    /// leak existed because the strip rule lived in prose plus copy-pasted
    /// `env_remove` lines: six sites removed `CLAUDECODE`, none removed the
    /// sibling marker, and nothing could notice. A grep-level assertion is
    /// crude, but it is the only mechanism that fails when somebody adds a
    /// seventh spawn site by copying an existing one.
    #[test]
    fn every_spawn_site_goes_through_the_shared_strip() {
        // Built at runtime so this file never contains the literal it bans,
        // which would otherwise make the test match its own source.
        let mut needles: Vec<String> = INHERITED_CLAUDE_MARKERS
            .iter()
            .map(|m| format!("env_remove(\"{m}\")"))
            .collect();
        needles.push("env_remove(CLAUDECODE_ENV)".to_string());
        needles.push("env_remove(CLAUDE_CHILD_SESSION_ENV)".to_string());

        let mut offenders = Vec::new();
        let mut files_scanned = 0usize;
        let mut helper_call_sites = 0usize;

        for path in source_files() {
            // This module is where the strip is implemented, so it is the one
            // place the banned form legitimately appears.
            if path.file_name().and_then(|n| n.to_str()) == Some("claude_env.rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            files_scanned += 1;
            helper_call_sites += text.matches(".strip_inherited_claude_markers()").count();
            for (idx, line) in text.lines().enumerate() {
                if needles.iter().any(|n| line.contains(n.as_str())) {
                    offenders.push(format!("{}:{}", path.display(), idx + 1));
                }
            }
        }

        // Vacuity guards: an empty scan would report PASS while asserting
        // nothing. Both counts must be non-trivial for the result to mean
        // anything.
        assert!(
            files_scanned > 0,
            "scanned no sources — the assertion below would be vacuous"
        );
        assert!(
            helper_call_sites >= 5,
            "expected the shared strip at every spawn site, found {helper_call_sites} call(s) — \
             either the scan is broken or spawn sites stopped using it"
        );

        assert!(
            offenders.is_empty(),
            "these sites strip an inherited Claude marker by hand instead of calling \
             `strip_inherited_claude_markers()`, which is how CLAUDE_CODE_CHILD_SESSION came to \
             be missing from every site for months:\n  {}",
            offenders.join("\n  ")
        );
    }
}

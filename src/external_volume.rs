//! The external-volume contract, supervisor side.
//!
//! Plan `2026-08-07-external-storage-tiering-for-fleet-disk-pressure`, Phase 5.
//!
//! # This is a deliberate, flagged duplicate — read before extending it
//!
//! The reference implementation is `qontinui-runner`'s
//! `src-tauri/src/external_volume.rs`, and it is richer (it also serves the
//! CI-node admission gate). This file exists because the supervisor is a
//! separate crate that shares no code with the runner except
//! `qontinui-schemas`, and hoisting the contract into schemas would be a
//! cross-repo change the plan did not scope — one with a recorded fleet-wide
//! hazard (a stale sibling `qontinui-schemas` checkout breaks every dependent
//! build with errors that look like source errors).
//!
//! **What is genuinely shared is the CONTRACT, not the code**: the two
//! environment variable names, the sentinel filename, and the GUID
//! normalization rules below. Those three things must stay byte-identical
//! across both crates — if they drift, one guard admits a build the other
//! refuses, on the same volume, which is worse than having no guard.
//! Consolidating both onto one shared crate is named as a follow-up in the
//! plan; until then, **any edit here needs the same edit there**.
//!
//! The three-state semantics and the reason `Mismatched` may not collapse into
//! either neighbour are documented in full on the runner side; the short
//! version is that an un-mounted volume-GUID mount point is still an ordinary
//! writable directory on the internal disk, so `Path::exists` is not a
//! presence test — only the sentinel is.

use std::path::{Path, PathBuf};

/// MUST match the runner's `EXTERNAL_VOLUME_PATH_ENV`.
pub const EXTERNAL_VOLUME_PATH_ENV: &str = "QONTINUI_EXTERNAL_VOLUME_PATH";
/// MUST match the runner's `EXTERNAL_VOLUME_GUID_ENV`.
pub const EXTERNAL_VOLUME_GUID_ENV: &str = "QONTINUI_EXTERNAL_VOLUME_GUID";
/// MUST match the runner's `SENTINEL_FILENAME`.
pub const SENTINEL_FILENAME: &str = ".qontinui-volume-id";

/// Whether the declared external volume is provably mounted. Three-valued for
/// the same reason as the runner's twin: a WRONG volume is not a disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalVolumeState {
    Absent,
    Present,
    Mismatched { expected: String, found: String },
}

impl ExternalVolumeState {
    /// Operator-readable refusal, or `None` when the volume is usable.
    ///
    /// This `Option` **is** the sanctioned collapse to a boolean, and it is
    /// deliberately not a `bool`: a caller that wants to know "may I write
    /// here?" is forced to hold the reason it may not, so no refusal can reach
    /// an operator without saying which of the two failure modes it was. A bare
    /// presence predicate existed here briefly and was deleted — nothing used
    /// it, and it offered a way to refuse without being able to explain why.
    pub fn refusal_reason(&self, mount: &Path) -> Option<String> {
        match self {
            ExternalVolumeState::Present => None,
            ExternalVolumeState::Absent => Some(format!(
                "declared external volume at {} is NOT mounted (sentinel {} absent)",
                mount.display(),
                SENTINEL_FILENAME
            )),
            ExternalVolumeState::Mismatched { expected, found } => Some(format!(
                "WRONG volume mounted at {}: sentinel {} names {found}, expected {expected}",
                mount.display(),
                SENTINEL_FILENAME
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalVolume {
    pub mount: PathBuf,
    pub expected_guid: String,
}

/// Read the declaration from the environment. Both variables required — a path
/// with no expected GUID reduces the sentinel to an existence test, which is
/// exactly the test the un-mounted stub defeats.
pub fn declared() -> Option<ExternalVolume> {
    declared_from(
        std::env::var(EXTERNAL_VOLUME_PATH_ENV).ok().as_deref(),
        std::env::var(EXTERNAL_VOLUME_GUID_ENV).ok().as_deref(),
    )
}

fn declared_from(path: Option<&str>, guid: Option<&str>) -> Option<ExternalVolume> {
    let path = path?.trim();
    let guid = guid?.trim();
    if path.is_empty() || guid.is_empty() {
        return None;
    }
    Some(ExternalVolume {
        mount: PathBuf::from(path),
        expected_guid: guid.to_string(),
    })
}

impl ExternalVolume {
    pub fn probe(&self) -> ExternalVolumeState {
        match std::fs::read_to_string(self.mount.join(SENTINEL_FILENAME)) {
            Err(_) => ExternalVolumeState::Absent,
            Ok(found) => {
                if guid_eq(&found, &self.expected_guid) {
                    ExternalVolumeState::Present
                } else {
                    ExternalVolumeState::Mismatched {
                        expected: self.expected_guid.clone(),
                        found: found.trim().to_string(),
                    }
                }
            }
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        path_starts_with(path, &self.mount)
    }
}

/// `None` ⇒ not an external path (no declaration, or outside the mount) ⇒ the
/// caller keeps its existing fail-open behaviour unchanged.
pub fn external_state_for(path: &Path) -> Option<ExternalVolumeState> {
    let vol = declared()?;
    if !vol.contains(path) {
        return None;
    }
    Some(vol.probe())
}

fn guid_eq(a: &str, b: &str) -> bool {
    normalize_guid(a) == normalize_guid(b)
}

fn normalize_guid(s: &str) -> String {
    s.trim()
        .trim_start_matches("\\\\?\\Volume")
        .trim_matches(|c: char| c == '{' || c == '}' || c == '\\' || c.is_whitespace())
        .to_ascii_lowercase()
}

/// Case-insensitive on Windows — a case difference must not answer "not
/// external", because that routes an external path back to the fail-OPEN arm.
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    if cfg!(windows) {
        let norm = |p: &Path| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
                .collect::<Vec<_>>()
        };
        let (p, pre) = (norm(path), norm(prefix));
        pre.len() <= p.len() && p[..pre.len()] == pre[..]
    } else {
        path.starts_with(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_vars_required_and_blank_is_not_a_declaration() {
        assert!(declared_from(Some("D:/qontinui-ext"), Some("{abc}")).is_some());
        assert!(declared_from(Some("D:/qontinui-ext"), None).is_none());
        assert!(declared_from(None, Some("{abc}")).is_none());
        assert!(declared_from(Some("  "), Some("{abc}")).is_none());
        assert!(declared_from(Some("D:/qontinui-ext"), Some(" ")).is_none());
    }

    /// The cross-crate contract this file's header warns about. If the runner
    /// changes any of these three strings and this crate does not, one guard
    /// admits a build the other refuses on the same volume. Pinning them in a
    /// test at least makes the drift a red build rather than a silent
    /// divergence.
    #[test]
    fn the_shared_contract_strings_are_what_the_runner_uses() {
        assert_eq!(EXTERNAL_VOLUME_PATH_ENV, "QONTINUI_EXTERNAL_VOLUME_PATH");
        assert_eq!(EXTERNAL_VOLUME_GUID_ENV, "QONTINUI_EXTERNAL_VOLUME_GUID");
        assert_eq!(SENTINEL_FILENAME, ".qontinui-volume-id");
    }

    #[test]
    fn guid_compare_ignores_punctuation_and_case() {
        let bare = "d913fcde-1111-2222-3333-444455556666";
        assert!(guid_eq(bare, "{D913FCDE-1111-2222-3333-444455556666}"));
        assert!(guid_eq(
            "\\\\?\\Volume{d913fcde-1111-2222-3333-444455556666}\\",
            bare
        ));
        assert!(!guid_eq(bare, "{ffffffff-1111-2222-3333-444455556666}"));
    }

    #[test]
    fn containment_is_component_wise_not_string_prefix() {
        let v = ExternalVolume {
            mount: PathBuf::from("D:/qontinui-ext"),
            expected_guid: "{abc}".into(),
        };
        assert!(v.contains(Path::new("D:/qontinui-ext/target-pool")));
        assert!(!v.contains(Path::new("D:/qontinui-external-decoy")));
        assert!(!v.contains(Path::new("D:/qontinui-root/qontinui-runner")));
    }

    #[test]
    fn missing_sentinel_is_absent_even_though_the_stub_dir_exists() {
        let dir = std::env::temp_dir().join("qontinui-sup-extvol-absent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists(), "the stub exists — that is the trap");
        let v = ExternalVolume {
            mount: dir.clone(),
            expected_guid: "{d913fcde}".into(),
        };
        assert_eq!(v.probe(), ExternalVolumeState::Absent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_sentinel_refuses_and_is_distinct_from_absent() {
        let dir = std::env::temp_dir().join("qontinui-sup-extvol-mismatch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SENTINEL_FILENAME), "{ffffffff-dead}").unwrap();
        let v = ExternalVolume {
            mount: dir.clone(),
            expected_guid: "{d913fcde}".into(),
        };
        let s = v.probe();
        assert!(
            s.refusal_reason(&dir).is_some(),
            "a wrong volume must refuse"
        );
        assert_ne!(s, ExternalVolumeState::Absent);
        assert!(v
            .probe()
            .refusal_reason(&dir)
            .unwrap()
            .contains("WRONG volume"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

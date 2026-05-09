//! Machine identity loader for coord build-event reporting.
//!
//! Mirrors `qontinui-runner/src-tauri/src/bin/qontinui_profile.rs::machine_path()`
//! exactly: reads `~/.qontinui/machine.json`, parses JSON, extracts the
//! `machine_id` UUID string. The supervisor uses this to attach a machine
//! identity to its `coord.build_events` POSTs (see plan
//! `coord-tinderbox-build-status-2026-05-09.md`, Phase B2).
//!
//! Identity is **read-only** from the supervisor's perspective — it is minted
//! runner-side by `qontinui_profile machine init`. Absent file means coord
//! reporting is disabled (graceful no-op); we never mint a UUID here, because
//! `coord.machines` requires the FK to be pre-registered.

use std::path::Path;

/// Default machine.json path inside the user's home dir.
fn default_machine_json_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("machine.json"))
}

/// Load the machine UUID from `~/.qontinui/machine.json`.
///
/// Returns `None` for any failure — file missing, malformed JSON, missing
/// `machine_id` field, invalid UUID. Callers treat `None` as "coord
/// reporting disabled."
pub fn load_machine_id() -> Option<uuid::Uuid> {
    let path = default_machine_json_path()?;
    load_machine_id_from(&path)
}

/// Inner helper for testability — accepts an explicit path.
fn load_machine_id_from(path: &Path) -> Option<uuid::Uuid> {
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let id_str = v.get("machine_id")?.as_str()?;
    uuid::Uuid::parse_str(id_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn returns_none_for_missing_file() {
        let nonexistent = PathBuf::from("/definitely/does/not/exist/machine.json");
        assert!(load_machine_id_from(&nonexistent).is_none());
    }

    #[test]
    fn returns_none_for_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_machine_id_from(&path).is_none());
    }

    #[test]
    fn returns_none_for_missing_machine_id_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine.json");
        std::fs::write(&path, br#"{"hostname": "foo"}"#).unwrap();
        assert!(load_machine_id_from(&path).is_none());
    }

    #[test]
    fn returns_none_for_invalid_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine.json");
        std::fs::write(&path, br#"{"machine_id": "not-a-uuid"}"#).unwrap();
        assert!(load_machine_id_from(&path).is_none());
    }

    #[test]
    fn returns_uuid_for_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine.json");
        let valid_uuid = "00000000-0000-4000-8000-000000000000";
        std::fs::write(
            &path,
            format!(r#"{{"machine_id": "{}", "hostname": "test"}}"#, valid_uuid),
        )
        .unwrap();
        let loaded = load_machine_id_from(&path).expect("should parse");
        assert_eq!(loaded.to_string(), valid_uuid);
    }
}

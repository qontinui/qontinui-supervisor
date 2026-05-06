use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::config::{RunnerConfig, SupervisorConfig};

#[derive(Serialize, Deserialize, Default)]
pub struct PersistentSettings {
    pub ai_provider: Option<String>,
    pub ai_model: Option<String>,
    pub auto_debug_enabled: Option<bool>,
    /// Multi-runner configurations. Empty means use default single primary runner.
    #[serde(default)]
    pub runners: Vec<RunnerConfig>,
}

pub fn settings_path(config: &SupervisorConfig) -> PathBuf {
    config.dev_logs_dir.join("supervisor-settings.json")
}

pub fn load_settings(path: &Path) -> PersistentSettings {
    // Best-effort one-shot rewrite of pre-RunnerKind on-disk shape. NEVER
    // fails the load — any IO/parse error inside the migrator falls through
    // and the read+deserialize below proceeds against whatever's there.
    migrate_settings(path);

    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => PersistentSettings::default(),
    }
}

pub fn save_settings(path: &Path, settings: &PersistentSettings) {
    if let Err(e) = try_save_settings(path, settings) {
        warn!("Failed to save settings: {}", e);
    }
}

/// Save settings, returning an error on failure instead of logging a warning.
pub fn try_save_settings(path: &Path, settings: &PersistentSettings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(settings).map_err(|e| format!("serialize: {e}"))?;
    crate::fs_atomic::atomic_write(path, json.as_bytes())
        .map_err(|e| format!("write {:?}: {e}", path))?;
    Ok(())
}

/// Idempotent on-disk rewrite of pre-`RunnerKind` settings shape.
///
/// Walks `runners[]` and, for each entry that has `is_primary` but no `kind`,
/// derives a `RunnerKind`-shaped `kind` (per shape A: `{"type": "primary"}` etc.)
/// from the legacy probe + id prefix and strips `is_primary`. If no entry
/// needs rewriting the file is left untouched (zero-cost no-op path).
///
/// Failure mode: best-effort. Any IO/parse error logs `warn!` and returns —
/// the migrator MUST NOT block `load_settings`.
fn migrate_settings(path: &Path) {
    // 1. Read raw JSON. Missing file is a normal no-op; other read errors warn.
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!("settings: migrate skipped, cannot read {:?}: {e}", path);
            return;
        }
    };

    // 2. Parse as Value so we can rewrite without round-tripping through the
    //    typed `PersistentSettings` (which would silently apply serde defaults
    //    and lose the "old shape" signal we're keying off).
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!("settings: migrate skipped, cannot parse {:?}: {e}", path);
            return;
        }
    };

    // 3. Locate the runners array. Anything else (top-level not-an-object,
    //    no `runners` key, `runners` not an array) is a no-op.
    let runners = match value.get_mut("runners").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr,
        None => return,
    };

    let mut rewritten = 0usize;
    for entry in runners.iter_mut() {
        let obj = match entry.as_object_mut() {
            Some(o) => o,
            None => continue,
        };

        // Only migrate entries that have `is_primary` AND no `kind` field.
        let has_is_primary = obj.contains_key("is_primary");
        let has_kind = obj.contains_key("kind");
        if !has_is_primary || has_kind {
            continue;
        }

        // Derive the kind variant. `is_primary: true` always wins; otherwise
        // dispatch on the id prefix.
        let is_primary = obj
            .get("is_primary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let kind_value = if is_primary || id == "primary" {
            serde_json::json!({"type": "primary"})
        } else if id.starts_with("test-") {
            serde_json::json!({"type": "temp", "id": id})
        } else if id.starts_with("named-") {
            serde_json::json!({"type": "named", "name": name})
        } else {
            serde_json::json!({"type": "external"})
        };

        obj.insert("kind".to_string(), kind_value);
        obj.remove("is_primary");
        rewritten += 1;
    }

    // 4. Idempotency: if nothing changed, don't touch disk.
    if rewritten == 0 {
        return;
    }

    // 5. Re-serialize and atomically replace.
    let pretty = match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => {
            warn!("settings: migrate skipped, cannot serialize: {e}");
            return;
        }
    };
    if let Err(e) = crate::fs_atomic::atomic_write(path, pretty.as_bytes()) {
        warn!("settings: migrate skipped, cannot atomic_write {:?}: {e}", path);
        return;
    }

    info!(
        "settings: migrated {} runner entries from is_primary -> kind",
        rewritten
    );
}

// --- Runner CRUD helpers ---

/// Add a runner config to persistent settings.
pub fn add_runner(path: &Path, config: &RunnerConfig) {
    let mut settings = load_settings(path);
    settings.runners.push(config.clone());
    save_settings(path, &settings);
}

/// Remove a runner config by ID from persistent settings.
pub fn remove_runner(path: &Path, id: &str) {
    let mut settings = load_settings(path);
    settings.runners.retain(|r| r.id != id);
    save_settings(path, &settings);
}

/// Update a runner config by ID in persistent settings.
#[allow(dead_code)]
pub fn update_runner(path: &Path, config: &RunnerConfig) {
    let mut settings = load_settings(path);
    if let Some(existing) = settings.runners.iter_mut().find(|r| r.id == config.id) {
        *existing = config.clone();
    }
    save_settings(path, &settings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    fn read_value(path: &Path) -> serde_json::Value {
        let s = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn migrate_settings_handles_primary_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "ai_provider": "claude",
              "runners": [
                {
                  "id": "primary",
                  "name": "Primary",
                  "port": 9876,
                  "is_primary": true,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );

        migrate_settings(&path);

        let v = read_value(&path);
        let entry = &v["runners"][0];
        assert!(
            entry.get("is_primary").is_none(),
            "is_primary should be stripped, got: {entry}"
        );
        assert_eq!(entry["kind"], serde_json::json!({"type": "primary"}));
    }

    #[test]
    fn migrate_settings_handles_named_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "runners": [
                {
                  "id": "named-9879-19dcb942443-4",
                  "name": "target",
                  "port": 9879,
                  "is_primary": false,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );

        migrate_settings(&path);

        let v = read_value(&path);
        let entry = &v["runners"][0];
        assert!(entry.get("is_primary").is_none());
        assert_eq!(
            entry["kind"],
            serde_json::json!({"type": "named", "name": "target"})
        );
    }

    #[test]
    fn migrate_settings_handles_temp_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "runners": [
                {
                  "id": "test-abc123",
                  "name": "test-9877",
                  "port": 9877,
                  "is_primary": false,
                  "protected": false,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );

        migrate_settings(&path);

        let v = read_value(&path);
        let entry = &v["runners"][0];
        assert!(entry.get("is_primary").is_none());
        assert_eq!(
            entry["kind"],
            serde_json::json!({"type": "temp", "id": "test-abc123"})
        );
    }

    #[test]
    fn migrate_settings_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "runners": [
                {
                  "id": "primary",
                  "name": "Primary",
                  "port": 9876,
                  "is_primary": true,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                },
                {
                  "id": "named-9879-foo",
                  "name": "target",
                  "port": 9879,
                  "is_primary": false,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );

        // First run rewrites the file.
        migrate_settings(&path);
        let after_first = std::fs::read_to_string(&path).unwrap();
        let mtime_first = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Second run must be a no-op (no rewrite, mtime unchanged, content
        // unchanged).
        std::thread::sleep(std::time::Duration::from_millis(20));
        migrate_settings(&path);
        let after_second = std::fs::read_to_string(&path).unwrap();
        let mtime_second = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(
            after_first, after_second,
            "second migrate must not change file contents"
        );
        assert_eq!(
            mtime_first, mtime_second,
            "second migrate must not touch mtime"
        );
    }

    #[test]
    fn migrate_settings_no_op_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        // Must not panic, must not create the file.
        migrate_settings(&path);
        assert!(!path.exists());
    }

    #[test]
    fn migrate_settings_no_op_when_already_migrated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "runners": [
                {
                  "id": "primary",
                  "name": "Primary",
                  "port": 9876,
                  "kind": {"type": "primary"},
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );
        let before = std::fs::read_to_string(&path).unwrap();
        migrate_settings(&path);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn load_settings_after_migration_round_trips_to_runner_kind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("supervisor-settings.json");
        write_fixture(
            &path,
            r#"{
              "runners": [
                {
                  "id": "primary",
                  "name": "Primary",
                  "port": 9876,
                  "is_primary": true,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                },
                {
                  "id": "named-9879-x",
                  "name": "target",
                  "port": 9879,
                  "is_primary": false,
                  "protected": true,
                  "server_mode": false,
                  "extra_env": {}
                }
              ]
            }"#,
        );

        let settings = load_settings(&path);
        assert_eq!(settings.runners.len(), 2);
        assert!(settings.runners[0].kind().is_primary());
        assert!(settings.runners[1].kind().is_named());
    }
}

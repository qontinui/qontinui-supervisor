use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Basename a legacy flat settings file is migrate-claimed by. The flat
/// `dev_logs_dir/supervisor-settings.json` historically belonged to the
/// DEFAULT live instance, whose `--project-dir` basename (the runner npm dir)
/// is `qontinui-runner`. ONLY an instance with this basename inherits the
/// legacy registry; every other instance (worktree / isolated test) starts
/// with a fresh empty registry. Inheriting it on a non-default instance is
/// exactly the cross-instance registry bleed this module fixes.
const DEFAULT_INSTANCE_BASENAME: &str = "qontinui-runner";

/// Derive the per-instance key used to namespace this supervisor instance's
/// mutable state under `dev_logs_dir/instances/<key>/`.
///
/// Shape: `<project-dir-basename>-<8-hex-of-canonical-abs-project-dir>`.
/// The basename (the runner npm dir name — `qontinui-runner`,
/// `qontinui-runner-wt-e2e`, …) is for human readability; the 8-hex hash of
/// the **canonicalized absolute project_dir** is for collision-proofing so two
/// same-named project dirs under different parents never share a key.
///
/// Canonicalization is best-effort: if `canonicalize()` fails (path doesn't
/// exist yet, permission error, …) we hash the raw absolute path string
/// instead. This NEVER panics.
pub fn instance_key(config: &SupervisorConfig) -> String {
    let project_dir = &config.project_dir;

    // The instance is keyed off the runner npm dir (parent of src-tauri), so a
    // `--project-dir .../qontinui-runner/src-tauri` yields the basename
    // `qontinui-runner`. Fall back to the project_dir basename, then to a
    // literal "instance", so the key is always well-formed.
    let basename = project_dir
        .parent()
        .and_then(|p| p.file_name())
        .or_else(|| project_dir.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "instance".to_string());

    // Hash the canonicalized absolute path. Best-effort canonicalize; on
    // failure hash the raw absolute-ish path string. The hash MUST be stable
    // for the same path across runs, so we hash the string form (not OS inode).
    let canonical = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.clone());
    let canonical = strip_verbatim_prefix(canonical);
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let short_hash = hex::encode(&digest[..4]); // 8 hex chars

    format!("{basename}-{short_hash}")
}

/// Strip Windows' `\\?\` verbatim prefix so the hashed path string matches the
/// plain absolute form a human (or another code path) would compute. Mirrors
/// `config::strip_verbatim_prefix`; kept local so settings has no cross-module
/// coupling for a one-liner. No-op on non-Windows.
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    match path.to_str() {
        Some(s) if s.starts_with(VERBATIM_UNC) => path,
        Some(s) => match s.strip_prefix(VERBATIM) {
            Some(stripped) => PathBuf::from(stripped),
            None => path,
        },
        None => path,
    }
}

#[cfg(not(windows))]
#[inline]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// Directory holding this instance's mutable state, namespaced per instance:
/// `dev_logs_dir/instances/<instance-key>/`. Logs stay shared in
/// `dev_logs_dir/` (intentional — operator-friendly); only instance STATE
/// (the runner registry + AI config) moves here.
pub fn instance_state_dir(config: &SupervisorConfig) -> PathBuf {
    config
        .dev_logs_dir
        .join("instances")
        .join(instance_key(config))
}

/// Per-instance supervisor settings path:
/// `dev_logs_dir/instances/<instance-key>/supervisor-settings.json`.
///
/// This is **per supervisor instance**, not flat under `dev_logs_dir`. Two
/// supervisors managing different project dirs (e.g. the live
/// `qontinui-runner` and an isolated `qontinui-runner-wt-e2e`) compute the same
/// grandparent `dev_logs_dir` but DIFFERENT instance keys, so their runner
/// registries no longer bleed into each other (the 2026-06-05 E2E leak).
///
/// As a best-effort side effect this:
///  1. creates the instance state dir, and
///  2. one-shot migrates the legacy flat `dev_logs_dir/supervisor-settings.json`
///     INTO this instance's path — but ONLY when this instance is the historical
///     default (`qontinui-runner` basename), so the live registry follows the
///     live instance and every other instance starts fresh+empty. See
///     [`migrate_legacy_settings`].
///
/// Both side effects are idempotent and never fail the caller; on any IO error
/// they log a warning and fall through to returning the per-instance path.
pub fn settings_path(config: &SupervisorConfig) -> PathBuf {
    let dir = instance_state_dir(config);
    let path = dir.join("supervisor-settings.json");

    // Best-effort: ensure the dir exists so the first save() succeeds.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "settings: could not create instance state dir {:?}: {e}",
            dir
        );
    }

    // Best-effort one-shot legacy migration. No-op once the per-instance file
    // exists, or when this instance is not the default.
    migrate_legacy_settings(config, &path);

    path
}

/// One-shot migration of the legacy flat `dev_logs_dir/supervisor-settings.json`
/// into the per-instance path.
///
/// Migration rule (the crux of the isolation fix): the legacy file's contents
/// belong to the DEFAULT live instance (`qontinui-runner`). So we ONLY
/// migrate-claim it when this instance's basename is the historical default
/// [`DEFAULT_INSTANCE_BASENAME`]. Any other instance (worktree, isolated E2E)
/// is left with NO per-instance file → `load_settings` returns an empty
/// registry → it does NOT inherit the live instance's runners. Inheriting them
/// is exactly the bug.
///
/// We **copy** the legacy file and leave it in place, dropping a
/// `supervisor-settings.json.migrated-to-<key>` marker next to it (so a human
/// auditing `.dev-logs/` can see where it went and that the copy is one-shot).
/// Leaving the legacy file untouched is the conservative choice: an older
/// supervisor binary that still reads the flat path keeps working.
///
/// Best-effort throughout: any IO/parse error logs `warn!`/`info!` and returns.
/// NEVER fails the load.
fn migrate_legacy_settings(config: &SupervisorConfig, instance_path: &Path) {
    // Already migrated / instance already has state → nothing to do.
    if instance_path.exists() {
        return;
    }

    // Only the default instance may claim the legacy flat file.
    let basename = config
        .project_dir
        .parent()
        .and_then(|p| p.file_name())
        .or_else(|| config.project_dir.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if basename != DEFAULT_INSTANCE_BASENAME {
        return;
    }

    let legacy_path = config.dev_logs_dir.join("supervisor-settings.json");
    if !legacy_path.exists() {
        return;
    }

    let contents = match std::fs::read(&legacy_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "settings: legacy migration skipped, cannot read {:?}: {e}",
                legacy_path
            );
            return;
        }
    };

    if let Err(e) = crate::fs_atomic::atomic_write(instance_path, &contents) {
        warn!(
            "settings: legacy migration skipped, cannot write {:?}: {e}",
            instance_path
        );
        return;
    }

    // Drop a breadcrumb marker next to the legacy file (best-effort). The
    // legacy file itself is intentionally left in place.
    let marker = config.dev_logs_dir.join(format!(
        "supervisor-settings.json.migrated-to-{}",
        instance_key(config)
    ));
    let _ = std::fs::write(
        &marker,
        format!("migrated to {}\n", instance_path.display()),
    );

    info!(
        "settings: migrated legacy registry {:?} -> per-instance {:?}",
        legacy_path, instance_path
    );
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
        warn!(
            "settings: migrate skipped, cannot atomic_write {:?}: {e}",
            path
        );
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

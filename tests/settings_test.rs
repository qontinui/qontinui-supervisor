use qontinui_supervisor::settings::{load_settings, save_settings, PersistentSettings};
use tempfile::TempDir;

#[test]
fn test_load_missing_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.json");
    let settings = load_settings(&path);
    assert!(settings.ai_provider.is_none());
    assert!(settings.ai_model.is_none());
    assert!(settings.auto_debug_enabled.is_none());
}

#[test]
fn test_load_invalid_json() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, "not json at all {{{").unwrap();
    let settings = load_settings(&path);
    // Should return defaults instead of panicking
    assert!(settings.ai_provider.is_none());
    assert!(settings.ai_model.is_none());
    assert!(settings.auto_debug_enabled.is_none());
}

#[test]
fn test_save_and_load_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let settings = PersistentSettings {
        ai_provider: Some("claude".to_string()),
        ai_model: Some("opus".to_string()),
        auto_debug_enabled: Some(true),
        runners: vec![],
    };

    save_settings(&path, &settings);
    let loaded = load_settings(&path);

    assert_eq!(loaded.ai_provider, Some("claude".to_string()));
    assert_eq!(loaded.ai_model, Some("opus".to_string()));
    assert_eq!(loaded.auto_debug_enabled, Some(true));
}

#[test]
fn test_partial_settings() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("partial.json");

    // Write JSON with only one field set
    std::fs::write(&path, r#"{"ai_provider": "gemini"}"#).unwrap();
    let loaded = load_settings(&path);

    assert_eq!(loaded.ai_provider, Some("gemini".to_string()));
    assert!(loaded.ai_model.is_none());
    assert!(loaded.auto_debug_enabled.is_none());
}

use qontinui_supervisor::config::{RunnerConfig, SupervisorConfig};
use std::path::{Path, PathBuf};

/// Build a `SupervisorConfig` with the given project dir + dev-logs dir.
/// All other fields are inert for settings-path purposes.
fn make_config(project_dir: PathBuf, dev_logs_dir: PathBuf) -> SupervisorConfig {
    SupervisorConfig {
        project_dir,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        log_dir: None,
        port: 9875,
        dev_logs_dir,
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
        runners: vec![RunnerConfig::default_primary()],
        build_pool: qontinui_supervisor::config::BuildPoolConfig { pool_size: 1 },
        no_prewarm: false,
        no_webview: true,
    }
}

#[test]
fn test_settings_path_is_per_instance_under_instances_dir() {
    use qontinui_supervisor::settings::{instance_key, settings_path};

    let dir = TempDir::new().unwrap();
    let dev_logs = dir.path().join(".dev-logs");
    let project_dir = dir.path().join("qontinui-runner").join("src-tauri");
    let config = make_config(project_dir, dev_logs.clone());

    let key = instance_key(&config);
    let path = settings_path(&config);

    assert_eq!(
        path,
        dev_logs
            .join("instances")
            .join(&key)
            .join("supervisor-settings.json"),
        "settings path must be namespaced under instances/<key>/"
    );
    // The instance state dir is created as a side effect.
    assert!(path.parent().unwrap().is_dir());
}

#[test]
fn test_instance_key_shape_default_instance() {
    use qontinui_supervisor::settings::instance_key;

    let dir = TempDir::new().unwrap();
    let project_dir = dir.path().join("qontinui-runner").join("src-tauri");
    let config = make_config(project_dir, dir.path().join(".dev-logs"));

    let key = instance_key(&config);
    // basename-<8hex>
    assert!(
        key.starts_with("qontinui-runner-"),
        "key should start with the project dir basename, got {key}"
    );
    let hash = key.rsplit('-').next().unwrap();
    assert_eq!(hash.len(), 8, "hash suffix must be 8 hex chars, got {key}");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash suffix must be hex, got {key}"
    );
}

#[test]
fn test_instance_key_stable_for_same_path() {
    use qontinui_supervisor::settings::instance_key;

    let dir = TempDir::new().unwrap();
    let project_dir = dir.path().join("qontinui-runner").join("src-tauri");
    let config = make_config(project_dir, dir.path().join(".dev-logs"));

    let a = instance_key(&config);
    let b = instance_key(&config);
    assert_eq!(a, b, "instance key must be stable for the same path");
}

#[test]
fn test_instance_key_differs_same_basename_different_parent() {
    use qontinui_supervisor::settings::instance_key;

    // Two project dirs with the SAME basename but DIFFERENT parents must get
    // different keys (the collision-proofing the hash exists for).
    let root = TempDir::new().unwrap();
    let p1 = root
        .path()
        .join("a")
        .join("qontinui-runner")
        .join("src-tauri");
    let p2 = root
        .path()
        .join("b")
        .join("qontinui-runner")
        .join("src-tauri");
    std::fs::create_dir_all(&p1).unwrap();
    std::fs::create_dir_all(&p2).unwrap();

    let c1 = make_config(p1, root.path().join(".dev-logs"));
    let c2 = make_config(p2, root.path().join(".dev-logs"));

    let k1 = instance_key(&c1);
    let k2 = instance_key(&c2);
    assert_ne!(
        k1, k2,
        "same-basename different-parent must yield different keys"
    );
    // Same human-readable basename prefix though.
    assert!(k1.starts_with("qontinui-runner-"));
    assert!(k2.starts_with("qontinui-runner-"));
}

#[test]
fn test_instance_key_no_panic_on_nonexistent_path() {
    use qontinui_supervisor::settings::instance_key;

    // Path that does not exist — canonicalize() fails, must fall back, not panic.
    let config = make_config(
        PathBuf::from("/no/such/place/qontinui-runner/src-tauri"),
        PathBuf::from("/no/such/place/.dev-logs"),
    );
    let key = instance_key(&config);
    assert!(key.starts_with("qontinui-runner-"), "got {key}");
}

// --- Migration matrix ---

fn write_legacy(dev_logs: &Path, body: &str) {
    std::fs::create_dir_all(dev_logs).unwrap();
    std::fs::write(dev_logs.join("supervisor-settings.json"), body).unwrap();
}

const LEGACY_REGISTRY_JSON: &str = r#"{
  "ai_provider": "claude",
  "runners": [
    { "id": "primary", "name": "Primary", "port": 9876, "kind": {"type": "primary"}, "protected": true, "server_mode": false, "extra_env": {} },
    { "id": "named-9879-x", "name": "live-target", "port": 9879, "kind": {"type": "named", "name": "live-target"}, "protected": true, "server_mode": false, "extra_env": {} }
  ]
}"#;

#[test]
fn test_migration_default_instance_claims_legacy_registry() {
    use qontinui_supervisor::settings::{load_settings, settings_path};

    let dir = TempDir::new().unwrap();
    let dev_logs = dir.path().join(".dev-logs");
    write_legacy(&dev_logs, LEGACY_REGISTRY_JSON);

    // Default instance: project dir basename == qontinui-runner.
    let project_dir = dir.path().join("qontinui-runner").join("src-tauri");
    let config = make_config(project_dir, dev_logs.clone());

    let path = settings_path(&config);
    // The legacy registry is migrated into the per-instance file.
    assert!(path.exists(), "default instance must claim the legacy file");
    let loaded = load_settings(&path);
    assert_eq!(
        loaded.runners.len(),
        2,
        "live instance must KEEP all its runners via migration"
    );
    assert_eq!(loaded.ai_provider.as_deref(), Some("claude"));

    // Legacy file is left in place (conservative; older binaries keep working).
    assert!(dev_logs.join("supervisor-settings.json").exists());
    // Breadcrumb marker dropped.
    let markers: Vec<_> = std::fs::read_dir(&dev_logs)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("supervisor-settings.json.migrated-to-")
        })
        .collect();
    assert_eq!(markers.len(), 1, "exactly one migration marker expected");
}

#[test]
fn test_migration_other_instance_does_not_inherit_live_registry() {
    use qontinui_supervisor::settings::{load_settings, settings_path};

    let dir = TempDir::new().unwrap();
    let dev_logs = dir.path().join(".dev-logs");
    write_legacy(&dev_logs, LEGACY_REGISTRY_JSON);

    // Non-default instance: a worktree project dir.
    let project_dir = dir.path().join("qontinui-runner-wt-e2e").join("src-tauri");
    let config = make_config(project_dir, dev_logs.clone());

    let path = settings_path(&config);
    // No per-instance file is created from the legacy registry...
    assert!(
        !path.exists(),
        "non-default instance must NOT claim the legacy registry"
    );
    // ...so it starts with a fresh empty registry (the isolation guarantee).
    let loaded = load_settings(&path);
    assert!(
        loaded.runners.is_empty(),
        "non-default instance must start with an empty registry, got {} runners",
        loaded.runners.len()
    );
    // Legacy file untouched.
    assert!(dev_logs.join("supervisor-settings.json").exists());
}

#[test]
fn test_migration_skipped_when_per_instance_file_exists() {
    use qontinui_supervisor::settings::{load_settings, save_settings, settings_path};

    let dir = TempDir::new().unwrap();
    let dev_logs = dir.path().join(".dev-logs");
    write_legacy(&dev_logs, LEGACY_REGISTRY_JSON);

    let project_dir = dir.path().join("qontinui-runner").join("src-tauri");
    let config = make_config(project_dir, dev_logs.clone());

    // Pre-seed a DIFFERENT per-instance file (1 runner, distinct provider).
    let path =
        qontinui_supervisor::settings::instance_state_dir(&config).join("supervisor-settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pre = PersistentSettings {
        ai_provider: Some("gemini".to_string()),
        ai_model: None,
        auto_debug_enabled: None,
        runners: vec![RunnerConfig::default_primary()],
    };
    save_settings(&path, &pre);

    // settings_path must NOT overwrite the existing per-instance file with the
    // legacy registry.
    let resolved = settings_path(&config);
    assert_eq!(resolved, path);
    let loaded = load_settings(&resolved);
    assert_eq!(loaded.runners.len(), 1, "existing per-instance file wins");
    assert_eq!(loaded.ai_provider.as_deref(), Some("gemini"));
}

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

#[test]
fn test_settings_path() {
    use qontinui_supervisor::config::SupervisorConfig;
    use qontinui_supervisor::settings::settings_path;
    use std::path::PathBuf;

    let config = SupervisorConfig {
        project_dir: PathBuf::from("/tmp/project/src-tauri"),
        dev_mode: false,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        port: 9875,
        dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
    };

    let path = settings_path(&config);
    assert_eq!(
        path,
        PathBuf::from("/tmp/.dev-logs/supervisor-settings.json")
    );
}

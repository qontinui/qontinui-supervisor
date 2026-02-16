use qontinui_supervisor::config::*;

#[test]
fn test_default_ports() {
    assert_eq!(DEFAULT_SUPERVISOR_PORT, 9875);
    assert_eq!(RUNNER_API_PORT, 9876);
    assert_eq!(EXPO_PORT, 8081);
}

#[test]
fn test_runner_vite_port() {
    assert_eq!(RUNNER_VITE_PORT, 1420);
}

#[test]
fn test_config_from_fields() {
    use std::path::PathBuf;

    let config = SupervisorConfig {
        project_dir: PathBuf::from("/tmp/runner/src-tauri"),
        dev_mode: true,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        port: 9875,
        dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
        cli_args: vec!["supervisor".to_string()],
        expo_dir: None,
        expo_port: 8081,
    };

    assert!(config.dev_mode);
    assert!(!config.watchdog_enabled_at_start);
    assert!(!config.auto_start);
    assert_eq!(config.port, 9875);
    assert_eq!(config.expo_port, 8081);
    assert!(config.expo_dir.is_none());
}

#[test]
fn test_config_runner_exe_path() {
    use std::path::PathBuf;

    let config = SupervisorConfig {
        project_dir: PathBuf::from("/tmp/runner/src-tauri"),
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

    let exe_path = config.runner_exe_path();
    assert!(exe_path.ends_with("qontinui-runner.exe"));
    assert!(exe_path.to_string_lossy().contains("target"));
    assert!(exe_path.to_string_lossy().contains("debug"));
}

#[test]
fn test_config_runner_npm_dir() {
    use std::path::PathBuf;

    let config = SupervisorConfig {
        project_dir: PathBuf::from("/tmp/runner/src-tauri"),
        dev_mode: true,
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

    let npm_dir = config.runner_npm_dir();
    assert_eq!(npm_dir, PathBuf::from("/tmp/runner"));
}

#[test]
fn test_ai_models_defined() {
    assert!(!AI_MODELS.is_empty());
    // Verify each model has all fields
    for (provider, key, model_id, display_name) in AI_MODELS {
        assert!(!provider.is_empty());
        assert!(!key.is_empty());
        assert!(!model_id.is_empty());
        assert!(!display_name.is_empty());
    }
}

#[test]
fn test_service_ports_defined() {
    assert!(!SERVICE_PORTS.is_empty());
    // Check that known services exist
    let names: Vec<&str> = SERVICE_PORTS.iter().map(|(name, _)| *name).collect();
    assert!(names.contains(&"postgresql"));
    assert!(names.contains(&"redis"));
    assert!(names.contains(&"backend"));
    assert!(names.contains(&"frontend"));
}

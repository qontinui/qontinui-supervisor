use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::{RunnerConfig, SupervisorConfig};

/// One target rectangle for placing a spawned temp runner's window.
/// Coordinates are absolute virtual-desktop pixels (Windows convention),
/// so a left-of-primary monitor uses negative X.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MonitorConfig {
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Default 3-monitor side-spawn layout: left + right enabled, middle disabled.
/// Assumes 1920x1080 displays with the primary in the middle. Override via the
/// dashboard if your geometry differs — this is just a starting point.
fn default_spawn_monitors() -> Vec<MonitorConfig> {
    vec![
        MonitorConfig {
            label: "Left".into(),
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
            enabled: true,
        },
        MonitorConfig {
            label: "Middle".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            enabled: false,
        },
        MonitorConfig {
            label: "Right".into(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            enabled: true,
        },
    ]
}

#[derive(Serialize, Deserialize)]
pub struct PersistentSettings {
    pub ai_provider: Option<String>,
    pub ai_model: Option<String>,
    pub auto_debug_enabled: Option<bool>,
    /// Multi-runner configurations. Empty means use default single primary runner.
    #[serde(default)]
    pub runners: Vec<RunnerConfig>,
    /// Where to place spawned temp runner windows. Round-robin'd across the
    /// enabled entries on each `spawn-test`.
    #[serde(default = "default_spawn_monitors")]
    pub spawn_monitors: Vec<MonitorConfig>,
}

impl Default for PersistentSettings {
    fn default() -> Self {
        Self {
            ai_provider: None,
            ai_model: None,
            auto_debug_enabled: None,
            runners: Vec::new(),
            spawn_monitors: default_spawn_monitors(),
        }
    }
}

pub fn settings_path(config: &SupervisorConfig) -> PathBuf {
    config.dev_logs_dir.join("supervisor-settings.json")
}

pub fn load_settings(path: &Path) -> PersistentSettings {
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
    std::fs::write(path, json).map_err(|e| format!("write {:?}: {e}", path))?;
    Ok(())
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

// --- Monitor config helpers ---

pub fn save_spawn_monitors(path: &Path, monitors: &[MonitorConfig]) -> Result<(), String> {
    let mut settings = load_settings(path);
    settings.spawn_monitors = monitors.to_vec();
    try_save_settings(path, &settings)
}

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

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
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => PersistentSettings::default(),
    }
}

pub fn save_settings(path: &Path, settings: &PersistentSettings) {
    match serde_json::to_string_pretty(settings) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                warn!("Failed to save settings to {:?}: {}", path, e);
            }
        }
        Err(e) => {
            warn!("Failed to serialize settings: {}", e);
        }
    }
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

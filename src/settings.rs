use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::SupervisorConfig;

#[derive(Serialize, Deserialize, Default)]
pub struct PersistentSettings {
    pub ai_provider: Option<String>,
    pub ai_model: Option<String>,
    pub auto_debug_enabled: Option<bool>,
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

//! Runner instance discovery.
//!
//! On startup, queries the primary runner's `/instances` endpoint to find
//! secondary instances that aren't registered with the supervisor, and
//! auto-registers them.

use std::sync::Arc;
use tracing::info;

use crate::config::RunnerConfig;
use crate::log_capture::{LogLevel, LogSource};
use crate::settings;
use crate::state::{ManagedRunner, SupervisorState};

/// Discover runner instances from the primary runner and auto-register any
/// that aren't already known to the supervisor.
pub async fn discover_runner_instances(state: &Arc<SupervisorState>) {
    let primary = match state.get_primary().await {
        Some(p) => p,
        None => return,
    };

    let primary_port = primary.config.port;

    // Wait for the primary runner's API to be responding (checked by health cache)
    // before attempting discovery. In dev mode this can take 3+ minutes.
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Runner discovery: waiting for primary runner on port {} to be ready...",
                primary_port
            ),
        )
        .await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let responding = primary.cached_health.read().await.runner_responding;
        if responding {
            break;
        }
        if std::time::Instant::now() > deadline {
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Info,
                    "Runner discovery: primary not ready after 5min, skipping".to_string(),
                )
                .await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let url = format!("http://127.0.0.1:{}/instances", primary_port);

    let instances: Vec<DiscoveredInstance> = match client.get(&url).send().await {
        Ok(resp) => match resp.json::<ApiResponse<Vec<DiscoveredInstance>>>().await {
            Ok(body) if body.success => body.data.unwrap_or_default(),
            _ => Vec::new(),
        },
        Err(e) => {
            info!("Runner discovery: failed to query /instances: {}", e);
            return;
        }
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Runner discovery: found {} instance(s) from primary",
                instances.len()
            ),
        )
        .await;

    if instances.is_empty() {
        return;
    }

    // Get currently known ports
    let known_ports: Vec<u16> = {
        let runners = state.runners.read().await;
        runners.values().map(|r| r.config.port).collect()
    };

    let mut registered = 0u32;
    for inst in &instances {
        if inst.is_self || inst.port < 1024 || known_ports.contains(&inst.port) {
            continue;
        }

        // Check if this instance is actually reachable
        if !inst.reachable {
            continue;
        }

        let name = inst
            .name
            .clone()
            .unwrap_or_else(|| format!("Runner :{}", inst.port));
        let id = format!("discovered-{}", inst.port);

        let runner_config = RunnerConfig {
            id: id.clone(),
            name: name.clone(),
            port: inst.port,
            is_primary: false,
            protected: false,
        };

        // Add to runtime state
        let managed = Arc::new(ManagedRunner::new(
            runner_config.clone(),
            state.config.watchdog_enabled_at_start,
        ));
        {
            let mut runners = state.runners.write().await;
            runners.insert(id.clone(), managed);
        }

        // Persist to settings
        let path = settings::settings_path(&state.config);
        settings::add_runner(&path, &runner_config);

        info!(
            "Runner discovery: auto-registered '{}' on port {}",
            name, inst.port
        );
        registered += 1;
    }

    if registered > 0 {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Runner discovery: auto-registered {} instance(s) from primary runner",
                    registered
                ),
            )
            .await;
        state.notify_health_change();
    }
}

#[derive(serde::Deserialize)]
struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
}

#[derive(serde::Deserialize)]
struct DiscoveredInstance {
    name: Option<String>,
    port: u16,
    is_self: bool,
    reachable: bool,
}

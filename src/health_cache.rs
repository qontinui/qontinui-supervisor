use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::config::RUNNER_VITE_PORT;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::is_temp_runner;
use crate::process::port;
use crate::state::SupervisorState;

/// Returns true if this runner is a named runner managed by the supervisor.
fn is_named_runner(runner_id: &str) -> bool {
    runner_id.starts_with("named-")
}

#[derive(Clone, Debug, Default)]
pub struct CachedPortHealth {
    pub runner_port_open: bool,
    pub runner_responding: bool,
    pub vite_port_open: bool,
}

/// Cached per-runner health snapshot, built by the background refresher.
/// Readable via `try_read()` in sync contexts (e.g., SSE streams).
#[derive(Clone, Debug, serde::Serialize)]
pub struct CachedRunnerHealth {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub is_primary: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub api_responding: bool,
}

pub fn spawn_health_cache_refresher(state: Arc<SupervisorState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(2));
        let mut tick_count: u64 = 0;
        loop {
            // Wait for either the periodic tick or an immediate refresh notification
            tokio::select! {
                _ = ticker.tick() => {},
                _ = state.health_cache_notify.notified() => {
                    // Small delay to let state settle after a start/stop
                    tokio::time::sleep(Duration::from_millis(100)).await;
                },
            }

            // Refresh health for all managed runners
            let runners = state.get_all_runners().await;
            let dev_mode = state.config.dev_mode;

            // Primary runner's health also goes into the legacy cached_health
            let mut primary_health = CachedPortHealth::default();
            let mut runner_snapshots = Vec::with_capacity(runners.len());

            for managed in &runners {
                let runner_port = managed.config.port;
                let is_primary = managed.config.is_primary;

                let runner_port_open = port::is_port_in_use(runner_port);
                let runner_responding = port::is_runner_responding(runner_port).await;

                // For primary in dev mode, also check Vite port
                let vite_port_open = if is_primary && dev_mode {
                    port::is_port_in_use(RUNNER_VITE_PORT)
                } else {
                    false
                };

                let new_health = CachedPortHealth {
                    runner_port_open,
                    runner_responding,
                    vite_port_open,
                };

                // For user-managed runners (not temp, not named), the supervisor only
                // observes. The `running` flag is initialized once at startup from
                // port-in-use and would otherwise go stale. Sync it to the observed
                // API responsiveness so the header status reflects reality.
                let runner_id = &managed.config.id;
                let is_supervisor_managed = is_temp_runner(runner_id) || is_named_runner(runner_id);
                if !is_supervisor_managed {
                    let mut runner_state = managed.runner.write().await;
                    if runner_state.running != runner_responding {
                        runner_state.running = runner_responding;
                        if !runner_responding {
                            runner_state.pid = None;
                        }
                    }
                }

                // Build runner snapshot for SSE consumers
                let runner_state = managed.runner.read().await;
                runner_snapshots.push(CachedRunnerHealth {
                    id: managed.config.id.clone(),
                    name: managed.config.name.clone(),
                    port: runner_port,
                    is_primary,
                    running: runner_state.running,
                    pid: runner_state.pid,
                    api_responding: runner_responding,
                });
                drop(runner_state);

                // Update per-runner cache
                let mut cache = managed.cached_health.write().await;
                *cache = new_health.clone();
                drop(cache);

                if is_primary {
                    primary_health = new_health;
                }
            }

            // If no runners exist, check legacy ports
            if runners.is_empty() {
                let runner_port = crate::config::RUNNER_API_PORT;
                let vite_port = RUNNER_VITE_PORT;

                primary_health = CachedPortHealth {
                    runner_port_open: port::is_port_in_use(runner_port),
                    runner_responding: port::is_runner_responding(runner_port).await,
                    vite_port_open: port::is_port_in_use(vite_port),
                };
            }

            // Update legacy cached_health (from primary)
            let mut cache = state.cached_health.write().await;
            *cache = primary_health.clone();
            drop(cache);

            // Update cached runner health snapshot (for SSE consumers)
            let mut runner_cache = state.cached_runner_health.write().await;
            *runner_cache = runner_snapshots;
            drop(runner_cache);

            tick_count += 1;
            // Log to dashboard once per minute (every 30 ticks at 2s interval)
            if tick_count % 30 == 1 {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Debug,
                        format!(
                            "Health cache: runner_port={}, api_responding={}, vite={} (runners: {})",
                            primary_health.runner_port_open,
                            primary_health.runner_responding,
                            primary_health.vite_port_open,
                            runners.len()
                        ),
                    )
                    .await;
            }
            debug!("Health cache refreshed");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cached_port_health_default_all_false() {
        let health = CachedPortHealth::default();
        assert!(!health.runner_port_open);
        assert!(!health.runner_responding);
        assert!(!health.vite_port_open);
    }

    #[test]
    fn test_cached_port_health_clone() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
            vite_port_open: false,
        };
        let cloned = health.clone();
        assert_eq!(cloned.runner_port_open, true);
        assert_eq!(cloned.runner_responding, true);
        assert_eq!(cloned.vite_port_open, false);
    }

    #[test]
    fn test_cached_port_health_debug_format() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: false,
            vite_port_open: true,
        };
        let debug_str = format!("{:?}", health);
        assert!(debug_str.contains("runner_port_open: true"));
        assert!(debug_str.contains("runner_responding: false"));
        assert!(debug_str.contains("vite_port_open: true"));
    }

    #[test]
    fn test_cached_port_health_all_true() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
            vite_port_open: true,
        };
        assert!(health.runner_port_open);
        assert!(health.runner_responding);
        assert!(health.vite_port_open);
    }
}

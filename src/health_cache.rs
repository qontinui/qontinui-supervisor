use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::config::RUNNER_VITE_PORT;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::port;
use crate::state::SupervisorState;

#[derive(Clone, Debug, Default)]
pub struct CachedPortHealth {
    pub runner_port_open: bool,
    pub runner_responding: bool,
    pub vite_port_open: bool,
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

            let runner_port = crate::config::RUNNER_API_PORT;
            let vite_port = RUNNER_VITE_PORT;

            let runner_port_open = port::is_port_in_use(runner_port);
            let runner_responding = port::is_runner_responding(runner_port).await;
            let vite_port_open = port::is_port_in_use(vite_port);

            let new_health = CachedPortHealth {
                runner_port_open,
                runner_responding,
                vite_port_open,
            };

            let mut cache = state.cached_health.write().await;
            *cache = new_health;
            drop(cache);

            tick_count += 1;
            // Log to dashboard once per minute (every 30 ticks at 2s interval)
            if tick_count % 30 == 1 {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Debug,
                        format!(
                            "Health cache: runner_port={}, api_responding={}, vite={}",
                            runner_port_open, runner_responding, vite_port_open
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

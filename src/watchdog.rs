use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::WATCHDOG_CHECK_INTERVAL_SECS;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::{ManagedRunner, SharedState};

/// Spawn the watchdog background task.
/// The watchdog monitors runner health and reports status, but never restarts
/// any runner. The supervisor is observe-only for all non-temp runners, and
/// temp runners are ephemeral (no need to auto-recover).
pub fn spawn_watchdog(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Watchdog task started (health monitoring only, no auto-restart)");
        let interval = Duration::from_secs(WATCHDOG_CHECK_INTERVAL_SECS);

        loop {
            sleep(interval).await;

            // Iterate all managed runners and update health state
            let runners = state.get_all_runners().await;
            for managed in runners {
                check_runner_health(&state, &managed).await;
            }
        }
    })
}

/// Check a single runner's health and log changes. No restart actions taken.
async fn check_runner_health(state: &SharedState, managed: &Arc<ManagedRunner>) {
    let runner_name = &managed.config.name;

    let (runner_running, stop_requested, restart_requested) = {
        let runner = managed.runner.read().await;
        (
            runner.running,
            runner.stop_requested,
            runner.restart_requested,
        )
    };

    // Don't report during manual operations
    if stop_requested || restart_requested {
        return;
    }

    // Check runner health from per-runner cache
    let is_healthy = {
        let cached = managed.cached_health.read().await;
        cached.runner_responding
    };

    if is_healthy {
        // Reset unhealthy tracking if we had any
        let needs_reset = {
            let wd = managed.watchdog.read().await;
            wd.restart_attempts > 0
        };
        if needs_reset {
            info!(
                "Watchdog: runner '{}' recovered",
                runner_name
            );
            state
                .logs
                .emit(
                    LogSource::Watchdog,
                    LogLevel::Info,
                    format!("Runner '{}' recovered", runner_name),
                )
                .await;
            let mut wd = managed.watchdog.write().await;
            wd.restart_attempts = 0;
        }
        return;
    }

    // Runner is not healthy — log but do NOT restart
    if runner_running {
        warn!("Watchdog: runner '{}' process not responding", runner_name);
        state
            .logs
            .emit(
                LogSource::Watchdog,
                LogLevel::Warn,
                format!(
                    "Runner '{}' not responding (supervisor will not auto-restart)",
                    runner_name
                ),
            )
            .await;
    } else {
        info!("Watchdog: runner '{}' process has exited", runner_name);
        state
            .logs
            .emit(
                LogSource::Watchdog,
                LogLevel::Info,
                format!(
                    "Runner '{}' has exited (supervisor will not auto-restart)",
                    runner_name
                ),
            )
            .await;
    }

    // Track unhealthy count for dashboard display
    {
        let mut wd = managed.watchdog.write().await;
        wd.record_crash();
    }

    state.notify_health_change();
}

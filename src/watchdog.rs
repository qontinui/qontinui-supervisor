use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::*;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::start_runner;
use crate::process::port::{is_port_in_use, is_runner_responding};
use crate::state::SharedState;

/// Spawn the watchdog background task.
pub fn spawn_watchdog(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Watchdog task started");
        let interval = Duration::from_secs(WATCHDOG_CHECK_INTERVAL_SECS);

        loop {
            sleep(interval).await;

            let enabled = {
                let wd = state.watchdog.read().await;
                wd.enabled
            };

            if !enabled {
                continue;
            }

            let runner_running = {
                let runner = state.runner.read().await;
                runner.running
            };

            let stop_requested = {
                let runner = state.runner.read().await;
                runner.stop_requested
            };

            let restart_requested = {
                let runner = state.runner.read().await;
                runner.restart_requested
            };

            // Don't interfere with manual operations
            if stop_requested || restart_requested {
                continue;
            }

            // Check if build is in progress
            {
                let build = state.build.read().await;
                if build.build_in_progress {
                    continue;
                }
            }

            // Check runner health
            let is_healthy = check_runner_health(&state).await;

            if is_healthy {
                // Runner is healthy — reset attempts if we had any
                let mut wd = state.watchdog.write().await;
                if wd.restart_attempts > 0 {
                    info!("Watchdog: runner recovered, resetting attempt counter");
                    state.logs.emit(
                        LogSource::Watchdog,
                        LogLevel::Info,
                        "Runner recovered, resetting restart attempts",
                    ).await;
                    wd.restart_attempts = 0;
                }
                continue;
            }

            // Runner is not healthy
            if runner_running {
                // Process is marked running but not responding — crashed
                warn!("Watchdog: runner process not responding");
                state.logs.emit(
                    LogSource::Watchdog,
                    LogLevel::Warn,
                    "Runner process not responding, attempting recovery",
                ).await;
            } else {
                // Process has already exited
                info!("Watchdog: runner process has exited");
                state.logs.emit(
                    LogSource::Watchdog,
                    LogLevel::Info,
                    "Runner process has exited, attempting restart",
                ).await;
            }

            // Record crash and check limits
            {
                let mut wd = state.watchdog.write().await;
                wd.record_crash();

                // Check crash loop
                if wd.is_crash_loop(WATCHDOG_CRASH_LOOP_THRESHOLD, WATCHDOG_CRASH_LOOP_WINDOW_SECS) {
                    let msg = format!(
                        "Crash loop detected ({} crashes in {}s window). Disabling watchdog.",
                        WATCHDOG_CRASH_LOOP_THRESHOLD, WATCHDOG_CRASH_LOOP_WINDOW_SECS
                    );
                    error!("{}", msg);
                    state.logs.emit(LogSource::Watchdog, LogLevel::Error, &msg).await;
                    wd.enabled = false;
                    wd.disabled_reason = Some("Crash loop detected".to_string());
                    continue;
                }

                // Check cooldown
                if wd.is_in_cooldown(WATCHDOG_COOLDOWN_SECS) {
                    info!("Watchdog: still in cooldown period, skipping restart");
                    continue;
                }

                // Check max attempts
                if wd.restart_attempts >= WATCHDOG_MAX_RESTART_ATTEMPTS {
                    let msg = format!(
                        "Max restart attempts ({}) reached. Disabling watchdog.",
                        WATCHDOG_MAX_RESTART_ATTEMPTS
                    );
                    error!("{}", msg);
                    state.logs.emit(LogSource::Watchdog, LogLevel::Error, &msg).await;
                    wd.enabled = false;
                    wd.disabled_reason = Some("Max restart attempts reached".to_string());
                    continue;
                }

                wd.restart_attempts += 1;
                wd.last_restart_at = Some(chrono::Utc::now());
            }

            // Attempt restart
            let attempt = state.watchdog.read().await.restart_attempts;
            let msg = format!("Watchdog: restart attempt {}/{}", attempt, WATCHDOG_MAX_RESTART_ATTEMPTS);
            info!("{}", msg);
            state.logs.emit(LogSource::Watchdog, LogLevel::Info, &msg).await;

            // Stop first if runner state thinks it's running
            if runner_running {
                if let Err(e) = crate::process::manager::stop_runner(&state).await {
                    warn!("Watchdog: error stopping runner: {}", e);
                }
            }

            // Start
            match start_runner(&state).await {
                Ok(()) => {
                    info!("Watchdog: runner restarted successfully");
                    state.logs.emit(
                        LogSource::Watchdog,
                        LogLevel::Info,
                        "Runner restarted successfully",
                    ).await;
                }
                Err(e) => {
                    error!("Watchdog: failed to restart runner: {}", e);
                    state.logs.emit(
                        LogSource::Watchdog,
                        LogLevel::Error,
                        format!("Failed to restart runner: {}", e),
                    ).await;
                }
            }
        }
    })
}

/// Check if the runner is healthy based on port availability.
async fn check_runner_health(state: &SharedState) -> bool {
    if state.config.dev_mode {
        // Dev mode: both Vite and API ports should be responsive
        let vite_up = is_port_in_use(RUNNER_VITE_PORT);
        let api_up = is_runner_responding(RUNNER_API_PORT).await;
        vite_up && api_up
    } else {
        // Exe mode: only API port
        is_runner_responding(RUNNER_API_PORT).await
    }
}

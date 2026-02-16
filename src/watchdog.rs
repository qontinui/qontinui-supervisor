use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::*;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::start_runner;
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

            let (runner_running, stop_requested, restart_requested) = {
                let runner = state.runner.read().await;
                (
                    runner.running,
                    runner.stop_requested,
                    runner.restart_requested,
                )
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

            // Check runner health (reads from cache — instant, no port check)
            let is_healthy = check_runner_health(&state).await;

            if is_healthy {
                // Runner is healthy — reset attempts if we had any
                let needs_reset = {
                    let wd = state.watchdog.read().await;
                    wd.restart_attempts > 0
                };
                if needs_reset {
                    info!("Watchdog: runner recovered, resetting attempt counter");
                    state
                        .logs
                        .emit(
                            LogSource::Watchdog,
                            LogLevel::Info,
                            "Runner recovered, resetting restart attempts",
                        )
                        .await;
                    let mut wd = state.watchdog.write().await;
                    wd.restart_attempts = 0;
                }
                continue;
            }

            // Runner is not healthy
            if runner_running {
                // Process is marked running but not responding — crashed
                warn!("Watchdog: runner process not responding");
                state
                    .logs
                    .emit(
                        LogSource::Watchdog,
                        LogLevel::Warn,
                        "Runner process not responding, attempting recovery",
                    )
                    .await;
            } else {
                // Process has already exited
                info!("Watchdog: runner process has exited");
                state
                    .logs
                    .emit(
                        LogSource::Watchdog,
                        LogLevel::Info,
                        "Runner process has exited, attempting restart",
                    )
                    .await;
            }

            // Record crash and check limits — collect decisions under lock, act outside
            let action = {
                let mut wd = state.watchdog.write().await;
                wd.record_crash();

                if wd.is_crash_loop(
                    WATCHDOG_CRASH_LOOP_THRESHOLD,
                    WATCHDOG_CRASH_LOOP_WINDOW_SECS,
                ) {
                    wd.enabled = false;
                    wd.disabled_reason = Some("Crash loop detected".to_string());
                    WatchdogAction::CrashLoop
                } else if wd.is_in_cooldown(WATCHDOG_COOLDOWN_SECS) {
                    WatchdogAction::InCooldown
                } else if wd.restart_attempts >= WATCHDOG_MAX_RESTART_ATTEMPTS {
                    wd.enabled = false;
                    wd.disabled_reason = Some("Max restart attempts reached".to_string());
                    WatchdogAction::MaxAttempts
                } else {
                    wd.restart_attempts += 1;
                    wd.last_restart_at = Some(chrono::Utc::now());
                    WatchdogAction::Restart(wd.restart_attempts)
                }
            };
            // Lock is dropped here

            match action {
                WatchdogAction::CrashLoop => {
                    let msg = format!(
                        "Crash loop detected ({} crashes in {}s window). Disabling watchdog.",
                        WATCHDOG_CRASH_LOOP_THRESHOLD, WATCHDOG_CRASH_LOOP_WINDOW_SECS
                    );
                    error!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Watchdog, LogLevel::Error, &msg)
                        .await;
                    state.notify_health_change();
                    crate::ai_debug::schedule_debug(
                        &state,
                        "Runner crash loop detected — watchdog disabled",
                    )
                    .await;
                    continue;
                }
                WatchdogAction::InCooldown => {
                    info!("Watchdog: still in cooldown period, skipping restart");
                    continue;
                }
                WatchdogAction::MaxAttempts => {
                    let msg = format!(
                        "Max restart attempts ({}) reached. Disabling watchdog.",
                        WATCHDOG_MAX_RESTART_ATTEMPTS
                    );
                    error!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Watchdog, LogLevel::Error, &msg)
                        .await;
                    state.notify_health_change();
                    crate::ai_debug::schedule_debug(
                        &state,
                        "Runner max restart attempts reached — watchdog disabled",
                    )
                    .await;
                    continue;
                }
                WatchdogAction::Restart(attempt) => {
                    let msg = format!(
                        "Watchdog: restart attempt {}/{}",
                        attempt, WATCHDOG_MAX_RESTART_ATTEMPTS
                    );
                    info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Watchdog, LogLevel::Info, &msg)
                        .await;
                }
            }

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
                    state
                        .logs
                        .emit(
                            LogSource::Watchdog,
                            LogLevel::Info,
                            "Runner restarted successfully",
                        )
                        .await;
                    state.notify_health_change();
                }
                Err(e) => {
                    error!("Watchdog: failed to restart runner: {}", e);
                    state
                        .logs
                        .emit(
                            LogSource::Watchdog,
                            LogLevel::Error,
                            format!("Failed to restart runner: {}", e),
                        )
                        .await;
                    state.notify_health_change();
                }
            }
        }
    })
}

/// Internal enum for watchdog decision flow (avoids holding lock across awaits).
enum WatchdogAction {
    CrashLoop,
    InCooldown,
    MaxAttempts,
    Restart(u32),
}

/// Check if the runner is healthy using the cached health data.
async fn check_runner_health(state: &SharedState) -> bool {
    let cached = state.cached_health.read().await;
    if state.config.dev_mode {
        cached.vite_port_open && cached.runner_responding
    } else {
        cached.runner_responding
    }
}

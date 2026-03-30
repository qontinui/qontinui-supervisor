use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::*;
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::{start_runner_by_id, stop_runner_by_id};
use crate::state::{ManagedRunner, SharedState};

/// Spawn the watchdog background task.
pub fn spawn_watchdog(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Watchdog task started");
        let interval = Duration::from_secs(WATCHDOG_CHECK_INTERVAL_SECS);

        loop {
            sleep(interval).await;

            // Don't interfere with smart rebuild (runner is intentionally down)
            {
                let sr = state.smart_rebuild.read().await;
                if !matches!(sr.phase, crate::smart_rebuild::SmartRebuildPhase::Idle) {
                    continue;
                }
            }

            // Check if build is in progress
            {
                let build = state.build.read().await;
                if build.build_in_progress {
                    continue;
                }
            }

            // Iterate all managed runners
            let runners = state.get_all_runners().await;
            for managed in runners {
                check_runner_watchdog(&state, &managed).await;
            }
        }
    })
}

/// Check a single runner's health and take watchdog action if needed.
async fn check_runner_watchdog(state: &SharedState, managed: &Arc<ManagedRunner>) {
    let runner_id = &managed.config.id;
    let runner_name = &managed.config.name;

    let enabled = {
        let wd = managed.watchdog.read().await;
        wd.enabled
    };

    if !enabled {
        return;
    }

    let (runner_running, stop_requested, restart_requested) = {
        let runner = managed.runner.read().await;
        (
            runner.running,
            runner.stop_requested,
            runner.restart_requested,
        )
    };

    // Don't interfere with manual operations
    if stop_requested || restart_requested {
        return;
    }

    // Check runner health from per-runner cache
    let is_healthy = {
        let cached = managed.cached_health.read().await;
        if managed.config.is_primary && state.config.dev_mode {
            cached.vite_port_open && cached.runner_responding
        } else {
            cached.runner_responding
        }
    };

    if is_healthy {
        // Runner is healthy — reset attempts if we had any
        let needs_reset = {
            let wd = managed.watchdog.read().await;
            wd.restart_attempts > 0
        };
        if needs_reset {
            info!(
                "Watchdog: runner '{}' recovered, resetting attempt counter",
                runner_name
            );
            state
                .logs
                .emit(
                    LogSource::Watchdog,
                    LogLevel::Info,
                    format!(
                        "Runner '{}' recovered, resetting restart attempts",
                        runner_name
                    ),
                )
                .await;
            let mut wd = managed.watchdog.write().await;
            wd.restart_attempts = 0;
        }
        return;
    }

    // Protected runners must not be restarted by the watchdog — only manual
    // action can restart them.  Log the issue but take no recovery action.
    if managed.is_protected().await {
        warn!(
            "Watchdog: protected runner '{}' is unhealthy but will NOT be restarted (protected)",
            runner_name
        );
        state
            .logs
            .emit(
                LogSource::Watchdog,
                LogLevel::Warn,
                format!(
                    "Protected runner '{}' is unhealthy — skipping automatic restart",
                    runner_name
                ),
            )
            .await;
        return;
    }

    // Runner is not healthy
    if runner_running {
        warn!("Watchdog: runner '{}' process not responding", runner_name);
        state
            .logs
            .emit(
                LogSource::Watchdog,
                LogLevel::Warn,
                format!(
                    "Runner '{}' process not responding, attempting recovery",
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
                    "Runner '{}' process has exited, attempting restart",
                    runner_name
                ),
            )
            .await;
    }

    // Record crash and check limits
    let action = {
        let mut wd = managed.watchdog.write().await;
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

    match action {
        WatchdogAction::CrashLoop => {
            let msg = format!(
                "Runner '{}': Crash loop detected ({} crashes in {}s window). Disabling watchdog.",
                runner_name, WATCHDOG_CRASH_LOOP_THRESHOLD, WATCHDOG_CRASH_LOOP_WINDOW_SECS
            );
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Watchdog, LogLevel::Error, &msg)
                .await;
            state.notify_health_change();
            crate::ai_debug::schedule_debug(
                state,
                &format!(
                    "Runner '{}' crash loop detected — watchdog disabled",
                    runner_name
                ),
            )
            .await;
            return;
        }
        WatchdogAction::InCooldown => {
            info!(
                "Watchdog: runner '{}' still in cooldown period, skipping restart",
                runner_name
            );
            return;
        }
        WatchdogAction::MaxAttempts => {
            let msg = format!(
                "Runner '{}': Max restart attempts ({}) reached. Disabling watchdog.",
                runner_name, WATCHDOG_MAX_RESTART_ATTEMPTS
            );
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Watchdog, LogLevel::Error, &msg)
                .await;
            state.notify_health_change();
            crate::ai_debug::schedule_debug(
                state,
                &format!(
                    "Runner '{}' max restart attempts reached — watchdog disabled",
                    runner_name
                ),
            )
            .await;
            return;
        }
        WatchdogAction::Restart(attempt) => {
            let msg = format!(
                "Watchdog: runner '{}' restart attempt {}/{}",
                runner_name, attempt, WATCHDOG_MAX_RESTART_ATTEMPTS
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Watchdog, LogLevel::Info, &msg)
                .await;
        }
    }

    let restart_start = std::time::Instant::now();

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::RestartStarted {
            source: RestartSource::Watchdog,
            rebuild: false,
        });

    // Stop first if runner state thinks it's running
    if runner_running {
        if let Err(e) = stop_runner_by_id(state, runner_id).await {
            warn!("Watchdog: error stopping runner '{}': {}", runner_name, e);
        }
    }

    // Start
    match start_runner_by_id(state, runner_id).await {
        Ok(()) => {
            info!("Watchdog: runner '{}' restarted successfully", runner_name);
            state
                .logs
                .emit(
                    LogSource::Watchdog,
                    LogLevel::Info,
                    format!("Runner '{}' restarted successfully", runner_name),
                )
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartCompleted {
                    source: RestartSource::Watchdog,
                    rebuild: false,
                    duration_secs: restart_start.elapsed().as_secs_f64(),
                    build_duration_secs: None,
                });
            state.notify_health_change();
        }
        Err(e) => {
            error!(
                "Watchdog: failed to restart runner '{}': {}",
                runner_name, e
            );
            state
                .logs
                .emit(
                    LogSource::Watchdog,
                    LogLevel::Error,
                    format!("Failed to restart runner '{}': {}", runner_name, e),
                )
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartFailed {
                    source: RestartSource::Watchdog,
                    error: e.to_string(),
                });
            state.notify_health_change();
        }
    }
}

/// Internal enum for watchdog decision flow (avoids holding lock across awaits).
enum WatchdogAction {
    CrashLoop,
    InCooldown,
    MaxAttempts,
    Restart(u32),
}

use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::*;
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

/// Spawn the overnight watchdog background task.
///
/// Runs every 3 minutes and, during overnight hours (11pm–6am local),
/// takes a UI Bridge snapshot of the runner's frontend to verify the
/// webview is actually rendering. Restarts the runner after 3 consecutive
/// failures (~9 minutes of broken UI), respecting code activity to avoid
/// interfering with active Claude Code sessions.
pub fn spawn_overnight_watchdog(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Overnight watchdog task started");
        let interval = Duration::from_secs(OVERNIGHT_CHECK_INTERVAL_SECS);

        loop {
            sleep(interval).await;

            // Only active when the regular watchdog is enabled
            let watchdog_enabled = {
                let wd = state.watchdog.read().await;
                wd.enabled
            };
            if !watchdog_enabled {
                continue;
            }

            let now_local = chrono::Local::now();
            let hour = now_local.hour();

            if !is_overnight_hour(hour) {
                // Daytime: reset state and mark inactive
                let mut ow = state.overnight_watchdog.write().await;
                if ow.active {
                    info!("Overnight watchdog: leaving overnight hours, deactivating");
                    ow.active = false;
                    ow.consecutive_failures = 0;
                }
                continue;
            }

            // We're in overnight hours
            {
                let mut ow = state.overnight_watchdog.write().await;
                if !ow.active {
                    info!("Overnight watchdog: entering overnight hours, activating");
                }
                ow.active = true;
                ow.last_check_at = Some(chrono::Utc::now());
            }

            // Skip if runner API is already down (the regular watchdog handles that)
            let runner_responding = {
                let cached = state.cached_health.read().await;
                cached.runner_responding
            };
            if !runner_responding {
                continue;
            }

            // Skip if build/stop/restart in progress
            {
                let build = state.build.read().await;
                if build.build_in_progress {
                    continue;
                }
            }
            {
                let runner = state.runner.read().await;
                if runner.stop_requested || runner.restart_requested {
                    continue;
                }
            }

            // Take UI Bridge snapshot
            let snapshot_result = take_runner_ui_snapshot(&state.http_client).await;

            match snapshot_result {
                Ok(element_count) if element_count >= 1 => {
                    // Success
                    let mut ow = state.overnight_watchdog.write().await;
                    ow.consecutive_failures = 0;
                    ow.last_successful_check_at = Some(chrono::Utc::now());
                    ow.last_failure_reason = None;
                    info!(
                        "Overnight watchdog: UI check passed ({} elements)",
                        element_count
                    );
                }
                Ok(_) => {
                    handle_failure(&state, "UI returned 0 elements".to_string()).await;
                }
                Err(e) => {
                    handle_failure(&state, e).await;
                }
            }
        }
    })
}

/// Handle a UI snapshot failure: increment counter, potentially restart.
async fn handle_failure(state: &SharedState, reason: String) {
    let (consecutive_failures, should_restart) = {
        let mut ow = state.overnight_watchdog.write().await;
        ow.consecutive_failures += 1;
        ow.last_failure_reason = Some(reason.clone());
        (
            ow.consecutive_failures,
            ow.consecutive_failures >= OVERNIGHT_MAX_CONSECUTIVE_FAILURES,
        )
    };

    if !should_restart {
        warn!(
            "Overnight watchdog: UI check failed ({}/{}): {}",
            consecutive_failures, OVERNIGHT_MAX_CONSECUTIVE_FAILURES, reason
        );
        state
            .logs
            .emit(
                LogSource::OvernightWatchdog,
                LogLevel::Warn,
                format!(
                    "UI check failed ({}/{}): {}",
                    consecutive_failures, OVERNIGHT_MAX_CONSECUTIVE_FAILURES, reason
                ),
            )
            .await;
        return;
    }

    // Check code activity before restarting
    let (code_being_edited, external_claude) = {
        let ca = state.code_activity.read().await;
        (ca.code_being_edited, ca.external_claude_session)
    };

    if code_being_edited || external_claude {
        warn!(
            "Overnight watchdog: UI broken for {} checks but deferring restart \
             (code_editing={}, external_claude={})",
            consecutive_failures, code_being_edited, external_claude
        );
        state
            .logs
            .emit(
                LogSource::OvernightWatchdog,
                LogLevel::Warn,
                format!(
                    "UI broken but deferring restart (code_editing={}, external_claude={})",
                    code_being_edited, external_claude
                ),
            )
            .await;
        return;
    }

    // Restart the runner (no rebuild)
    warn!(
        "Overnight watchdog: UI broken for {} consecutive checks, restarting runner",
        consecutive_failures
    );
    state
        .logs
        .emit(
            LogSource::OvernightWatchdog,
            LogLevel::Warn,
            format!(
                "UI broken for {} consecutive checks, restarting runner (no rebuild)",
                consecutive_failures
            ),
        )
        .await;

    let restart_start = std::time::Instant::now();

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::RestartStarted {
            source: RestartSource::Watchdog,
            rebuild: false,
        });

    match crate::process::manager::restart_runner(state, false, RestartSource::Watchdog, false)
        .await
    {
        Ok(()) => {
            info!("Overnight watchdog: runner restarted successfully");
            let mut ow = state.overnight_watchdog.write().await;
            ow.consecutive_failures = 0;
            ow.last_action_taken = Some(format!(
                "Restarted runner at {} (took {:.1}s)",
                chrono::Utc::now().to_rfc3339(),
                restart_start.elapsed().as_secs_f64()
            ));
            state
                .logs
                .emit(
                    LogSource::OvernightWatchdog,
                    LogLevel::Info,
                    "Runner restarted successfully after UI failure",
                )
                .await;
            state.notify_health_change();
        }
        Err(e) => {
            warn!(
                "Overnight watchdog: restart failed: {}, scheduling AI debug",
                e
            );
            let mut ow = state.overnight_watchdog.write().await;
            ow.last_action_taken = Some(format!("Restart failed: {}", e));
            drop(ow);
            state
                .logs
                .emit(
                    LogSource::OvernightWatchdog,
                    LogLevel::Error,
                    format!("Restart failed: {}, scheduling AI debug", e),
                )
                .await;
            crate::ai_debug::schedule_debug(
                state,
                "Overnight watchdog: runner restart failed after UI bridge health check failure",
            )
            .await;
        }
    }
}

/// Check if the given hour (0-23) falls within overnight hours.
pub fn is_overnight_hour(hour: u32) -> bool {
    !(OVERNIGHT_END_HOUR..OVERNIGHT_START_HOUR).contains(&hour)
}

/// Take a UI Bridge snapshot from the runner and return the element count.
async fn take_runner_ui_snapshot(client: &reqwest::Client) -> Result<usize, String> {
    let url = format!(
        "http://127.0.0.1:{}/ui-bridge/control/snapshot",
        RUNNER_API_PORT
    );

    let response = client
        .get(&url)
        .timeout(Duration::from_secs(OVERNIGHT_SNAPSHOT_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse JSON: {}", e))?;

    // The UI Bridge snapshot returns { data: { elements: [...] } }
    let elements = body
        .get("data")
        .and_then(|d| d.get("elements"))
        .and_then(|e| e.as_array())
        .ok_or_else(|| "Missing data.elements in response".to_string())?;

    Ok(elements.len())
}

use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overnight_hour_23_is_overnight() {
        assert!(is_overnight_hour(23));
    }

    #[test]
    fn test_overnight_hour_0_is_overnight() {
        assert!(is_overnight_hour(0));
    }

    #[test]
    fn test_overnight_hour_3_is_overnight() {
        assert!(is_overnight_hour(3));
    }

    #[test]
    fn test_overnight_hour_5_is_overnight() {
        assert!(is_overnight_hour(5));
    }

    #[test]
    fn test_overnight_hour_6_is_not_overnight() {
        assert!(!is_overnight_hour(6));
    }

    #[test]
    fn test_overnight_hour_12_is_not_overnight() {
        assert!(!is_overnight_hour(12));
    }

    #[test]
    fn test_overnight_hour_22_is_not_overnight() {
        assert!(!is_overnight_hour(22));
    }

    #[test]
    fn test_overnight_hour_7_is_not_overnight() {
        assert!(!is_overnight_hour(7));
    }

    #[test]
    fn test_snapshot_parsing_valid_response() {
        let json: serde_json::Value = serde_json::json!({
            "data": {
                "elements": [
                    {"id": "1", "tag": "div"},
                    {"id": "2", "tag": "button"}
                ]
            }
        });

        let elements = json
            .get("data")
            .and_then(|d| d.get("elements"))
            .and_then(|e| e.as_array())
            .unwrap();

        assert_eq!(elements.len(), 2);
    }

    #[test]
    fn test_snapshot_parsing_empty_elements() {
        let json: serde_json::Value = serde_json::json!({
            "data": {
                "elements": []
            }
        });

        let elements = json
            .get("data")
            .and_then(|d| d.get("elements"))
            .and_then(|e| e.as_array())
            .unwrap();

        assert_eq!(elements.len(), 0);
    }

    #[test]
    fn test_snapshot_parsing_missing_data() {
        let json: serde_json::Value = serde_json::json!({"status": "ok"});

        let result = json
            .get("data")
            .and_then(|d| d.get("elements"))
            .and_then(|e| e.as_array());

        assert!(result.is_none());
    }
}

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::is_temp_runner;
use crate::process::port;
use crate::state::SupervisorState;

/// Returns true if this runner is a named runner managed by the supervisor.
fn is_named_runner(runner_id: &str) -> bool {
    runner_id.starts_with("named-")
}

/// Derived health classification for a runner surfaced to dashboard consumers.
///
/// This is the supervisor-side view: it reflects both process liveness (is the
/// OS process + port alive?) and application-level signals from the runner's
/// own `/health` body (e.g. `ui_error`, `derived_status`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunnerStatus {
    Healthy,
    Degraded {
        reason: String,
    },
    Errored {
        reason: String,
    },
    #[default]
    Offline,
    Starting,
}

/// Snapshot of a UI-level runtime error reported by the runner's `/health`
/// endpoint. Mirrors the `ui_error` object the runner emits when its React
/// error boundary catches a crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiErrorSummary {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_stack: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub reported_at: DateTime<Utc>,
    pub count: u32,
}

/// Snapshot of the most recent Rust crash dump the runner found on startup.
/// Mirrors the runner's `RecentCrash` (camelCase on the wire) so
/// `fetch_runner_health_body` can deserialize the field directly. Non-unwinding
/// panics abort the process across the WebView2 FFI boundary and bypass the
/// React error boundary entirely, so this is the only way fleet consumers see
/// that a runner was just force-restarted after a Rust crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentCrashSummary {
    pub file_path: String,
    pub reported_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

/// Raw /health response body shape we care about. Uses `serde(default)` so
/// older runners (without `ui_error` / `derived_status` / `recent_crash`) still
/// parse cleanly.
#[derive(Debug, Deserialize)]
struct RunnerHealthBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    derived_status: Option<String>,
    #[serde(default)]
    ui_error: Option<UiErrorSummary>,
    #[serde(default)]
    recent_crash: Option<RecentCrashSummary>,
}

#[derive(Clone, Debug, Default)]
pub struct CachedPortHealth {
    pub runner_port_open: bool,
    pub runner_responding: bool,
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
    /// Most recent UI-level error reported by the runner's `/health` body.
    /// `None` when the runner reports no error or when the field is missing
    /// (older runners predating Phase 3J.1).
    pub ui_error: Option<UiErrorSummary>,
    /// Most recent Rust crash dump surfaced by the runner's `/health` body.
    /// `None` when no fresh dump is on disk or the runner predates the
    /// crash-dump scanner (post-3J follow-up).
    pub recent_crash: Option<RecentCrashSummary>,
    /// Supervisor-derived status. Combines runner process state with the
    /// runner's own `derived_status` + `ui_error` + `recent_crash` signals.
    pub derived_status: RunnerStatus,
}

/// Truncate a string to at most `max_chars` chars, adding an ellipsis marker
/// if truncation occurred. Uses char boundaries, not byte boundaries, so it is
/// safe on multi-byte UTF-8 input.
fn truncate_reason(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Fetch `{runner_url}/health` with a 3-second timeout and parse the
/// runner's self-reported UI error + derived status. Returns `None` if the
/// endpoint is unreachable, the body fails to parse, or the request times
/// out — callers treat that as "no signal" and fall back to port-level state.
async fn fetch_runner_health_body(port: u16) -> Option<RunnerHealthBody> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<RunnerHealthBody>().await.ok()
}

/// Derive the supervisor's view of a runner's status from the signals we have.
///
/// Rules (in order):
/// 1. /health unreachable → `Offline` (if supervisor thinks process is dead)
///    or `Starting` (process exists but /health isn't responding yet).
/// 2. /health reachable with a `ui_error` → `Errored { reason }` where reason
///    is the ui_error message truncated to 200 chars.
/// 3. /health reachable with a `recent_crash` → `Errored { reason }` using
///    the crash's `panicMessage` (or a generic fallback if the message was
///    not captured). Non-unwinding Rust panics abort the process before the
///    React boundary sees them, so this is the only signal for that class.
/// 4. /health reachable, runner body reports `derived_status: "errored"` → `Errored`.
/// 5. /health reachable, no error signals, running=true → `Healthy`.
/// 6. Everything else → `Offline`.
fn derive_runner_status(
    running: bool,
    api_responding: bool,
    body: Option<&RunnerHealthBody>,
) -> RunnerStatus {
    if !api_responding {
        // Process may be alive (e.g. Tauri window launched, HTTP server still
        // warming) but /health doesn't respond yet. Treat that as Starting so
        // operators see a blue "spinning up" indicator instead of red.
        if running {
            return RunnerStatus::Starting;
        }
        return RunnerStatus::Offline;
    }

    if let Some(body) = body {
        if let Some(ui_err) = body.ui_error.as_ref() {
            return RunnerStatus::Errored {
                reason: truncate_reason(&ui_err.message, 200),
            };
        }
        if let Some(crash) = body.recent_crash.as_ref() {
            let reason = crash
                .panic_message
                .as_deref()
                .map(|m| truncate_reason(m, 200))
                .unwrap_or_else(|| {
                    "runner restarted after Rust panic (no message captured)".to_string()
                });
            return RunnerStatus::Errored { reason };
        }
        // Honour runner's own self-classification if present. The runner's
        // derived_status is an application-layer signal; we already handled
        // ui_error + recent_crash above, so here we surface `errored` (as a
        // belt-and-braces signal) and `degraded` (subsystem outage — e.g.
        // embedding service unreachable — where the runner is still
        // functional but operating in reduced capacity).
        if let Some(ds) = body.derived_status.as_deref() {
            if ds.eq_ignore_ascii_case("errored") {
                return RunnerStatus::Errored {
                    reason: "runner reported derived_status=errored".to_string(),
                };
            }
            if ds.eq_ignore_ascii_case("degraded") {
                return RunnerStatus::Degraded {
                    reason: "runner reported derived_status=degraded".to_string(),
                };
            }
        }
        // Runner reports `status: "starting"` during boot (legacy field kept
        // for backward compat with older runners). Surface that so the
        // dashboard can show a blue badge instead of green-too-soon.
        if body.status.as_deref() == Some("starting") {
            return RunnerStatus::Starting;
        }
    }

    if running {
        RunnerStatus::Healthy
    } else {
        // /health is responding but supervisor state says not running. This is
        // the "external runner" case (user-started). Treat as healthy since
        // the runner is clearly up and serving requests.
        RunnerStatus::Healthy
    }
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

            // Primary runner's health also goes into the legacy cached_health
            let mut primary_health = CachedPortHealth::default();
            let mut runner_snapshots = Vec::with_capacity(runners.len());

            for managed in &runners {
                let runner_port = managed.config.port;
                let is_primary = managed.config.is_primary;

                let runner_port_open = port::is_port_in_use(runner_port);
                let runner_responding = port::is_runner_responding(runner_port).await;

                let new_health = CachedPortHealth {
                    runner_port_open,
                    runner_responding,
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

                // If the runner's TCP port is responsive, GET its /health to
                // extract the application-layer signals (ui_error,
                // derived_status, recent_crash). Older runners that don't
                // emit these fields still parse cleanly thanks to
                // `serde(default)` on RunnerHealthBody — the missing fields
                // stay `None` and `derived_status` is inferred from process
                // state.
                let health_body = if runner_responding {
                    fetch_runner_health_body(runner_port).await
                } else {
                    None
                };
                let ui_error = health_body.as_ref().and_then(|b| b.ui_error.clone());
                let recent_crash = health_body.as_ref().and_then(|b| b.recent_crash.clone());

                // Build runner snapshot for SSE consumers
                let runner_state = managed.runner.read().await;
                let derived_status = derive_runner_status(
                    runner_state.running,
                    runner_responding,
                    health_body.as_ref(),
                );
                runner_snapshots.push(CachedRunnerHealth {
                    id: managed.config.id.clone(),
                    name: managed.config.name.clone(),
                    port: runner_port,
                    is_primary,
                    running: runner_state.running,
                    pid: runner_state.pid,
                    api_responding: runner_responding,
                    ui_error,
                    recent_crash,
                    derived_status,
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

                primary_health = CachedPortHealth {
                    runner_port_open: port::is_port_in_use(runner_port),
                    runner_responding: port::is_runner_responding(runner_port).await,
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
                            "Health cache: runner_port={}, api_responding={} (runners: {})",
                            primary_health.runner_port_open,
                            primary_health.runner_responding,
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
    }

    #[test]
    fn test_cached_port_health_clone() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
        };
        let cloned = health.clone();
        assert!(cloned.runner_port_open);
        assert!(cloned.runner_responding);
    }

    #[test]
    fn test_cached_port_health_debug_format() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: false,
        };
        let debug_str = format!("{:?}", health);
        assert!(debug_str.contains("runner_port_open: true"));
        assert!(debug_str.contains("runner_responding: false"));
    }

    #[test]
    fn test_cached_port_health_all_true() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
        };
        assert!(health.runner_port_open);
        assert!(health.runner_responding);
    }

    #[test]
    fn test_derive_runner_status_offline_when_api_down_and_not_running() {
        let status = derive_runner_status(false, false, None);
        assert!(matches!(status, RunnerStatus::Offline));
    }

    #[test]
    fn test_derive_runner_status_starting_when_process_alive_but_api_down() {
        // Process exists but /health isn't responding yet — the spec says to
        // treat this as Starting, not Offline.
        let status = derive_runner_status(true, false, None);
        assert!(matches!(status, RunnerStatus::Starting));
    }

    #[test]
    fn test_derive_runner_status_healthy_when_api_responding_no_body() {
        // /health reachable but body wasn't parseable (older runner or
        // network hiccup). Fall through to Healthy when supervisor tracks
        // the process as running.
        let status = derive_runner_status(true, true, None);
        assert!(matches!(status, RunnerStatus::Healthy));
    }

    #[test]
    fn test_derive_runner_status_errored_when_ui_error_present() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: Some(UiErrorSummary {
                message: "ReferenceError: foo is not defined".to_string(),
                digest: None,
                stack: None,
                component_stack: None,
                first_seen: Utc::now(),
                reported_at: Utc::now(),
                count: 1,
            }),
            recent_crash: None,
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("ReferenceError"));
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_starting_when_body_reports_starting() {
        // Older runners use top-level status="starting" during boot. Respect
        // that so the dashboard shows a blue badge, not green.
        let body = RunnerHealthBody {
            status: Some("starting".to_string()),
            derived_status: None,
            ui_error: None,
            recent_crash: None,
        };
        let status = derive_runner_status(true, true, Some(&body));
        assert!(matches!(status, RunnerStatus::Starting));
    }

    #[test]
    fn test_derive_runner_status_errored_from_derived_status_field() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("ERRORED".to_string()), // case-insensitive
            ui_error: None,
            recent_crash: None,
        };
        let status = derive_runner_status(true, true, Some(&body));
        assert!(matches!(status, RunnerStatus::Errored { .. }));
    }

    #[test]
    fn test_derive_runner_status_degraded_from_derived_status_field() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("Degraded".to_string()), // case-insensitive
            ui_error: None,
            recent_crash: None,
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Degraded { reason } => {
                assert!(reason.contains("degraded"));
            }
            other => panic!("expected Degraded, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_errored_when_recent_crash_present() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: None,
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_1.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: Some("src-tauri/src/foo.rs:42:9".to_string()),
                panic_message: Some("no reactor running".to_string()),
                thread: Some("main".to_string()),
            }),
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("no reactor running"));
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_errored_when_crash_has_no_message() {
        // Truncated dumps can miss the `=== PANIC MESSAGE ===` section. The
        // badge should still flip to errored with a placeholder reason so
        // operators see *something*.
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: None,
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_partial.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: None,
                panic_message: None,
                thread: None,
            }),
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("Rust panic"), "got: {reason}");
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_ui_error_wins_over_recent_crash() {
        // A live UI error is more actionable than a historical crash dump:
        // if both are present, surface the UI error reason in the badge.
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: Some(UiErrorSummary {
                message: "live ui error".to_string(),
                digest: None,
                stack: None,
                component_stack: None,
                first_seen: Utc::now(),
                reported_at: Utc::now(),
                count: 1,
            }),
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_1.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: None,
                panic_message: Some("stale crash".to_string()),
                thread: None,
            }),
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => assert!(reason.contains("live ui error")),
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_runner_health_body_parses_recent_crash_camel_case() {
        // The runner serializes RecentCrash with serde(rename_all="camelCase").
        // Deserializer must match that shape.
        let json = r#"{
            "status": "ok",
            "derived_status": "errored",
            "recent_crash": {
                "filePath": "D:/.dev-logs/crash_1.txt",
                "reportedAt": "2026-04-22T10:15:30Z",
                "panicLocation": "src-tauri/src/foo.rs:42:9",
                "panicMessage": "boom",
                "thread": "main"
            }
        }"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        let crash = body.recent_crash.expect("recent_crash present");
        assert_eq!(crash.panic_message.as_deref(), Some("boom"));
        assert_eq!(crash.thread.as_deref(), Some("main"));
    }

    #[test]
    fn test_truncate_reason_preserves_short_input() {
        assert_eq!(truncate_reason("hello", 200), "hello");
    }

    #[test]
    fn test_truncate_reason_clips_long_input() {
        let long = "x".repeat(500);
        let out = truncate_reason(&long, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn test_runner_health_body_parses_older_runner_without_new_fields() {
        // Older runners only emit `status`. The new fields must parse as
        // `None`, not error the whole response out.
        let json = r#"{"status":"ok"}"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        assert_eq!(body.status.as_deref(), Some("ok"));
        assert!(body.derived_status.is_none());
        assert!(body.ui_error.is_none());
        assert!(body.recent_crash.is_none());
    }

    #[test]
    fn test_runner_health_body_parses_full_payload() {
        let json = r#"{
            "status": "ok",
            "derived_status": "errored",
            "ui_error": {
                "message": "boom",
                "stack": null,
                "component_stack": null,
                "digest": null,
                "first_seen": "2026-04-21T00:00:00Z",
                "reported_at": "2026-04-21T00:00:05Z",
                "count": 3
            }
        }"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        assert_eq!(body.derived_status.as_deref(), Some("errored"));
        let ui_err = body.ui_error.expect("ui_error should be present");
        assert_eq!(ui_err.message, "boom");
        assert_eq!(ui_err.count, 3);
    }

    #[test]
    fn test_runner_status_default_is_offline() {
        let s: RunnerStatus = Default::default();
        assert!(matches!(s, RunnerStatus::Offline));
    }
}

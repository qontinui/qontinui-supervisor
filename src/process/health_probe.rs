//! Post-start health probe for runners.
//!
//! Used by the legacy `POST /runner/restart` and the per-runner
//! `POST /runners/{id}/start` endpoints to wait until a freshly-(re)started
//! runner is actually serving HTTP `/health` before returning a 200 to the
//! caller. Without this wait, those endpoints would return successfully the
//! moment the OS process spawned, even when the runner immediately hung
//! without binding its API port — leaving callers unable to distinguish
//! "started successfully" from "started but hung" without a separate probe.
//!
//! The probe encapsulates a simple 500ms poll loop against
//! [`crate::process::port::is_runner_responding`]. On timeout it captures
//! the last few log lines from the runner's per-runner log buffer using the
//! same mechanism `routes/runners.rs::recent_runner_log_lines` uses for the
//! spawn-test failure response, so the route handler can build a cleanly
//! diagnosable 503.
//!
//! Spawn-test has its own, richer post-spawn probe in `routes/runners.rs`
//! (`probe_runner_health`) that also captures `recent_panic` and child-alive
//! state. That probe is intentionally untouched here — restart/start callers
//! get the simpler "did the port come up?" semantics, which is what the
//! original failure mode warranted.

use std::time::{Duration, Instant};

use crate::log_capture::LogLevel;
use crate::process::port::is_runner_responding;
use crate::state::SharedState;

/// How often to poll the runner's `/health` endpoint while waiting.
///
/// Matches the cadence the spawn-test wait-loop uses for its `wait` parameter
/// (the `wait_timeout_secs` flow polls every 500ms-ish via a 2s coarse loop;
/// here we go finer-grained because the typical observed bind time on a
/// healthy runner is well under one poll interval).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Default total wait budget. Matches the runner's first-healthy watchdog
/// warm-up window for low-latency starts and is plenty of headroom for a
/// healthy runner to bind its port.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many recent log lines to include in the failure body. Matches
/// `routes/runners.rs::probe_runner_health`'s 20-line snapshot.
const RECENT_LOG_LIMIT: usize = 20;

/// Captured failure context for a runner that didn't come back up within
/// the wait budget. Carries enough information for the route handler to
/// build a 503 body with `elapsed_ms` and `recent_logs` without re-querying.
#[derive(Debug)]
pub struct HealthProbeFailure {
    /// How long we waited before giving up, in milliseconds. On a real
    /// timeout this is >= the configured budget; on an early-out (e.g.
    /// runner removed from registry mid-probe) it can be smaller.
    pub elapsed_ms: u64,
    /// Last [`RECENT_LOG_LIMIT`] log lines from the runner's per-runner log
    /// buffer, formatted as `"<level> <message>"` to keep the JSON payload
    /// compact. Empty if the runner was removed from the registry between
    /// the probe starting and timing out.
    pub recent_logs: Vec<String>,
}

/// Poll `runner_id`'s `/health` endpoint at [`POLL_INTERVAL`] cadence until
/// it succeeds or `timeout` elapses.
///
/// Returns:
/// - `Ok(())` on the first successful health response.
/// - `Err(HealthProbeFailure { elapsed_ms, recent_logs })` if the runner
///   never responded within the budget.
///
/// **Resolution semantics.** The runner's port is read from the live
/// `state.runners` registry on every poll. If the runner is removed from the
/// registry mid-probe (e.g. another caller stopped it), we abort with a
/// failure carrying whatever logs we can still snapshot. We deliberately do
/// not consult `state.stopped_runners` here — a runner missing from the live
/// registry means the start failed in a way the probe can't usefully wait
/// out.
pub async fn wait_for_runner_healthy(
    state: &SharedState,
    runner_id: &str,
    timeout: Duration,
) -> Result<(), HealthProbeFailure> {
    let start = Instant::now();

    loop {
        // Re-read the port each iteration so a config change (rare) is
        // honored, and so a registry removal cleanly aborts the loop.
        let port = match state.get_runner(runner_id).await {
            Some(managed) => managed.config.port,
            None => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                return Err(HealthProbeFailure {
                    elapsed_ms,
                    recent_logs: Vec::new(),
                });
            }
        };

        if is_runner_responding(port).await {
            return Ok(());
        }

        if start.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let recent_logs = recent_runner_log_lines(state, runner_id, RECENT_LOG_LIMIT).await;
    Err(HealthProbeFailure {
        elapsed_ms,
        recent_logs,
    })
}

/// Default-timeout convenience wrapper. Use [`DEFAULT_TIMEOUT`] (30 s),
/// matching the contract documented on the `/runner/restart` and
/// `/runners/{id}/start` route handlers.
pub async fn wait_for_runner_healthy_default(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), HealthProbeFailure> {
    wait_for_runner_healthy(state, runner_id, DEFAULT_TIMEOUT).await
}

/// Snapshot the last `limit` log lines from a runner's per-runner log buffer.
///
/// Mirrors `routes/runners.rs::recent_runner_log_lines` (the spawn-test
/// failure-body helper) — we duplicate the small formatting routine here
/// rather than make that helper public, because the two callers live in
/// different modules and the function body is six lines. If a third caller
/// appears, fold this into a shared helper.
async fn recent_runner_log_lines(
    state: &SharedState,
    runner_id: &str,
    limit: usize,
) -> Vec<String> {
    let Some(managed) = state.get_runner(runner_id).await else {
        return Vec::new();
    };
    let entries = managed.logs.history().await;
    let total = entries.len();
    let start = total.saturating_sub(limit);
    entries[start..]
        .iter()
        .map(|e| {
            let level = match e.level {
                LogLevel::Info => "info",
                LogLevel::Warn => "warn",
                LogLevel::Error => "error",
                LogLevel::Debug => "debug",
            };
            format!("{} {}", level, e.message)
        })
        .collect()
}

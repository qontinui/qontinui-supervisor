//! WSL-based CI runner health monitoring (Phase 3b of self-hosted CI runners).
//!
//! GitHub Actions self-hosted runners run as WSL systemd services (e.g.
//! `actions.runner.qontinui-qontinui-coord.spaceship-wsl`), NOT as
//! supervisor-managed child processes. The supervisor cannot use its existing
//! `child.wait()` pattern — it must probe via `wsl` commands.
//!
//! This module provides:
//! - `probe_ci_runners()`: synchronous probe via `wsl -e bash -c ...` commands
//! - `ci_runner_probe_loop()`: async 30s loop that stores state + auto-restarts
//! - `try_restart_ci_runner()`: rate-limited `systemctl restart` via WSL

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{info, warn};

use crate::state::SupervisorState;
use crate::wsl_util::wsl_command;

/// Probe interval: how often we check CI runner health.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum restart attempts per hour per service.
const MAX_RESTARTS_PER_HOUR: u32 = 3;

/// Window for rate-limiting restarts.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(3600);

/// Status of a CI runner service.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CiRunnerStatus {
    Idle,
    Busy,
    Offline,
}

impl CiRunnerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Offline => "offline",
        }
    }
}

/// Aggregate state of all CI runner services on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct CiRunnerState {
    pub status: CiRunnerStatus,
    pub labels: Vec<String>,
    pub service_names: Vec<String>,
    /// Whether a CI runner is installed on this host. Derived by the probe
    /// loop from service discovery (a discovered `actions.runner.*` service is
    /// itself proof of an install), with a `~/actions-runner/.runner`
    /// filesystem fallback for the classic tarball layout — see
    /// `derive_installed`. Deriving from services (rather than the filesystem
    /// check alone) is what makes this honest for systemd/service installs at
    /// any path. Cached here by the 30 s probe loop so the `/ci-runner/status`
    /// hot path never pays a per-request WSL cold-spawn — a raw `wsl.exe`
    /// invocation measures 18–35 s on Windows, which was aborting the frontend
    /// fetch and surfacing a spurious "Failed to reach supervisor" banner.
    pub installed: bool,
}

impl Default for CiRunnerState {
    fn default() -> Self {
        Self {
            status: CiRunnerStatus::Offline,
            labels: Vec::new(),
            service_names: Vec::new(),
            installed: false,
        }
    }
}

/// Rate-limiter for restart attempts: tracks timestamps of recent restarts
/// per service name.
pub struct RestartTracker {
    /// (service_name, restart_timestamp) pairs.
    attempts: Vec<(String, Instant)>,
}

impl Default for RestartTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartTracker {
    pub fn new() -> Self {
        Self {
            attempts: Vec::new(),
        }
    }

    /// Returns true if a restart is allowed for this service (under the
    /// rate limit of MAX_RESTARTS_PER_HOUR).
    fn may_restart(&self, service_name: &str) -> bool {
        let cutoff = Instant::now() - RATE_LIMIT_WINDOW;
        let recent = self
            .attempts
            .iter()
            .filter(|(name, ts)| name == service_name && *ts > cutoff)
            .count();
        (recent as u32) < MAX_RESTARTS_PER_HOUR
    }

    /// Record a restart attempt for the given service.
    fn record_restart(&mut self, service_name: &str) {
        // Prune entries older than the window while we're here.
        let cutoff = Instant::now() - RATE_LIMIT_WINDOW;
        self.attempts.retain(|(_, ts)| *ts > cutoff);
        self.attempts
            .push((service_name.to_string(), Instant::now()));
    }
}

/// Probe all CI runner services via WSL. This is a synchronous function
/// that shells out to `wsl` — callers should run it inside
/// `spawn_blocking`.
///
/// Returns the aggregate state: service names discovered, labels parsed
/// from the runner config, and overall status (idle/busy/offline).
pub fn probe_ci_runners() -> CiRunnerState {
    // Step 1: List active actions.runner.* services. A discovered
    // `actions.runner.*` service is itself proof of an install, so the
    // `installed` flag is derived from service discovery (see
    // `derive_installed`) rather than an independent filesystem check. That
    // makes it honest for systemd/service installs at *any* path — not just
    // the classic `~/actions-runner/.runner` tarball layout the old check
    // hard-coded — and it can never disagree with the services the probe
    // already found. When no service is discovered, `derive_installed` falls
    // back to the filesystem check so a configured-but-unregistered
    // classic-layout runner is still recognized.
    let service_names = match list_runner_services() {
        Ok(names) if !names.is_empty() => names,
        Ok(_) => {
            return CiRunnerState {
                installed: derive_installed(&[], crate::ci_runner_lifecycle::is_runner_installed),
                ..CiRunnerState::default()
            }
        }
        Err(e) => {
            tracing::debug!("ci_runner_probe: failed to list services: {}", e);
            return CiRunnerState {
                installed: derive_installed(&[], crate::ci_runner_lifecycle::is_runner_installed),
                ..CiRunnerState::default()
            };
        }
    };

    // Step 2: Check if any runner worker is busy.
    let is_busy = check_runner_busy();

    // Step 3: Check that at least one service is active (running).
    let any_active = service_names.iter().any(|s| check_service_active(s));

    // Step 4: Parse labels from the runner config.
    let labels = parse_runner_labels();

    let status = if !any_active {
        CiRunnerStatus::Offline
    } else if is_busy {
        CiRunnerStatus::Busy
    } else {
        CiRunnerStatus::Idle
    };

    // `service_names` is non-empty here, so `installed` is `true` without any
    // WSL filesystem round-trip (the `||` fallback short-circuits).
    let installed = derive_installed(
        &service_names,
        crate::ci_runner_lifecycle::is_runner_installed,
    );

    CiRunnerState {
        status,
        labels,
        service_names,
        installed,
    }
}

/// Derive whether a CI runner is installed on this host from the discovered
/// service list, with a filesystem fallback.
///
/// A discovered `actions.runner.*` service is itself proof of an install and
/// covers systemd/service installs at *any* path — so a non-empty
/// `service_names` sets `installed` on its own, with no path/layout
/// assumption. This is a single source of truth that cannot disagree with the
/// services the probe already found (the exact defect the old independent WSL
/// `~/actions-runner/.runner` check produced: `installed: false` while runner
/// services were actively busy).
///
/// When no service is discovered, `fs_fallback` is consulted so a classic
/// tarball-layout runner that was configured (`~/actions-runner/.runner`
/// present) but never registered as a service is still recognized. `||`
/// short-circuits, so the (WSL cold-spawn) fallback runs only when it is
/// actually needed.
fn derive_installed(service_names: &[String], fs_fallback: impl FnOnce() -> bool) -> bool {
    !service_names.is_empty() || fs_fallback()
}

/// List `actions.runner.*` systemd services via WSL.
fn list_runner_services() -> Result<Vec<String>, String> {
    let output = wsl_command()
        .args([
            "-e",
            "bash",
            "-c",
            "systemctl list-units --type=service --plain --no-legend 'actions.runner.*' 2>/dev/null",
        ])
        .output()
        .map_err(|e| format!("failed to run wsl: {}", e))?;

    if !output.status.success() {
        return Err(format!("wsl systemctl list-units exited {}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut services = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // systemctl --plain output: "unit.service loaded active running description..."
        // We want the first field (the unit name).
        if let Some(unit) = line.split_whitespace().next() {
            if unit.starts_with("actions.runner.") {
                services.push(unit.to_string());
            }
        }
    }
    Ok(services)
}

/// Check if any Runner.Worker process is alive (indicates a job is running).
fn check_runner_busy() -> bool {
    wsl_command()
        .args([
            "-e",
            "bash",
            "-c",
            "pgrep -f 'Runner.Worker' >/dev/null 2>&1",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a specific systemd service is active (running).
fn check_service_active(service_name: &str) -> bool {
    wsl_command()
        .args([
            "-e",
            "bash",
            "-c",
            &format!("systemctl is-active --quiet {} 2>/dev/null", service_name),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse labels from the runner's `.runner` JSON config file.
/// Falls back to a default set if the file can't be read.
fn parse_runner_labels() -> Vec<String> {
    let output = match wsl_command()
        .args([
            "-e",
            "bash",
            "-c",
            "cat ~/actions-runner/.runner 2>/dev/null",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec!["self-hosted".to_string()],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // The .runner file is JSON. We parse it to extract the agentName and
    // any configured labels. The file structure includes fields like
    // "agentName", "agentId", etc. Labels may also be in a sibling
    // `.credentials_rsaparams` or configured separately, but the agent
    // name is the primary useful field from .runner.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
        let mut labels = vec!["self-hosted".to_string()];

        // Extract agent name as a label if present.
        if let Some(name) = parsed.get("agentName").and_then(|v| v.as_str()) {
            if !name.is_empty() {
                labels.push(name.to_string());
            }
        }

        // Also try to extract the pool name / machine name from the service
        // naming convention: actions.runner.<org>-<repo>.<machine>
        if let Some(name) = parsed.get("agentName").and_then(|v| v.as_str()) {
            // Typical: "spaceship-wsl" — already added above.
            // Add the machine hostname as a label for fleet identification.
            if let Ok(hostname_output) = wsl_command().args(["-e", "hostname"]).output() {
                if hostname_output.status.success() {
                    let hostname = String::from_utf8_lossy(&hostname_output.stdout)
                        .trim()
                        .to_string();
                    if !hostname.is_empty() && !labels.contains(&hostname) && name != hostname {
                        labels.push(hostname);
                    }
                }
            }
        }

        labels
    } else {
        vec!["self-hosted".to_string()]
    }
}

/// Attempt to restart a CI runner service via WSL systemctl.
/// Returns Ok(()) on success, Err with a message on failure.
pub fn try_restart_ci_runner(service_name: &str) -> Result<(), String> {
    // Validate service name to prevent command injection.
    if !service_name.starts_with("actions.runner.") {
        return Err(format!(
            "refusing to restart non-runner service: {}",
            service_name
        ));
    }
    if service_name.contains([';', '|', '&', '$', '`']) {
        return Err(format!(
            "service name contains suspicious characters: {}",
            service_name
        ));
    }

    let output = wsl_command()
        .args(["-e", "sudo", "systemctl", "restart", service_name])
        .output()
        .map_err(|e| format!("failed to run wsl: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "systemctl restart {} exited {}: {}",
            service_name,
            output.status,
            stderr.trim()
        ))
    }
}

/// Background probe loop. Runs every 30 seconds, probes CI runner state
/// via WSL, stores the result on `SupervisorState::ci_runner_state`, and
/// auto-restarts crashed services (rate-limited).
pub async fn ci_runner_probe_loop(state: Arc<SupervisorState>) {
    let mut interval = tokio::time::interval(PROBE_INTERVAL);
    // Skip the immediate first tick to let startup settle.
    interval.tick().await;

    let mut restart_tracker = RestartTracker::new();
    // Track which services were previously online so we can detect crashes.
    let mut previously_online: Vec<String> = Vec::new();

    info!(
        "ci_runner_probe: starting probe loop (interval={}s)",
        PROBE_INTERVAL.as_secs()
    );

    loop {
        interval.tick().await;

        // Probe in a blocking thread since it runs synchronous Command calls.
        let probe_result = tokio::task::spawn_blocking(probe_ci_runners).await;

        let new_state = match probe_result {
            Ok(s) => s,
            Err(e) => {
                warn!("ci_runner_probe: spawn_blocking panicked: {}", e);
                CiRunnerState::default()
            }
        };

        // Detect services that went from online to offline and auto-restart.
        for prev_service in &previously_online {
            let still_present = new_state.service_names.contains(prev_service);
            let was_active_before = true; // it was in previously_online
            let is_active_now = still_present && check_service_active_async(prev_service).await;

            if was_active_before && !is_active_now && still_present {
                // Service crashed — attempt restart if within rate limit.
                if restart_tracker.may_restart(prev_service) {
                    info!(
                        "ci_runner_probe: service {} went offline, attempting restart",
                        prev_service
                    );
                    let service_name = prev_service.clone();
                    let restart_result =
                        tokio::task::spawn_blocking(move || try_restart_ci_runner(&service_name))
                            .await;

                    match restart_result {
                        Ok(Ok(())) => {
                            info!("ci_runner_probe: successfully restarted {}", prev_service);
                            restart_tracker.record_restart(prev_service);
                        }
                        Ok(Err(e)) => {
                            warn!("ci_runner_probe: failed to restart {}: {}", prev_service, e);
                            restart_tracker.record_restart(prev_service);
                        }
                        Err(e) => {
                            warn!(
                                "ci_runner_probe: restart spawn_blocking panicked for {}: {}",
                                prev_service, e
                            );
                        }
                    }
                } else {
                    warn!(
                        "ci_runner_probe: service {} offline but restart rate-limited \
                         (max {} per hour)",
                        prev_service, MAX_RESTARTS_PER_HOUR
                    );
                }
            }
        }

        // Update previously_online: services that are both present and active.
        previously_online = new_state
            .service_names
            .iter()
            .filter(|s| check_service_active_sync(s))
            .cloned()
            .collect();

        // Store the state for consumption by fleet heartbeat and health endpoint.
        {
            let mut guard = state.ci_runner_state.write().await;
            *guard = new_state;
        }

        tracing::debug!(
            "ci_runner_probe: tick complete, status={}",
            state.ci_runner_state.read().await.status.as_str()
        );
    }
}

/// Async wrapper around `check_service_active` (runs in spawn_blocking).
async fn check_service_active_async(service_name: &str) -> bool {
    let name = service_name.to_string();
    tokio::task::spawn_blocking(move || check_service_active(&name))
        .await
        .unwrap_or(false)
}

/// Sync check — used within the probe loop's blocking context to avoid
/// nested spawn_blocking. Identical to `check_service_active`.
fn check_service_active_sync(service_name: &str) -> bool {
    check_service_active(service_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_runner_status_as_str() {
        assert_eq!(CiRunnerStatus::Idle.as_str(), "idle");
        assert_eq!(CiRunnerStatus::Busy.as_str(), "busy");
        assert_eq!(CiRunnerStatus::Offline.as_str(), "offline");
    }

    #[test]
    fn ci_runner_state_default_is_offline() {
        let state = CiRunnerState::default();
        assert_eq!(state.status, CiRunnerStatus::Offline);
        assert!(state.labels.is_empty());
        assert!(state.service_names.is_empty());
    }

    #[test]
    fn restart_tracker_rate_limits() {
        let mut tracker = RestartTracker::new();
        let service = "actions.runner.test.host";

        // First three should be allowed.
        for _ in 0..MAX_RESTARTS_PER_HOUR {
            assert!(tracker.may_restart(service));
            tracker.record_restart(service);
        }
        // Fourth should be denied.
        assert!(!tracker.may_restart(service));
    }

    #[test]
    fn restart_tracker_different_services_independent() {
        let mut tracker = RestartTracker::new();

        for _ in 0..MAX_RESTARTS_PER_HOUR {
            tracker.record_restart("actions.runner.a");
        }
        // Service "a" is rate-limited.
        assert!(!tracker.may_restart("actions.runner.a"));
        // Service "b" is independent.
        assert!(tracker.may_restart("actions.runner.b"));
    }

    #[test]
    fn try_restart_rejects_non_runner_service() {
        let result = try_restart_ci_runner("nginx.service");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("refusing"));
    }

    #[test]
    fn try_restart_rejects_injection_attempt() {
        let result = try_restart_ci_runner("actions.runner.test; rm -rf /");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("suspicious"));
    }

    #[test]
    fn derive_installed_true_when_service_present_without_runner_file() {
        // A discovered systemd/service runner with NO ~/actions-runner/.runner
        // file must still report installed: true — this is the systemd
        // false-negative the fix targets. The filesystem fallback must not even
        // be consulted (short-circuit), so we panic if it runs.
        let services = vec!["actions.runner.qontinui-qontinui-coord.spaceship-wsl".to_string()];
        assert!(derive_installed(&services, || panic!(
            "fs fallback must not run when a runner service is discovered"
        )));
    }

    #[test]
    fn derive_installed_false_when_no_runner_of_any_style() {
        // No service discovered AND no classic-layout runner file ⇒ not installed.
        assert!(!derive_installed(&[], || false));
    }

    #[test]
    fn derive_installed_true_when_classic_layout_file_present_but_no_service() {
        // Configured-but-unregistered classic tarball runner: no service, but
        // ~/actions-runner/.runner exists ⇒ the fs fallback keeps installed: true.
        assert!(derive_installed(&[], || true));
    }
}

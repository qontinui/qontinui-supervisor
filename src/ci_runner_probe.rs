//! WSL-based CI runner health monitoring (Phase 3b of self-hosted CI runners).
//!
//! GitHub Actions self-hosted runners run as WSL systemd services (e.g.
//! `actions.runner.qontinui-qontinui-coord.spaceship-wsl`), NOT as
//! supervisor-managed child processes. The supervisor cannot use its existing
//! `child.wait()` pattern — it must probe via `wsl` commands.
//!
//! # A monitor must not keep its subject alive
//!
//! This module used to fan out 8–9 `wsl -e …` calls every 30 s. **Each of
//! those starts the distro if it is down, and its exit re-arms WSL's poweroff
//! timer** — so the watchdog produced the liveness it reported and destroyed
//! it 60 s later (34 distro poweroff/boot cycles in 2h20m on MSI, each one
//! killing the CI job the freshly-woken runner had just claimed). Plan
//! `2026-08-21-supervisor-watchdog-observer-effect`.
//!
//! Two structural changes make it an observer instead of a participant:
//!
//! 1. **The non-waking gate lives at the spawn boundary**, in
//!    [`crate::wsl_util::wsl_command`] — not here, and not per call site. See
//!    that module for why.
//! 2. **One `wsl -e bash -c '<script>'` per tick**, emitting a single
//!    parseable block: units, per-unit active state, per-unit
//!    `WorkingDirectory`, per-unit `.runner`, busy flag, hostname, and the
//!    installed fallback. Nothing else re-probes: the restart arm reads that
//!    block's active map rather than issuing its own `is-active` calls.
//!
//! This module provides:
//! - [`probe_ci_runners`]: one gated, collapsed probe
//! - [`ci_runner_probe_loop`]: async 30s loop that stores state + auto-restarts
//! - [`try_restart_ci_runner`]: rate-limited `systemctl restart` via WSL
//!
//! Every piece of parsing is a pure function over `&str` with a thin spawn
//! wrapper around it, following the seam [`derive_installed`] established, so
//! the decisions are unit-testable without a WSL installation.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{info, warn};

use crate::state::SupervisorState;
use crate::wsl_util::{wsl_command, WslUnavailable};

/// Probe interval: how often we check CI runner health.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum restart attempts per hour per service.
const MAX_RESTARTS_PER_HOUR: u32 = 3;

/// Window for rate-limiting restarts.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(3600);

/// How many consecutive `DistroDown` ticks between repeats of the host-level
/// diagnostic. The first tick of a streak always logs; after that one line
/// every ~10 minutes is enough to keep the fault visible without flooding.
const DISTRO_DOWN_LOG_EVERY: u64 = 20;

/// Default location of the host-level WSL keepalive script (the thing that is
/// actually responsible for distro liveness — see the plan's Design principle:
/// the keepalive owns liveness, the probe owns reporting).
const DEFAULT_KEEPALIVE_SCRIPT: &str = r"C:\claude\scripts\wsl-keepalive.ps1";

/// Default location of the keepalive's documented disable flag.
const DEFAULT_KEEPALIVE_DISABLE_FLAG: &str = r"C:\claude\wsl-keepalive.disabled";

const KEEPALIVE_SCRIPT_ENV: &str = "QONTINUI_WSL_KEEPALIVE_SCRIPT";
const KEEPALIVE_DISABLE_FLAG_ENV: &str = "QONTINUI_WSL_KEEPALIVE_DISABLE_FLAG";

/// Status of a CI runner service.
///
/// `DistroDown` and `ProbeFailed` exist because the old three-variant model
/// forced every failure to answer `Offline` — a definitive verdict about the
/// runner derived from a failure to reach the thing that would know. They are
/// deliberately **not** collapsed into one `Unknown`: the runner client already
/// ships an `"unknown"` display state meaning *the supervisor was unreachable*,
/// which is a different fact about a different hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CiRunnerStatus {
    Idle,
    Busy,
    /// Distro up, no runner service active.
    Offline,
    /// The WSL distro is not running, so no runner can be online. Never a
    /// reason to `systemctl restart` — that is a host-level fault.
    DistroDown,
    /// The probe itself failed. NOT "offline": we do not know.
    ProbeFailed,
}

impl CiRunnerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Offline => "offline",
            Self::DistroDown => "distro_down",
            Self::ProbeFailed => "probe_failed",
        }
    }

    /// Whether this reading is evidence about the *services* at all.
    ///
    /// `DistroDown` and `ProbeFailed` say something about the host or the
    /// probe, not about a unit — so the restart arm must not act on them.
    fn is_service_evidence(&self) -> bool {
        matches!(self, Self::Idle | Self::Busy | Self::Offline)
    }
}

/// Aggregate state of all CI runner services on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct CiRunnerState {
    pub status: CiRunnerStatus,
    pub labels: Vec<String>,
    pub service_names: Vec<String>,
    /// The subset of `service_names` that `systemctl is-active` reported
    /// active in **this** tick's collapsed script. A service present in
    /// `service_names` but absent here is confirmed inactive — which is the
    /// only evidence the restart arm acts on.
    pub active_service_names: Vec<String>,
    /// Whether a CI runner is installed on this host. Derived by the probe
    /// from service discovery (a discovered `actions.runner.*` service is
    /// itself proof of an install), with a filesystem fallback for the
    /// classic tarball layout — see [`derive_installed`]. Both signals come
    /// out of the same single collapsed script, so the fallback costs no
    /// extra WSL spawn.
    ///
    /// When the probe could not answer (distro down, probe failed) this
    /// carries the **last known** value forward rather than flipping to
    /// `false`: a failure to look is not evidence of absence.
    pub installed: bool,
}

impl Default for CiRunnerState {
    fn default() -> Self {
        Self {
            // Pre-first-probe placeholder. The probe loop overwrites this
            // within one interval; every *failure* path constructs an explicit
            // `DistroDown`/`ProbeFailed` state instead of falling back here.
            status: CiRunnerStatus::Offline,
            labels: Vec::new(),
            service_names: Vec::new(),
            active_service_names: Vec::new(),
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

// ---------------------------------------------------------------------------
// The single collapsed probe script (§2)
// ---------------------------------------------------------------------------

/// One `bash -c` script producing everything a tick needs, as a tab-separated
/// block. Written as a single line on purpose — it crosses a Windows command
/// line — and as a Rust raw string so `\t` / `\n` reach `printf` as format
/// escapes rather than as literal control characters in the argument.
///
/// Per unit it emits the unit name, its `is-active` word, and its
/// `WorkingDirectory` (falling back to the directory of `ExecStart`'s path).
/// That working directory is what removes the hardcoded `~/actions-runner`
/// assumption from label derivation (D6) instead of replacing it with a better
/// guess. `--all` is load-bearing: without it `list-units` hides a *stopped*
/// unit entirely, which is exactly the state the restart arm exists to detect.
const PROBE_SCRIPT: &str = r#"units=$(systemctl list-units --type=service --plain --no-legend --all 'actions.runner.*' 2>/dev/null | awk '{print $1}'); for u in $units; do case "$u" in actions.runner.*) ;; *) continue ;; esac; st=$(systemctl is-active "$u" 2>/dev/null || true); wd=$(systemctl show -p WorkingDirectory --value "$u" 2>/dev/null || true); if [ -z "$wd" ]; then p=$(systemctl show -p ExecStart --value "$u" 2>/dev/null | sed -n 's/.*path=\([^ ;]*\).*/\1/p'); if [ -n "$p" ]; then wd=$(dirname "$p"); fi; fi; printf 'UNIT\t%s\t%s\t%s\n' "$u" "$st" "$wd"; if [ -n "$wd" ] && [ -f "$wd/.runner" ]; then printf 'RUNNERFILE\t%s\t%s\n' "$u" "$(tr -d '\n\r\t' < "$wd/.runner")"; fi; done; if pgrep -f 'Runner.Worker' >/dev/null 2>&1; then printf 'BUSY\t1\n'; else printf 'BUSY\t0\n'; fi; printf 'HOSTNAME\t%s\n' "$(hostname 2>/dev/null || cat /proc/sys/kernel/hostname 2>/dev/null || true)"; fb=0; for f in "$HOME"/actions-runner*/.runner /home/*/actions-runner*/.runner /root/actions-runner*/.runner; do if [ -f "$f" ]; then fb=1; break; fi; done; printf 'INSTALLED_FALLBACK\t%s\n' "$fb"; printf 'PROBE_END\t1\n'"#;

/// One discovered `actions.runner.*` systemd unit, as observed by a single
/// run of [`PROBE_SCRIPT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitObservation {
    pub name: String,
    /// The raw `systemctl is-active` word (`active`, `inactive`, `failed`, …).
    pub active_state: String,
    /// `systemctl show -p WorkingDirectory`, or the directory of `ExecStart`.
    pub working_dir: Option<String>,
    /// The unit's own `.runner` JSON, read from its own directory.
    pub runner_file: Option<String>,
}

impl UnitObservation {
    pub fn is_active(&self) -> bool {
        self.active_state == "active"
    }
}

/// Everything one tick's collapsed script observed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeSnapshot {
    pub units: Vec<UnitObservation>,
    pub busy: bool,
    pub hostname: Option<String>,
    /// A `.runner` file exists somewhere on the host even though no unit was
    /// discovered (classic configured-but-unregistered tarball layout).
    pub installed_fallback: bool,
}

/// Parse [`PROBE_SCRIPT`]'s output.
///
/// Returns `Err` when the terminating `PROBE_END` marker is missing: a
/// truncated or garbled block is UNKNOWN, and must map to `ProbeFailed`
/// rather than being read as "no services, therefore offline".
pub fn parse_probe_output(raw: &str) -> Result<ProbeSnapshot, String> {
    let mut snapshot = ProbeSnapshot::default();
    let mut saw_end = false;

    for line in raw.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let Some(key) = fields.next() else { continue };
        match key {
            "UNIT" => {
                let Some(name) = fields.next() else { continue };
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                let active_state = fields.next().unwrap_or("").trim().to_string();
                let working_dir = fields
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                snapshot.units.push(UnitObservation {
                    name: name.to_string(),
                    active_state,
                    working_dir,
                    runner_file: None,
                });
            }
            "RUNNERFILE" => {
                let Some(name) = fields.next() else { continue };
                let name = name.trim();
                // The rest of the line is the (newline-stripped) JSON body.
                let body: Vec<&str> = fields.collect();
                let body = body.join("\t");
                if body.trim().is_empty() {
                    continue;
                }
                if let Some(unit) = snapshot.units.iter_mut().find(|u| u.name == name) {
                    unit.runner_file = Some(body.trim().to_string());
                }
            }
            "BUSY" => {
                snapshot.busy = fields.next().map(str::trim) == Some("1");
            }
            "HOSTNAME" => {
                snapshot.hostname = fields
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            "INSTALLED_FALLBACK" => {
                snapshot.installed_fallback = fields.next().map(str::trim) == Some("1");
            }
            "PROBE_END" => saw_end = true,
            _ => {}
        }
    }

    if !saw_end {
        return Err(
            "probe script output is missing its PROBE_END marker (truncated or garbled)"
                .to_string(),
        );
    }
    Ok(snapshot)
}

/// Extract `agentName` from a `.runner` JSON body.
fn agent_name_from_runner_file(json: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    parsed
        .get("agentName")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Recover the machine name a unit encodes:
/// `actions.runner.<org>-<repo>.<machine>.service` → `<machine>`.
///
/// This is the path-free half of the D6 fix: it still answers when the unit's
/// `.runner` file cannot be read at all.
pub fn machine_name_from_unit(unit: &str) -> Option<String> {
    let rest = unit.strip_prefix("actions.runner.")?;
    let rest = rest.strip_suffix(".service").unwrap_or(rest);
    let (_org_repo, machine) = rest.rsplit_once('.')?;
    let machine = machine.trim();
    if machine.is_empty() {
        None
    } else {
        Some(machine.to_string())
    }
}

/// Derive the runner's labels from what was actually discovered.
///
/// The old implementation read a hardcoded `~/actions-runner/.runner` as the
/// default WSL user. MSI's runners live at `/home/runner/actions-runner-<repo>/`
/// under a separate `runner` user, so the read failed and every label set
/// silently degraded to `["self-hosted"]`. Here every source is discovered:
/// each unit's own `.runner` (read from its own `WorkingDirectory`), the
/// machine name encoded in the unit name as a fallback, and the hostname from
/// the same collapsed script.
pub fn derive_labels(units: &[UnitObservation], hostname: Option<&str>) -> Vec<String> {
    let mut labels = vec!["self-hosted".to_string()];
    let mut seen: BTreeSet<String> = labels.iter().cloned().collect();

    for unit in units {
        let name = unit
            .runner_file
            .as_deref()
            .and_then(agent_name_from_runner_file)
            .or_else(|| machine_name_from_unit(&unit.name));
        if let Some(name) = name {
            if seen.insert(name.clone()) {
                labels.push(name);
            }
        }
    }

    if let Some(host) = hostname.map(str::trim).filter(|h| !h.is_empty()) {
        if seen.insert(host.to_string()) {
            labels.push(host.to_string());
        }
    }

    labels
}

/// Map a snapshot to a status. Only reached when the distro is up and the
/// script ran to completion — the failure statuses are constructed by the
/// caller, never derived here.
pub fn derive_status(snapshot: &ProbeSnapshot) -> CiRunnerStatus {
    if !snapshot.units.iter().any(UnitObservation::is_active) {
        CiRunnerStatus::Offline
    } else if snapshot.busy {
        CiRunnerStatus::Busy
    } else {
        CiRunnerStatus::Idle
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
/// tarball-layout runner that was configured but never registered as a service
/// is still recognized. `||` short-circuits, and the fallback is now a field of
/// the same collapsed script rather than a second WSL spawn.
fn derive_installed(service_names: &[String], fs_fallback: impl FnOnce() -> bool) -> bool {
    !service_names.is_empty() || fs_fallback()
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Run [`PROBE_SCRIPT`] inside WSL. The **only** `wsl -e` spawn in a tick.
fn run_probe_script() -> Result<String, String> {
    let output = wsl_command()
        .map_err(|e| e.to_string())?
        .args(["-e", "bash", "-c", PROBE_SCRIPT])
        .output()
        .map_err(|e| format!("failed to run wsl: {e}"))?;

    // The script's own exit status is the status of its last `printf`, so a
    // non-zero exit means wsl/bash itself failed — the block is untrustworthy.
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "probe script exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Probe all CI runner services via WSL. Synchronous — callers run it inside
/// `spawn_blocking`.
///
/// `previous_installed` is carried forward when the probe cannot answer; see
/// [`CiRunnerState::installed`].
pub fn probe_ci_runners(previous_installed: bool) -> CiRunnerState {
    probe_ci_runners_with(
        previous_installed,
        crate::wsl_util::ensure_distro_running,
        run_probe_script,
    )
}

/// The testable core: the gate and the single script spawn are both injected,
/// so a test can assert **how many** `-e` spawns a tick makes (0 when the
/// distro is down) without a WSL installation.
pub fn probe_ci_runners_with(
    previous_installed: bool,
    gate: impl FnOnce() -> Result<(), WslUnavailable>,
    run_script: impl FnOnce() -> Result<String, String>,
) -> CiRunnerState {
    // Step 0 — the non-waking liveness gate. When the distro is down we issue
    // NO `wsl -e` command at all this tick: that is what converts this from a
    // participant into an observer.
    if let Err(e) = gate() {
        let status = match e {
            WslUnavailable::DistroDown { .. } => CiRunnerStatus::DistroDown,
            WslUnavailable::GateFailed(ref msg) => {
                tracing::debug!("ci_runner_probe: liveness gate failed: {msg}");
                CiRunnerStatus::ProbeFailed
            }
        };
        tracing::debug!("ci_runner_probe: {e}");
        return CiRunnerState {
            status,
            installed: previous_installed,
            ..CiRunnerState::default()
        };
    }

    // Step 1 — the one and only WSL spawn of this tick.
    let raw = match run_script() {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!("ci_runner_probe: probe script failed: {e}");
            return CiRunnerState {
                status: CiRunnerStatus::ProbeFailed,
                installed: previous_installed,
                ..CiRunnerState::default()
            };
        }
    };

    let snapshot = match parse_probe_output(&raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("ci_runner_probe: unparseable probe output: {e}");
            return CiRunnerState {
                status: CiRunnerStatus::ProbeFailed,
                installed: previous_installed,
                ..CiRunnerState::default()
            };
        }
    };

    let service_names: Vec<String> = snapshot.units.iter().map(|u| u.name.clone()).collect();
    let active_service_names: Vec<String> = snapshot
        .units
        .iter()
        .filter(|u| u.is_active())
        .map(|u| u.name.clone())
        .collect();

    CiRunnerState {
        status: derive_status(&snapshot),
        labels: derive_labels(&snapshot.units, snapshot.hostname.as_deref()),
        installed: derive_installed(&service_names, || snapshot.installed_fallback),
        service_names,
        active_service_names,
    }
}

// ---------------------------------------------------------------------------
// Restart (§4)
// ---------------------------------------------------------------------------

/// Attempt to restart a CI runner service via WSL systemctl.
/// Returns Ok(()) on success, Err with a message on failure.
///
/// The gate at the spawn boundary makes this structurally incapable of
/// **booting** a stopped distro, which is what the old restart arm did while
/// reporting success — masking a host fault as a service fault.
pub fn try_restart_ci_runner(service_name: &str) -> Result<(), String> {
    // Validate service name to prevent command injection.
    if !service_name.starts_with("actions.runner.") {
        return Err(format!(
            "refusing to restart non-runner service: {service_name}"
        ));
    }
    if service_name.contains([';', '|', '&', '$', '`']) {
        return Err(format!(
            "service name contains suspicious characters: {service_name}"
        ));
    }

    let output = wsl_command()
        .map_err(|e| e.to_string())?
        .args(["-e", "sudo", "systemctl", "restart", service_name])
        .output()
        .map_err(|e| format!("failed to run wsl: {e}"))?;

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

/// What the restart arm should do about one previously-online service, given
/// this tick's evidence. Pure so the "only on evidence" rule is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// The unit is confirmed active — nothing to do.
    StillActive,
    /// Distro confirmed running AND unit confirmed inactive: restart.
    Restart,
    /// The unit is no longer discovered at all while the distro is up: it was
    /// removed or unloaded, which is not a crash.
    UnitGone,
    /// We learned nothing about the unit this tick.
    NoEvidence(&'static str),
}

/// Decide the restart arm for one previously-online service.
///
/// This replaces the old `let was_active_before = true;` tautology and the
/// `still_present` gate, which between them made the arm unable to fire in the
/// real failure mode (distro down ⇒ empty service list ⇒ `still_present`
/// false) while firing a distro-booting `systemctl restart` in others.
pub fn decide_restart(
    service: &str,
    status: &CiRunnerStatus,
    previous_status_was_service_evidence: bool,
    service_names: &[String],
    active_service_names: &[String],
) -> RestartDecision {
    if !status.is_service_evidence() {
        return RestartDecision::NoEvidence(match status {
            CiRunnerStatus::DistroDown => "distro is not running",
            _ => "probe failed",
        });
    }
    if !previous_status_was_service_evidence {
        // The previous reading was DistroDown/ProbeFailed, so "previously
        // online" is stale by at least one tick and a unit may simply not have
        // finished starting. Re-establish the baseline before acting.
        return RestartDecision::NoEvidence("previous tick had no service evidence");
    }
    if active_service_names.iter().any(|s| s == service) {
        return RestartDecision::StillActive;
    }
    if service_names.iter().any(|s| s == service) {
        RestartDecision::Restart
    } else {
        RestartDecision::UnitGone
    }
}

// ---------------------------------------------------------------------------
// Keepalive reporting (§4 diagnostic)
// ---------------------------------------------------------------------------

/// Whether the host-level WSL keepalive — the thing that actually owns distro
/// liveness — appears to be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepalivePresence {
    /// The keepalive script is present and not disabled.
    Present,
    /// Present, but its documented disable flag exists, so it is holding
    /// nothing open on purpose.
    DisabledByFlag,
    /// No keepalive script at all — nothing is holding the distro open.
    Absent,
}

impl KeepalivePresence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Present => "keepalive script present (not proof it is running)",
            Self::DisabledByFlag => "keepalive script present but DISABLED by its flag file",
            Self::Absent => "NO keepalive script found",
        }
    }
}

/// Pure classifier for [`KeepalivePresence`].
pub fn classify_keepalive(script_present: bool, disable_flag_present: bool) -> KeepalivePresence {
    match (script_present, disable_flag_present) {
        (false, _) => KeepalivePresence::Absent,
        (true, true) => KeepalivePresence::DisabledByFlag,
        (true, false) => KeepalivePresence::Present,
    }
}

fn keepalive_script_path() -> PathBuf {
    std::env::var(KEEPALIVE_SCRIPT_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_KEEPALIVE_SCRIPT))
}

fn keepalive_disable_flag_path() -> PathBuf {
    std::env::var(KEEPALIVE_DISABLE_FLAG_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_KEEPALIVE_DISABLE_FLAG))
}

/// Observe the keepalive from the filesystem. Cheap (two `exists()` calls) and
/// only done on a `DistroDown` tick.
fn observe_keepalive() -> (KeepalivePresence, PathBuf) {
    let script = keepalive_script_path();
    let flag = keepalive_disable_flag_path();
    (classify_keepalive(script.exists(), flag.exists()), script)
}

// ---------------------------------------------------------------------------
// Probe loop
// ---------------------------------------------------------------------------

/// Background probe loop. Runs every 30 seconds, probes CI runner state
/// via WSL (one gated `wsl -e` per tick, none at all when the distro is
/// down), stores the result on `SupervisorState::ci_runner_state`, and
/// auto-restarts crashed services (rate-limited) — but only on evidence that
/// the distro is up and the unit is inactive.
pub async fn ci_runner_probe_loop(state: Arc<SupervisorState>) {
    let mut interval = tokio::time::interval(PROBE_INTERVAL);
    // Skip the immediate first tick to let startup settle.
    interval.tick().await;

    let mut restart_tracker = RestartTracker::new();
    // Track which services were previously online so we can detect crashes.
    let mut previously_online: Vec<String> = Vec::new();
    // Last known `installed`, carried across ticks the probe could not answer.
    let mut last_known_installed = false;
    // Whether the previous tick produced evidence about the services.
    let mut previous_was_service_evidence = false;
    // Length of the current consecutive `DistroDown` streak.
    let mut distro_down_ticks: u64 = 0;

    info!(
        "ci_runner_probe: starting probe loop (interval={}s)",
        PROBE_INTERVAL.as_secs()
    );

    loop {
        interval.tick().await;

        // Probe in a blocking thread since it runs synchronous Command calls.
        let carried = last_known_installed;
        let probe_result = tokio::task::spawn_blocking(move || probe_ci_runners(carried)).await;

        let new_state = match probe_result {
            Ok(s) => s,
            Err(e) => {
                warn!("ci_runner_probe: spawn_blocking panicked: {e}");
                CiRunnerState {
                    status: CiRunnerStatus::ProbeFailed,
                    installed: carried,
                    ..CiRunnerState::default()
                }
            }
        };
        last_known_installed = new_state.installed;

        // §4 — a distro-down reading is a HOST-level fault. Never restart a
        // unit for it, never consume the restart budget, and say what the
        // actual owner of distro liveness looks like.
        if new_state.status == CiRunnerStatus::DistroDown {
            distro_down_ticks += 1;
            if distro_down_ticks == 1 || distro_down_ticks.is_multiple_of(DISTRO_DOWN_LOG_EVERY) {
                let (keepalive, script_path) = observe_keepalive();
                warn!(
                    "ci_runner_probe: WSL distro is NOT running ({} consecutive tick(s)). \
                     CI runners cannot be online, and this is a HOST-level fault — no \
                     `systemctl restart` will be attempted and the restart budget is \
                     untouched, because restarting through WSL would merely boot the \
                     distro and mask the cause. Distro liveness is owned by the \
                     keepalive, not by this probe: {} (looked at {}).",
                    distro_down_ticks,
                    keepalive.as_str(),
                    script_path.display()
                );
            }
        } else {
            distro_down_ticks = 0;
        }

        // Detect services that went from online to offline and auto-restart.
        for prev_service in &previously_online {
            match decide_restart(
                prev_service,
                &new_state.status,
                previous_was_service_evidence,
                &new_state.service_names,
                &new_state.active_service_names,
            ) {
                RestartDecision::StillActive => {}
                RestartDecision::UnitGone => {
                    warn!(
                        "ci_runner_probe: service {prev_service} is no longer a loaded unit \
                         (removed or unconfigured) — not a crash, no restart attempted"
                    );
                }
                RestartDecision::NoEvidence(reason) => {
                    tracing::debug!(
                        "ci_runner_probe: no restart decision for {prev_service}: {reason}"
                    );
                }
                RestartDecision::Restart => {
                    if restart_tracker.may_restart(prev_service) {
                        info!(
                            "ci_runner_probe: service {prev_service} is inactive while the \
                             distro is running, attempting restart"
                        );
                        let service_name = prev_service.clone();
                        let restart_result = tokio::task::spawn_blocking(move || {
                            try_restart_ci_runner(&service_name)
                        })
                        .await;

                        match restart_result {
                            Ok(Ok(())) => {
                                info!("ci_runner_probe: successfully restarted {prev_service}");
                                restart_tracker.record_restart(prev_service);
                            }
                            Ok(Err(e)) => {
                                warn!("ci_runner_probe: failed to restart {prev_service}: {e}");
                                restart_tracker.record_restart(prev_service);
                            }
                            Err(e) => {
                                warn!(
                                    "ci_runner_probe: restart spawn_blocking panicked for \
                                     {prev_service}: {e}"
                                );
                            }
                        }
                    } else {
                        warn!(
                            "ci_runner_probe: service {prev_service} offline but restart \
                             rate-limited (max {MAX_RESTARTS_PER_HOUR} per hour)"
                        );
                    }
                }
            }
        }

        // Refresh the baseline from the SAME collapsed reading — no re-probe,
        // and therefore no blocking call left in the async loop body (D4/D7).
        // A tick with no service evidence leaves the baseline alone: we did not
        // learn that anything went offline.
        if new_state.status.is_service_evidence() {
            previously_online = new_state.active_service_names.clone();
        }
        previous_was_service_evidence = new_state.status.is_service_evidence();

        // Store the state for consumption by the `/ci-runner/status` endpoint.
        {
            let mut guard = state.ci_runner_state.write().await;
            *guard = new_state;
        }

        // The spawn counters are the observer-effect instrument: on a healthy
        // box `wsl_exec_spawns_total` must advance by at most 1 per tick and
        // not at all while the distro is down, while `gate_reads_total` (the
        // non-waking `wsl --list` reads) may advance freely.
        tracing::debug!(
            "ci_runner_probe: tick complete, status={}, wsl_exec_spawns_total={},              gate_reads_total={}",
            state.ci_runner_state.read().await.status.as_str(),
            crate::wsl_util::gated_spawn_count(),
            crate::wsl_util::gate_spawn_count()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // -- status model -------------------------------------------------------

    #[test]
    fn ci_runner_status_as_str() {
        assert_eq!(CiRunnerStatus::Idle.as_str(), "idle");
        assert_eq!(CiRunnerStatus::Busy.as_str(), "busy");
        assert_eq!(CiRunnerStatus::Offline.as_str(), "offline");
        assert_eq!(CiRunnerStatus::DistroDown.as_str(), "distro_down");
        assert_eq!(CiRunnerStatus::ProbeFailed.as_str(), "probe_failed");
    }

    #[test]
    fn serde_serialization_matches_as_str() {
        // The derive would otherwise emit "DistroDown" while the route emits
        // "distro_down" — a drift waiting for the first direct serializer.
        for status in [
            CiRunnerStatus::Idle,
            CiRunnerStatus::Busy,
            CiRunnerStatus::Offline,
            CiRunnerStatus::DistroDown,
            CiRunnerStatus::ProbeFailed,
        ] {
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
    }

    #[test]
    fn only_real_readings_count_as_service_evidence() {
        assert!(CiRunnerStatus::Idle.is_service_evidence());
        assert!(CiRunnerStatus::Busy.is_service_evidence());
        assert!(CiRunnerStatus::Offline.is_service_evidence());
        assert!(!CiRunnerStatus::DistroDown.is_service_evidence());
        assert!(!CiRunnerStatus::ProbeFailed.is_service_evidence());
    }

    #[test]
    fn ci_runner_state_default_is_offline() {
        let state = CiRunnerState::default();
        assert_eq!(state.status, CiRunnerStatus::Offline);
        assert!(state.labels.is_empty());
        assert!(state.service_names.is_empty());
        assert!(state.active_service_names.is_empty());
    }

    // -- rate limiting ------------------------------------------------------

    #[test]
    fn restart_tracker_rate_limits() {
        let mut tracker = RestartTracker::new();
        let service = "actions.runner.test.host";

        for _ in 0..MAX_RESTARTS_PER_HOUR {
            assert!(tracker.may_restart(service));
            tracker.record_restart(service);
        }
        assert!(!tracker.may_restart(service));
    }

    #[test]
    fn restart_tracker_different_services_independent() {
        let mut tracker = RestartTracker::new();

        for _ in 0..MAX_RESTARTS_PER_HOUR {
            tracker.record_restart("actions.runner.a");
        }
        assert!(!tracker.may_restart("actions.runner.a"));
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

    // -- derive_installed ---------------------------------------------------

    #[test]
    fn derive_installed_true_when_service_present_without_runner_file() {
        let services = vec!["actions.runner.qontinui-qontinui-coord.spaceship-wsl".to_string()];
        assert!(derive_installed(&services, || panic!(
            "fs fallback must not run when a runner service is discovered"
        )));
    }

    #[test]
    fn derive_installed_false_when_no_runner_of_any_style() {
        assert!(!derive_installed(&[], || false));
    }

    #[test]
    fn derive_installed_true_when_classic_layout_file_present_but_no_service() {
        assert!(derive_installed(&[], || true));
    }

    // -- collapsed script parsing -------------------------------------------

    fn sample_output() -> String {
        [
            "UNIT\tactions.runner.qontinui-qontinui-coord.msi-wsl.service\tactive\t/home/runner/actions-runner-coord",
            "RUNNERFILE\tactions.runner.qontinui-qontinui-coord.msi-wsl.service\t{\"agentName\": \"msi-wsl\", \"agentId\": 7}",
            "UNIT\tactions.runner.qontinui-qontinui-web.msi-wsl2.service\tinactive\t/home/runner/actions-runner-web",
            "BUSY\t0",
            "HOSTNAME\tmsi-wsl-host",
            "INSTALLED_FALLBACK\t0",
            "PROBE_END\t1",
        ]
        .join("\n")
    }

    #[test]
    fn parses_the_collapsed_block() {
        let snap = parse_probe_output(&sample_output()).expect("parses");
        assert_eq!(snap.units.len(), 2);
        assert!(snap.units[0].is_active());
        assert!(!snap.units[1].is_active());
        assert_eq!(
            snap.units[0].working_dir.as_deref(),
            Some("/home/runner/actions-runner-coord")
        );
        assert!(snap.units[0].runner_file.is_some());
        assert!(snap.units[1].runner_file.is_none());
        assert!(!snap.busy);
        assert_eq!(snap.hostname.as_deref(), Some("msi-wsl-host"));
        assert!(!snap.installed_fallback);
    }

    #[test]
    fn parses_empty_but_complete_block() {
        let snap =
            parse_probe_output("BUSY\t0\nHOSTNAME\tbox\nINSTALLED_FALLBACK\t1\nPROBE_END\t1")
                .expect("parses");
        assert!(snap.units.is_empty());
        assert!(snap.installed_fallback);
    }

    #[test]
    fn truncated_block_is_an_error_not_an_empty_reading() {
        // Without the end marker we cannot tell "no services" from "the block
        // was cut off" — and reading the second as Offline is exactly the
        // conflation this plan removes.
        let truncated = "UNIT\tactions.runner.a.b.service\tactive\t/home/runner/a";
        assert!(parse_probe_output(truncated).is_err());
        assert!(parse_probe_output("").is_err());
    }

    #[test]
    fn malformed_lines_are_skipped_without_inventing_units() {
        let raw = "UNIT\n\nUNIT\t\t\t\nNOISE\tx\ny\nPROBE_END\t1";
        let snap = parse_probe_output(raw).expect("parses");
        assert!(snap.units.is_empty());
    }

    #[test]
    fn busy_flag_is_read() {
        let raw = "UNIT\tactions.runner.a.b.service\tactive\t/x\nBUSY\t1\nPROBE_END\t1";
        let snap = parse_probe_output(raw).expect("parses");
        assert!(snap.busy);
        assert_eq!(derive_status(&snap), CiRunnerStatus::Busy);
    }

    // -- status derivation --------------------------------------------------

    #[test]
    fn status_is_offline_when_no_unit_is_active() {
        let snap = parse_probe_output(
            "UNIT\tactions.runner.a.b.service\tinactive\t/x\nBUSY\t0\nPROBE_END\t1",
        )
        .expect("parses");
        assert_eq!(derive_status(&snap), CiRunnerStatus::Offline);
    }

    #[test]
    fn status_is_idle_when_active_and_not_busy() {
        let snap = parse_probe_output(&sample_output()).expect("parses");
        assert_eq!(derive_status(&snap), CiRunnerStatus::Idle);
    }

    // -- labels (D6) --------------------------------------------------------

    #[test]
    fn labels_come_from_each_runners_own_directory() {
        let snap = parse_probe_output(&sample_output()).expect("parses");
        let labels = derive_labels(&snap.units, snap.hostname.as_deref());
        // Not the old degraded `["self-hosted"]`.
        assert_eq!(
            labels,
            vec![
                "self-hosted".to_string(),
                // from the unit's own `.runner`
                "msi-wsl".to_string(),
                // from the unit name, because that unit's `.runner` was unreadable
                "msi-wsl2".to_string(),
                "msi-wsl-host".to_string(),
            ]
        );
    }

    #[test]
    fn labels_do_not_degrade_when_no_runner_file_is_readable() {
        // MSI's layout: the runners are NOT at `~/actions-runner`, so the old
        // hardcoded read failed and every label collapsed to `["self-hosted"]`.
        let raw = "UNIT\tactions.runner.qontinui-qontinui-coord.msi-wsl.service\tactive\t\nBUSY\t0\nPROBE_END\t1";
        let snap = parse_probe_output(raw).expect("parses");
        let labels = derive_labels(&snap.units, None);
        assert_eq!(
            labels,
            vec!["self-hosted".to_string(), "msi-wsl".to_string()]
        );
    }

    #[test]
    fn labels_deduplicate_hostname_and_agent_name() {
        let raw = "UNIT\tactions.runner.org-repo.msi-wsl.service\tactive\t/x\nHOSTNAME\tmsi-wsl\nBUSY\t0\nPROBE_END\t1";
        let snap = parse_probe_output(raw).expect("parses");
        let labels = derive_labels(&snap.units, snap.hostname.as_deref());
        assert_eq!(
            labels,
            vec!["self-hosted".to_string(), "msi-wsl".to_string()]
        );
    }

    #[test]
    fn machine_name_parses_from_unit_names() {
        assert_eq!(
            machine_name_from_unit("actions.runner.qontinui-qontinui-coord.msi-wsl.service")
                .as_deref(),
            Some("msi-wsl")
        );
        assert_eq!(
            machine_name_from_unit("actions.runner.org-repo.spaceship-wsl").as_deref(),
            Some("spaceship-wsl")
        );
        assert_eq!(machine_name_from_unit("nginx.service"), None);
        assert_eq!(machine_name_from_unit("actions.runner.onlyone"), None);
    }

    #[test]
    fn agent_name_extraction_tolerates_junk() {
        assert_eq!(
            agent_name_from_runner_file(r#"{"agentName":"msi-wsl"}"#).as_deref(),
            Some("msi-wsl")
        );
        assert_eq!(agent_name_from_runner_file("not json"), None);
        assert_eq!(agent_name_from_runner_file(r#"{"agentName":""}"#), None);
        assert_eq!(agent_name_from_runner_file("{}"), None);
    }

    // -- ProbeFailed vs Offline vs DistroDown -------------------------------

    fn distro_down() -> Result<(), WslUnavailable> {
        Err(WslUnavailable::DistroDown {
            distro: Some("Ubuntu-24.04".to_string()),
            running: vec![],
        })
    }

    #[test]
    fn distro_down_maps_to_distro_down_not_offline() {
        let state = probe_ci_runners_with(true, distro_down, || {
            panic!("no `wsl -e` may be spawned when the distro is down")
        });
        assert_eq!(state.status, CiRunnerStatus::DistroDown);
        // A failure to look is not evidence of absence.
        assert!(state.installed);
    }

    #[test]
    fn gate_failure_maps_to_probe_failed_not_offline() {
        let state = probe_ci_runners_with(
            true,
            || Err(WslUnavailable::GateFailed("no wsl.exe".to_string())),
            || panic!("no `wsl -e` may be spawned when the gate could not be evaluated"),
        );
        assert_eq!(state.status, CiRunnerStatus::ProbeFailed);
        assert!(state.installed);
    }

    #[test]
    fn script_failure_maps_to_probe_failed_not_offline() {
        let state = probe_ci_runners_with(true, || Ok(()), || Err("wsl exited 1".to_string()));
        assert_eq!(state.status, CiRunnerStatus::ProbeFailed);
        assert!(state.installed);
    }

    #[test]
    fn unparseable_output_maps_to_probe_failed_not_offline() {
        let state = probe_ci_runners_with(true, || Ok(()), || Ok("garbage".to_string()));
        assert_eq!(state.status, CiRunnerStatus::ProbeFailed);
    }

    #[test]
    fn distro_up_with_no_active_unit_is_genuinely_offline() {
        let state = probe_ci_runners_with(
            false,
            || Ok(()),
            || {
                Ok("UNIT\tactions.runner.a.b.service\tinactive\t/x\nBUSY\t0\nINSTALLED_FALLBACK\t0\nPROBE_END\t1".to_string())
            },
        );
        assert_eq!(state.status, CiRunnerStatus::Offline);
        // A discovered unit is itself proof of an install.
        assert!(state.installed);
        assert_eq!(state.active_service_names.len(), 0);
        assert_eq!(state.service_names.len(), 1);
    }

    #[test]
    fn installed_fallback_rides_the_same_single_spawn() {
        let state = probe_ci_runners_with(
            false,
            || Ok(()),
            || Ok("BUSY\t0\nINSTALLED_FALLBACK\t1\nPROBE_END\t1".to_string()),
        );
        assert_eq!(state.status, CiRunnerStatus::Offline);
        assert!(state.installed);
    }

    // -- spawn count (Verification §3) --------------------------------------

    #[test]
    fn one_tick_spawns_at_most_one_wsl_exec_when_the_distro_is_up() {
        // `gated_spawn_count` is process-wide; hold the shared test lock so a
        // peer test's increment cannot land inside our delta window.
        let _serialize = crate::wsl_util::test_lock();
        let spawns = Cell::new(0u32);
        let gated_before = crate::wsl_util::gated_spawn_count();
        let state = probe_ci_runners_with(
            false,
            || Ok(()),
            || {
                spawns.set(spawns.get() + 1);
                Ok(sample_output())
            },
        );
        assert_eq!(
            spawns.get(),
            1,
            "a tick must collapse to ONE `wsl -e` spawn"
        );
        assert_eq!(state.status, CiRunnerStatus::Idle);
        // Nothing else in the crate may reach for `wsl` behind our back — this
        // is the assertion that catches the cross-module D8 fallback.
        assert_eq!(crate::wsl_util::gated_spawn_count(), gated_before);
    }

    #[test]
    fn one_tick_spawns_zero_wsl_execs_when_the_distro_is_down() {
        let _serialize = crate::wsl_util::test_lock();
        let spawns = Cell::new(0u32);
        let gated_before = crate::wsl_util::gated_spawn_count();
        let state = probe_ci_runners_with(false, distro_down, || {
            spawns.set(spawns.get() + 1);
            Ok(sample_output())
        });
        assert_eq!(
            spawns.get(),
            0,
            "a distro-down tick must issue NO `wsl -e` command at all"
        );
        assert_eq!(state.status, CiRunnerStatus::DistroDown);
        // On the pre-fix code this was 2: `systemctl list-units` plus the
        // cross-module `is_runner_installed` filesystem fallback (D8).
        assert_eq!(crate::wsl_util::gated_spawn_count(), gated_before);
    }

    // -- restart arm (§4 / D5) ----------------------------------------------

    const SVC: &str = "actions.runner.org-repo.host.service";

    #[test]
    fn restart_fires_only_when_distro_is_up_and_unit_is_inactive() {
        assert_eq!(
            decide_restart(SVC, &CiRunnerStatus::Offline, true, &[SVC.to_string()], &[]),
            RestartDecision::Restart
        );
    }

    #[test]
    fn restart_never_fires_on_distro_down() {
        // The pre-fix arm would have issued `wsl -e sudo systemctl restart`,
        // which BOOTS the distro and reports success — masking a host fault.
        assert!(matches!(
            decide_restart(
                SVC,
                &CiRunnerStatus::DistroDown,
                true,
                &[SVC.to_string()],
                &[]
            ),
            RestartDecision::NoEvidence(_)
        ));
        // …and not even when the distro-down reading left the lists empty.
        assert!(matches!(
            decide_restart(SVC, &CiRunnerStatus::DistroDown, true, &[], &[]),
            RestartDecision::NoEvidence(_)
        ));
    }

    #[test]
    fn restart_never_fires_on_probe_failure() {
        assert!(matches!(
            decide_restart(SVC, &CiRunnerStatus::ProbeFailed, true, &[], &[]),
            RestartDecision::NoEvidence(_)
        ));
    }

    #[test]
    fn restart_waits_a_tick_after_the_distro_comes_back() {
        // Previous tick had no service evidence: the unit may simply not have
        // finished starting yet.
        assert!(matches!(
            decide_restart(
                SVC,
                &CiRunnerStatus::Offline,
                false,
                &[SVC.to_string()],
                &[]
            ),
            RestartDecision::NoEvidence(_)
        ));
    }

    #[test]
    fn active_unit_is_left_alone() {
        assert_eq!(
            decide_restart(
                SVC,
                &CiRunnerStatus::Idle,
                true,
                &[SVC.to_string()],
                &[SVC.to_string()]
            ),
            RestartDecision::StillActive
        );
    }

    #[test]
    fn removed_unit_is_not_a_crash() {
        assert_eq!(
            decide_restart(SVC, &CiRunnerStatus::Offline, true, &[], &[]),
            RestartDecision::UnitGone
        );
    }

    // -- keepalive reporting ------------------------------------------------

    #[test]
    fn keepalive_classification() {
        assert_eq!(classify_keepalive(false, false), KeepalivePresence::Absent);
        assert_eq!(classify_keepalive(false, true), KeepalivePresence::Absent);
        assert_eq!(
            classify_keepalive(true, true),
            KeepalivePresence::DisabledByFlag
        );
        assert_eq!(classify_keepalive(true, false), KeepalivePresence::Present);
    }
}

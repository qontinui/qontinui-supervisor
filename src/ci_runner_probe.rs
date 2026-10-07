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

/// Last-resort location of the host-level WSL keepalive script, used only when
/// neither the env override nor the scheduled task names one. The script is
/// installed from the claude-config checkout, so this legacy path is normally
/// stale — the scheduled task's registered action is the source of truth.
const DEFAULT_KEEPALIVE_SCRIPT: &str = r"C:\claude\scripts\wsl-keepalive.ps1";

/// Name of the Task Scheduler task that runs the keepalive
/// (`install-wsl-keepalive.ps1`). Its action carries the script's real path.
const KEEPALIVE_TASK_NAME: &str = "QontinuiWslKeepalive";

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
    /// `active` in **this** tick's collapsed script.
    pub active_service_names: Vec<String>,
    /// The subset of `service_names` **confirmed** not running (`inactive`,
    /// `failed`, `dead`). This is the restart arm's only trigger.
    ///
    /// It is a separate list rather than "everything not in
    /// `active_service_names`" on purpose: a unit that is `activating`, or
    /// whose `is-active` word never arrived, belongs to neither list, and
    /// restarting it would be acting on an absence of evidence.
    pub inactive_service_names: Vec<String>,
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
            inactive_service_names: Vec::new(),
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
/// `WorkingDirectory` (falling back to the directory of `ExecStart`'s FIRST
/// path -- a unit with two `ExecStart=` lines otherwise produced a two-line
/// `dirname` result that split one record across two output lines). That
/// working directory is what removes the hardcoded `~/actions-runner`
/// assumption from label derivation (D6) instead of replacing it with a better
/// guess. `--all` is load-bearing: without it `list-units` hides a *stopped*
/// unit entirely, which is exactly the state the restart arm exists to detect.
///
/// Two details are load-bearing and were each measured wrong first:
///
/// - **`pgrep -f '[R]unner\.Worker'`, never `'Runner.Worker'`.** The pattern is
///   a literal inside this script, so it is in the wrapper `bash`'s own
///   `/proc/<pid>/cmdline`; `pgrep` excludes only itself, never its parent. The
///   plain spelling therefore matched the probe and reported `BUSY=1` on an
///   idle host forever, so `Idle` was unreachable. The bracket makes the
///   pattern not match its own text.
/// - **`sudo -n` fallbacks for the `.runner` reads.** The probe runs as the
///   default WSL user, and a runner installed under its own account lives in a
///   `0750` home (MSI: `/home/runner/actions-runner-<repo>/`). The unprivileged
///   glob does not even expand there, so both the label source and the
///   installed fallback silently answered "nothing". `-n` never prompts, so a
///   host without passwordless sudo degrades to the unprivileged answer.
const PROBE_SCRIPT: &str = r#"units=$(systemctl list-units --type=service --plain --no-legend --all 'actions.runner.*' 2>/dev/null | awk '{print $1}'); for u in $units; do case "$u" in actions.runner.*) ;; *) continue ;; esac; st=$(systemctl is-active "$u" 2>/dev/null || true); wd=$(systemctl show -p WorkingDirectory --value "$u" 2>/dev/null || true); if [ -z "$wd" ]; then p=$(systemctl show -p ExecStart --value "$u" 2>/dev/null | sed -n 's/.*path=\([^ ;]*\).*/\1/p' | head -1); if [ -n "$p" ]; then wd=$(dirname "$p"); fi; fi; printf 'UNIT\t%s\t%s\t%s\n' "$u" "$st" "$wd"; if [ -n "$wd" ]; then rf=""; if [ -r "$wd/.runner" ]; then rf=$(tr -d '\n\r\t' < "$wd/.runner"); else rf=$(sudo -n cat "$wd/.runner" 2>/dev/null | tr -d '\n\r\t'); fi; if [ -n "$rf" ]; then printf 'RUNNERFILE\t%s\t%s\n' "$u" "$rf"; fi; fi; done; if pgrep -f '[R]unner\.Worker' >/dev/null 2>&1; then printf 'BUSY\t1\n'; else printf 'BUSY\t0\n'; fi; printf 'HOSTNAME\t%s\n' "$(hostname 2>/dev/null || cat /proc/sys/kernel/hostname 2>/dev/null || true)"; fb=0; for f in "$HOME"/actions-runner*/.runner /home/*/actions-runner*/.runner /root/actions-runner*/.runner; do if [ -f "$f" ]; then fb=1; break; fi; done; if [ "$fb" = 0 ]; then if sudo -n sh -c 'for f in /home/*/actions-runner*/.runner /root/actions-runner*/.runner; do if [ -f "$f" ]; then exit 0; fi; done; exit 1' >/dev/null 2>&1; then fb=1; fi; fi; printf 'INSTALLED_FALLBACK\t%s\n' "$fb"; printf 'PROBE_END\t1\n'"#;

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

/// What a `systemctl is-active` word actually tells us about a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitActivity {
    /// Confirmed running.
    Active,
    /// Confirmed not running -- the only state the restart arm may act on.
    Inactive,
    /// Mid-transition, or no word at all. NOT evidence of anything.
    Unknown,
}

/// Classify a `systemctl is-active` word.
///
/// The empty string is deliberately `Unknown`: `is-active` writes its errors to
/// the stderr this script discards, so a bus timeout mid-tick yields an empty
/// field. Reading that as `Inactive` would restart a healthy unit on an absence
/// of evidence -- the exact thing the honest-status model forbids. The
/// transitional words are `Unknown` for a narrower reason: a unit that is
/// already `activating` does not need a second `systemctl restart` on top.
pub fn classify_active_state(word: &str) -> UnitActivity {
    match word.trim() {
        "active" => UnitActivity::Active,
        "inactive" | "failed" | "dead" => UnitActivity::Inactive,
        _ => UnitActivity::Unknown,
    }
}

impl UnitObservation {
    pub fn activity(&self) -> UnitActivity {
        classify_active_state(&self.active_state)
    }

    pub fn is_active(&self) -> bool {
        self.activity() == UnitActivity::Active
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
///
/// The BOM strip is not defensive padding: `config.sh` writes `.runner` as
/// UTF-8 **with** a BOM (confirmed on MSI, both runners), and `serde_json`
/// rejects a leading `U+FEFF` outright. Without this the parse always failed
/// and every label silently fell back to the unit-name derivation -- the same
/// silent degradation, one layer in, that the hardcoded path caused.
fn agent_name_from_runner_file(json: &str) -> Option<String> {
    let json = json.trim_start_matches('\u{feff}').trim();
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
        .filter(|u| u.activity() == UnitActivity::Active)
        .map(|u| u.name.clone())
        .collect();
    let inactive_service_names: Vec<String> = snapshot
        .units
        .iter()
        .filter(|u| u.activity() == UnitActivity::Inactive)
        .map(|u| u.name.clone())
        .collect();

    CiRunnerState {
        status: derive_status(&snapshot),
        labels: derive_labels(&snapshot.units, snapshot.hostname.as_deref()),
        installed: derive_installed(&service_names, || snapshot.installed_fallback),
        service_names,
        active_service_names,
        inactive_service_names,
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
    inactive_service_names: &[String],
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
    if inactive_service_names.iter().any(|s| s == service) {
        return RestartDecision::Restart;
    }
    if service_names.iter().any(|s| s == service) {
        // Discovered, but neither confirmed active nor confirmed inactive:
        // mid-transition, or `is-active` produced no word. Not evidence.
        RestartDecision::NoEvidence("unit activity could not be established")
    } else {
        RestartDecision::UnitGone
    }
}

/// The set of services the restart arm keeps watching into the next tick.
///
/// `service_names ∩ (previously_watched ∪ active)`. Keeping the *previously
/// watched* half is what stops a unit being forgotten: a baseline of "whatever
/// is active right now" drops a unit the moment it goes inactive, so any tick
/// the arm could not act on (a distro-down blip, the one-tick deferral after
/// the distro returns, a rate-limited or failed restart) silently retired the
/// unit and it was never restarted again. Intersecting with `service_names`
/// still retires a unit that is genuinely gone.
pub fn next_watch_baseline(
    previously_watched: &[String],
    service_names: &[String],
    active_service_names: &[String],
) -> Vec<String> {
    let mut watched: Vec<String> = previously_watched
        .iter()
        .chain(active_service_names.iter())
        .filter(|s| service_names.contains(s))
        .cloned()
        .collect();
    watched.sort();
    watched.dedup();
    watched
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
    /// The script's location could not be established, so presence is unknown
    /// rather than absent.
    Unknown,
}

impl KeepalivePresence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Present => "keepalive script present (not proof it is running)",
            Self::DisabledByFlag => "keepalive script present but DISABLED by its flag file",
            Self::Absent => "NO keepalive script found",
            Self::Unknown => "keepalive script location UNKNOWN (scheduled task could not be read)",
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

/// Where the resolved keepalive script path came from. Logged so a "missing"
/// verdict against the legacy default is never mistaken for a real finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveSource {
    /// `QONTINUI_WSL_KEEPALIVE_SCRIPT`.
    Env,
    /// The registered action of the `QontinuiWslKeepalive` scheduled task.
    Task,
    /// The task is absent from this account's task list; the legacy default,
    /// which is normally stale.
    Default,
}

impl KeepaliveSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Env => "env override",
            Self::Task => "scheduled task",
            Self::Default => "legacy default (task not registered, or not visible to this account)",
        }
    }
}

/// What asking Task Scheduler produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskLookup {
    /// The task list was read and holds no task of that name.
    NotRegistered,
    /// The task's action names this script, and optionally a `-DisableFlag`.
    Task {
        script: PathBuf,
        disable_flag: Option<PathBuf>,
    },
    /// The question could not be answered: `schtasks` would not spawn, failed,
    /// timed out, or its output named no usable `-File` (an action with
    /// `-Command`, or a path that is not decodable). The location is UNKNOWN.
    Unreadable,
}

/// A resolved keepalive script, or the admission that it cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedScript {
    Known {
        script: PathBuf,
        /// The task's own `-DisableFlag`, when it carries one.
        task_disable_flag: Option<PathBuf>,
        source: KeepaliveSource,
    },
    Unresolved,
}

/// Split a command line into tokens. Only `"` quotes: single quotes are literal
/// under the Windows argv rules, and a path such as `C:\Users\O'Brien\k.ps1`
/// must not open a quote at its apostrophe.
fn split_command_line(args: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut in_token = false;
    for c in args.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                in_token = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

/// Decode the XML entities `schtasks` may emit, `&amp;` last so an escaped
/// ampersand is never decoded twice.
fn decode_xml_entities(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Pure: the value following `switch` in a scheduled task's `<Arguments>`.
/// Only `<Arguments>` elements are read, in order, and the first one carrying a
/// usable value wins. The switch matches case-insensitively on a whole token,
/// and a value that is itself a switch (`-File -NoProfile`) is rejected.
pub fn task_switch_value(xml: &str, switch: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(open) = rest.find("<Arguments>") {
        let body = &rest[open + "<Arguments>".len()..];
        let close = body.find("</Arguments>")?;
        let tokens = split_command_line(&decode_xml_entities(&body[..close]));
        if let Some(i) = tokens.iter().position(|t| t.eq_ignore_ascii_case(switch)) {
            if let Some(value) = tokens.get(i + 1) {
                if !value.is_empty() && !value.starts_with('-') {
                    return Some(value.clone());
                }
            }
        }
        rest = &body[close + "</Arguments>".len()..];
    }
    None
}

/// Decode `schtasks` output. It is single-byte (the OEM code page) when piped,
/// but a UTF-16 stream (BOM, or interleaved NULs) is decoded as such rather
/// than silently failing to match anything.
pub fn decode_schtasks_output(bytes: &[u8]) -> String {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.contains(&0);
    if !utf16 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let body = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Pure: find the full name (folder included, e.g. `\Fleet\QontinuiWslKeepalive`)
/// of the task called `name` in `schtasks /Query /FO CSV /NH` output. Matching a
/// listing rather than trusting a bare `/TN` means a task outside the root
/// folder is found, and "not registered" never has to be inferred from an exit
/// code or from locale-dependent error text.
pub fn find_task_in_listing(listing: &str, name: &str) -> Option<String> {
    let suffix = format!("\\{name}");
    let mut nested = None;
    for line in listing.lines() {
        let Some(first) = line.trim().trim_start_matches('\u{FEFF}').strip_prefix('"') else {
            continue;
        };
        let Some(end) = first.find('"') else { continue };
        let field = &first[..end];
        if field == suffix {
            return Some(field.to_string());
        }
        if nested.is_none() && field.ends_with(&suffix) {
            nested = Some(field.to_string());
        }
    }
    nested
}

/// Pure: whether `p` is an absolute, literal Windows path (`C:\...`, `C:/...`
/// or UNC) with no `%VAR%` for Task Scheduler to expand at run time. Spelled out
/// rather than `Path::is_absolute` so it answers the same on every host.
fn is_literal_absolute_windows_path(p: &str) -> bool {
    let b = p.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    (drive || p.starts_with("\\\\")) && !p.contains('%')
}

/// Pure: map a task's XML definition to a [`TaskLookup`].
pub fn lookup_from_task_xml_bytes(bytes: &[u8]) -> TaskLookup {
    let xml = decode_schtasks_output(bytes);
    let undecodable = |s: &str| s.contains('\u{FFFD}');
    let Some(script) = task_switch_value(&xml, "-File") else {
        return TaskLookup::Unreadable;
    };
    let flag = task_switch_value(&xml, "-DisableFlag");
    // U+FFFD means a non-ASCII byte could not be decoded: location UNKNOWN.
    if undecodable(&script) || flag.as_deref().is_some_and(undecodable) {
        return TaskLookup::Unreadable;
    }
    // A relative or `%VAR%` path would be tested as written and read as absent.
    let literal = |p: &str| is_literal_absolute_windows_path(p);
    if !literal(&script) || flag.as_deref().is_some_and(|f| !literal(f)) {
        return TaskLookup::Unreadable;
    }
    TaskLookup::Task {
        script: PathBuf::from(script),
        disable_flag: flag.map(PathBuf::from),
    }
}

/// Pure precedence: explicit env override, then the scheduled task, then the
/// legacy default. The task lookup runs only when no override is set. Only a
/// task that is genuinely not registered falls back to the default — a lookup
/// that could not be answered is `Unresolved`, never silently "absent".
pub fn resolve_keepalive_script(
    env_override: Option<String>,
    lookup_task: impl FnOnce() -> TaskLookup,
) -> ResolvedScript {
    if let Some(v) = env_override.filter(|v| !v.trim().is_empty()) {
        return ResolvedScript::Known {
            script: PathBuf::from(v),
            task_disable_flag: None,
            source: KeepaliveSource::Env,
        };
    }
    match lookup_task() {
        TaskLookup::Task {
            script,
            disable_flag,
        } => ResolvedScript::Known {
            script,
            task_disable_flag: disable_flag,
            source: KeepaliveSource::Task,
        },
        TaskLookup::Unreadable => ResolvedScript::Unresolved,
        TaskLookup::NotRegistered => ResolvedScript::Known {
            script: PathBuf::from(DEFAULT_KEEPALIVE_SCRIPT),
            task_disable_flag: None,
            source: KeepaliveSource::Default,
        },
    }
}

/// Pure: where the disable flag lives. Env override, then the task's own
/// `-DisableFlag`, then the legacy default.
pub fn resolve_disable_flag(env_override: Option<String>, task_flag: Option<PathBuf>) -> PathBuf {
    env_override
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or(task_flag)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_KEEPALIVE_DISABLE_FLAG))
}

/// How long to wait for one `schtasks` call before giving up (a stalled Task
/// Scheduler service must not hang the probe).
const SCHTASKS_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `cmd` to completion within `timeout`, returning whether it exited
/// successfully and everything it wrote to stdout. stdout is drained on its own
/// thread, so output larger than the pipe buffer cannot stall the child into a
/// false timeout. `None` means it would not spawn or did not finish in time.
fn run_bounded(cmd: &mut std::process::Command, timeout: Duration) -> Option<(bool, Vec<u8>)> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                // Killing closes the pipe, which ends the reader thread.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    Some((status.success(), bytes))
}

/// The two-call lookup, with the process runner injected so every failure arm
/// is testable: list the tasks and find ours by name (so "not registered" is a
/// fact about the list, not an inference from an exit code), then read its XML.
fn lookup_task_via(mut run: impl FnMut(&[&str]) -> Option<(bool, Vec<u8>)>) -> TaskLookup {
    let Some((true, listing)) = run(&["/Query", "/FO", "CSV", "/NH"]) else {
        return TaskLookup::Unreadable;
    };
    let Some(full_name) =
        find_task_in_listing(&decode_schtasks_output(&listing), KEEPALIVE_TASK_NAME)
    else {
        return TaskLookup::NotRegistered;
    };
    match run(&["/Query", "/TN", &full_name, "/XML"]) {
        Some((true, bytes)) => lookup_from_task_xml_bytes(&bytes),
        _ => TaskLookup::Unreadable,
    }
}

/// `schtasks.exe` from System32, not a bare-name search that includes the
/// current directory; falls back to the bare name when `SystemRoot` is unset.
fn schtasks_program() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("schtasks.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("schtasks"))
}

/// Ask Task Scheduler where the keepalive script really lives.
fn query_keepalive_task() -> TaskLookup {
    lookup_task_via(|args| {
        let mut cmd = std::process::Command::new(schtasks_program());
        cmd.args(args);
        run_bounded(&mut cmd, SCHTASKS_TIMEOUT)
    })
}

/// Observe the keepalive: resolve the script (bounded `schtasks` calls when no
/// env override is set), then test it and the disable flag with `exists()`.
/// Blocking — call through `spawn_blocking`. Only done on a `DistroDown` tick.
fn observe_keepalive() -> (KeepalivePresence, PathBuf, Option<KeepaliveSource>) {
    match resolve_keepalive_script(
        std::env::var(KEEPALIVE_SCRIPT_ENV).ok(),
        query_keepalive_task,
    ) {
        ResolvedScript::Known {
            script,
            task_disable_flag,
            source,
        } => {
            let flag = resolve_disable_flag(
                std::env::var(KEEPALIVE_DISABLE_FLAG_ENV).ok(),
                task_disable_flag,
            );
            (
                classify_keepalive(script.exists(), flag.exists()),
                script,
                Some(source),
            )
        }
        ResolvedScript::Unresolved => (KeepalivePresence::Unknown, PathBuf::new(), None),
    }
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
                let (keepalive, script_path, source) =
                    match tokio::task::spawn_blocking(observe_keepalive).await {
                        Ok(obs) => obs,
                        Err(e) => {
                            warn!("ci_runner_probe: keepalive observation panicked: {e}");
                            (KeepalivePresence::Unknown, PathBuf::new(), None)
                        }
                    };
                let source = source.map_or("unresolved", |s| s.as_str());
                warn!(
                    "ci_runner_probe: WSL distro is NOT running ({} consecutive tick(s)). \
                     CI runners cannot be online, and this is a HOST-level fault — no \
                     `systemctl restart` will be attempted and the restart budget is \
                     untouched, because restarting through WSL would merely boot the \
                     distro and mask the cause. Distro liveness is owned by the \
                     keepalive, not by this probe: {} (looked at {}, from {}).",
                    distro_down_ticks,
                    keepalive.as_str(),
                    script_path.display(),
                    source
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
                &new_state.inactive_service_names,
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
            previously_online = next_watch_baseline(
                &previously_online,
                &new_state.service_names,
                &new_state.active_service_names,
            );
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
        // `deliberate_wakes_total` counts operator lifecycle actions that were
        // ALLOWED to start the distro — it should never move on its own, so a
        // tick where it advanced explains an otherwise mysterious distro boot.
        tracing::debug!(
            "ci_runner_probe: tick complete, status={}, wsl_exec_spawns_total={},              gate_reads_total={}, deliberate_wakes_total={}",
            state.ci_runner_state.read().await.status.as_str(),
            crate::wsl_util::gated_spawn_count(),
            crate::wsl_util::gate_spawn_count(),
            crate::wsl_util::waking_spawn_count()
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
        assert!(state.inactive_service_names.is_empty());
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

    #[test]
    fn agent_name_extraction_handles_the_bom_config_sh_actually_writes() {
        // Verbatim shape of MSI's `.runner` after the probe's newline strip:
        // a UTF-8 BOM, then the flattened JSON. serde_json rejects the BOM.
        let real = "\u{feff}{  \"agentId\": 22,  \"agentName\": \"msi-wsl\",  \"poolId\": 1}";
        assert_eq!(
            agent_name_from_runner_file(real).as_deref(),
            Some("msi-wsl")
        );
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
        assert_eq!(state.inactive_service_names.len(), 1);
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
            decide_restart(
                SVC,
                &CiRunnerStatus::Offline,
                true,
                &[SVC.to_string()],
                &[],
                &[SVC.to_string()],
            ),
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
                &[],
                &[SVC.to_string()],
            ),
            RestartDecision::NoEvidence(_)
        ));
        // …and not even when the distro-down reading left the lists empty.
        assert!(matches!(
            decide_restart(SVC, &CiRunnerStatus::DistroDown, true, &[], &[], &[]),
            RestartDecision::NoEvidence(_)
        ));
    }

    #[test]
    fn restart_never_fires_on_probe_failure() {
        assert!(matches!(
            decide_restart(SVC, &CiRunnerStatus::ProbeFailed, true, &[], &[], &[]),
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
                &[],
                &[SVC.to_string()],
            ),
            RestartDecision::NoEvidence(_)
        ));
    }

    #[test]
    fn restart_never_fires_on_an_unestablished_activity() {
        // Discovered, but `is-active` said `activating` — or said nothing at
        // all, because its stderr is discarded. Restarting here would act on an
        // absence of evidence, and would stack a restart on a unit already
        // coming up.
        assert!(matches!(
            decide_restart(
                SVC,
                &CiRunnerStatus::Offline,
                true,
                &[SVC.to_string()],
                &[],
                &[],
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
                &[SVC.to_string()],
                &[],
            ),
            RestartDecision::StillActive
        );
    }

    #[test]
    fn removed_unit_is_not_a_crash() {
        assert_eq!(
            decide_restart(SVC, &CiRunnerStatus::Offline, true, &[], &[], &[]),
            RestartDecision::UnitGone
        );
    }

    // -- the watch baseline -------------------------------------------------

    #[test]
    fn baseline_keeps_a_unit_that_went_inactive() {
        // The defect this closes: a baseline of "whatever is active now" drops
        // the unit on the very tick it goes down, so the next tick no longer
        // considers it and it is NEVER restarted.
        let watched = next_watch_baseline(&[SVC.to_string()], &[SVC.to_string()], &[]);
        assert_eq!(watched, vec![SVC.to_string()]);
    }

    #[test]
    fn baseline_survives_a_distro_down_blip_and_the_deferral_tick() {
        let svc = SVC.to_string();
        // Tick N: active.
        let mut watched =
            next_watch_baseline(&[], std::slice::from_ref(&svc), std::slice::from_ref(&svc));
        assert_eq!(watched, vec![svc.clone()]);
        // Tick N+1 is DistroDown: the loop leaves the baseline alone entirely.
        // Tick N+2: distro back, unit inactive, restart deferred one tick.
        watched = next_watch_baseline(&watched, std::slice::from_ref(&svc), &[]);
        assert_eq!(
            watched,
            vec![svc.clone()],
            "a unit must still be watched after the one-tick deferral, or the \
             restart arm can never fire for it again"
        );
        // Tick N+3: still inactive — and still watched, so the arm can fire.
        watched = next_watch_baseline(&watched, std::slice::from_ref(&svc), &[]);
        assert_eq!(watched, vec![svc]);
    }

    #[test]
    fn baseline_retires_a_unit_that_is_genuinely_gone() {
        let watched = next_watch_baseline(&[SVC.to_string()], &[], &[]);
        assert!(watched.is_empty());
    }

    #[test]
    fn baseline_admits_newly_active_units_and_deduplicates() {
        let a = "actions.runner.org-repo.a.service".to_string();
        let b = "actions.runner.org-repo.b.service".to_string();
        let watched = next_watch_baseline(
            std::slice::from_ref(&a),
            &[a.clone(), b.clone()],
            &[a.clone(), b.clone()],
        );
        assert_eq!(watched, vec![a, b]);
    }

    // -- is-active classification -------------------------------------------

    #[test]
    fn active_state_classification() {
        assert_eq!(classify_active_state("active"), UnitActivity::Active);
        assert_eq!(classify_active_state("inactive"), UnitActivity::Inactive);
        assert_eq!(classify_active_state("failed"), UnitActivity::Inactive);
        assert_eq!(classify_active_state("dead"), UnitActivity::Inactive);
        // Mid-transition and missing words are NOT evidence.
        assert_eq!(classify_active_state("activating"), UnitActivity::Unknown);
        assert_eq!(classify_active_state("deactivating"), UnitActivity::Unknown);
        assert_eq!(classify_active_state("reloading"), UnitActivity::Unknown);
        assert_eq!(classify_active_state(""), UnitActivity::Unknown);
        assert_eq!(classify_active_state("  "), UnitActivity::Unknown);
    }

    #[test]
    fn a_missing_is_active_word_is_never_read_as_inactive() {
        // `is-active`'s stderr is discarded, so a bus timeout emits an empty
        // field. The unit must land in neither list.
        let state = probe_ci_runners_with(
            false,
            || Ok(()),
            || {
                Ok(
                    "UNIT\tactions.runner.a.b.service\t\t/x\nBUSY\t0\nINSTALLED_FALLBACK\t0\nPROBE_END\t1"
                        .to_string(),
                )
            },
        );
        assert_eq!(state.service_names.len(), 1);
        assert!(state.active_service_names.is_empty());
        assert!(
            state.inactive_service_names.is_empty(),
            "an empty is-active word must not read as confirmed-inactive"
        );
    }

    // -- the probe script itself --------------------------------------------

    #[test]
    fn probe_script_pgrep_pattern_cannot_match_its_own_text() {
        // The pattern is a literal inside the script, so it sits in the wrapper
        // bash's own /proc/<pid>/cmdline. `pgrep` excludes only itself, never
        // its parent — so the plain spelling matched the probe and pinned BUSY
        // to 1 forever (measured on MSI). The bracket breaks the self-match.
        assert!(
            PROBE_SCRIPT.contains("[R]unner"),
            "pgrep pattern must be bracket-escaped so it cannot match the script itself"
        );
        assert!(
            !PROBE_SCRIPT.contains("'Runner.Worker'"),
            "the self-matching pgrep spelling must not come back"
        );
    }

    #[test]
    fn probe_script_takes_only_the_first_exec_start_path() {
        // Two `ExecStart=` lines make `--value` emit two lines; without
        // `head -1` the `dirname` result is multi-line and splits one UNIT
        // record across two output lines.
        assert!(PROBE_SCRIPT.contains("| head -1"));
    }

    #[test]
    fn probe_script_retries_unreadable_runner_paths_without_prompting() {
        // A runner installed under its own 0750 home is invisible to the
        // default WSL user; `-n` guarantees sudo never blocks on a prompt.
        assert!(PROBE_SCRIPT.contains("sudo -n cat"));
        assert!(PROBE_SCRIPT.contains("sudo -n sh -c"));
        assert!(!PROBE_SCRIPT.contains("sudo cat"));
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

    /// The `-File <path>` script out of a task's XML, as production reads it.
    fn script_path_from_task_xml(xml: &str) -> Option<PathBuf> {
        task_switch_value(xml, "-File").map(PathBuf::from)
    }

    fn task_xml(arguments: &str) -> String {
        format!("<Exec><Command>powershell.exe</Command><Arguments>{arguments}</Arguments></Exec>")
    }

    const LIVE_SCRIPT: &str = r"C:\qontinui-root\qontinui-claude-config\scripts\wsl-keepalive.ps1";
    const TASK: &str = "QontinuiWslKeepalive";

    fn live_task() -> TaskLookup {
        TaskLookup::Task {
            script: PathBuf::from(LIVE_SCRIPT),
            disable_flag: None,
        }
    }

    #[test]
    fn script_path_parsed_from_task_xml() {
        let quoted = task_xml(&format!(
            "-NoProfile -File \"{LIVE_SCRIPT}\" -Distro \"Ubuntu-24.04\""
        ));
        assert_eq!(
            script_path_from_task_xml(&quoted),
            Some(PathBuf::from(LIVE_SCRIPT))
        );
        // The same definition with the quotes XML-escaped, three ways.
        for entity in ["&quot;", "&#34;", "&#x22;"] {
            let escaped = quoted.replace('"', entity);
            assert_eq!(
                script_path_from_task_xml(&escaped),
                Some(PathBuf::from(LIVE_SCRIPT)),
                "entity {entity}"
            );
        }
        // Bare path, and a path containing a space.
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-File C:\k\wsl-keepalive.ps1")),
            Some(PathBuf::from(r"C:\k\wsl-keepalive.ps1"))
        );
        assert_eq!(
            script_path_from_task_xml(&task_xml(r#"-File "C:\Program Files\k\a.ps1""#)),
            Some(PathBuf::from(r"C:\Program Files\k\a.ps1"))
        );
    }

    #[test]
    fn apostrophe_in_a_path_is_literal() {
        // Single quotes are literal under Windows argv rules: this must not
        // open a quote and swallow `-Distro` into the path.
        let xml = task_xml(r#"-File "C:\Users\O'Brien\wsl-keepalive.ps1" -Distro "Ubuntu-24.04""#);
        assert_eq!(
            script_path_from_task_xml(&xml),
            Some(PathBuf::from(r"C:\Users\O'Brien\wsl-keepalive.ps1"))
        );
        // The same path bare, with the apostrophe XML-escaped.
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-File C:\Users\O&apos;Brien\k.ps1 -X")),
            Some(PathBuf::from(r"C:\Users\O'Brien\k.ps1"))
        );
    }

    #[test]
    fn script_path_matches_file_switch_only_as_a_whole_token() {
        // PowerShell switches are case-insensitive.
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-file C:\k\a.ps1")),
            Some(PathBuf::from(r"C:\k\a.ps1"))
        );
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-FILE C:\k\a.ps1")),
            Some(PathBuf::from(r"C:\k\a.ps1"))
        );
        // A longer switch that merely starts with -File is not -File.
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-FileLogPath C:\x.log -File C:\k\a.ps1")),
            Some(PathBuf::from(r"C:\k\a.ps1"))
        );
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-Filename C:\x.ps1")),
            None
        );
        // First occurrence wins.
        assert_eq!(
            script_path_from_task_xml(&task_xml(r"-File C:\first.ps1 -File C:\second.ps1")),
            Some(PathBuf::from(r"C:\first.ps1"))
        );
    }

    #[test]
    fn script_path_absent_when_task_xml_names_none() {
        assert_eq!(script_path_from_task_xml(&task_xml("-NoProfile")), None);
        assert_eq!(script_path_from_task_xml(&task_xml("-File")), None);
        assert_eq!(
            script_path_from_task_xml(&task_xml("-File -NoProfile")),
            None
        );
        assert_eq!(script_path_from_task_xml(&task_xml(r#"-File """#)), None);
        assert_eq!(script_path_from_task_xml(""), None);
        // A -File outside the <Arguments> element is not an action argument.
        assert_eq!(
            script_path_from_task_xml(
                r"<Description>-File C:\nope.ps1</Description><Arguments>-NoProfile</Arguments>"
            ),
            None
        );
        // An unterminated <Arguments> element.
        assert_eq!(
            script_path_from_task_xml(r"<Arguments>-File C:\k\a.ps1"),
            None
        );
    }

    #[test]
    fn a_later_action_can_carry_the_script() {
        // The first <Exec> is some other action; the keepalive is the second.
        let xml = format!(
            "{}{}",
            task_xml("-NoProfile -Command Get-Date"),
            task_xml(&format!("-File \"{LIVE_SCRIPT}\""))
        );
        assert_eq!(
            script_path_from_task_xml(&xml),
            Some(PathBuf::from(LIVE_SCRIPT))
        );
    }

    #[test]
    fn schtasks_output_decoding() {
        // Single-byte output is decoded as text.
        assert_eq!(decode_schtasks_output(b"<a>x</a>"), "<a>x</a>");
        // UTF-16LE with a BOM, and without one, is decoded rather than missed.
        let utf16: Vec<u8> = "<a>x</a>"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_schtasks_output(&utf16), "<a>x</a>");
        let mut bom = vec![0xFF, 0xFE];
        bom.extend_from_slice(&utf16);
        assert_eq!(decode_schtasks_output(&bom), "<a>x</a>");
        // A dangling odd byte is dropped, never a panic.
        let mut odd = utf16.clone();
        odd.push(0x41);
        assert_eq!(decode_schtasks_output(&odd), "<a>x</a>");
    }

    #[test]
    fn xml_entities_are_not_decoded_twice() {
        // `&amp;quot;` is the text `&quot;`, not a quote character.
        assert_eq!(decode_xml_entities("&amp;quot;"), "&quot;");
        assert_eq!(decode_xml_entities("&quot;&amp;&lt;"), "\"&<");
    }

    #[test]
    fn task_listing_is_matched_by_name_wherever_the_task_lives() {
        let listing = concat!(
            "\"\\Qontinui-Subst-D-Drive\",\"N/A\",\"Ready\"\r\n",
            "\"\\QontinuiWslKeepalive\",\"At log on\",\"Running\"\r\n",
        );
        assert_eq!(
            find_task_in_listing(listing, TASK),
            Some("\\QontinuiWslKeepalive".to_string())
        );
        // A task in a subfolder is found, with its folder.
        let nested = "\"\\Fleet\\QontinuiWslKeepalive\",\"N/A\",\"Ready\"";
        assert_eq!(
            find_task_in_listing(nested, TASK),
            Some("\\Fleet\\QontinuiWslKeepalive".to_string())
        );
        // A different task whose name merely ends the same way is not ours.
        assert_eq!(
            find_task_in_listing("\"\\Not-QontinuiWslKeepalive\",\"N/A\",\"Ready\"", TASK),
            None
        );
        // Absent, empty and junk listings find nothing.
        assert_eq!(find_task_in_listing(listing, "Other"), None);
        assert_eq!(find_task_in_listing("", TASK), None);
        assert_eq!(find_task_in_listing("INFO: no tasks\r\n", TASK), None);
    }

    #[test]
    fn task_xml_maps_to_a_lookup() {
        let xml = task_xml(&format!("-File \"{LIVE_SCRIPT}\""));
        assert_eq!(lookup_from_task_xml_bytes(xml.as_bytes()), live_task());
        // The task's own -DisableFlag is carried through.
        let with_flag = task_xml(&format!(
            "-File \"{LIVE_SCRIPT}\" -DisableFlag \"D:\\off.flag\""
        ));
        assert_eq!(
            lookup_from_task_xml_bytes(with_flag.as_bytes()),
            TaskLookup::Task {
                script: PathBuf::from(LIVE_SCRIPT),
                disable_flag: Some(PathBuf::from(r"D:\off.flag")),
            }
        );
        // An action with no -File (e.g. -Command) is UNKNOWN.
        assert_eq!(
            lookup_from_task_xml_bytes(task_xml("-Command Get-Date").as_bytes()),
            TaskLookup::Unreadable
        );
        // So is a path or flag holding an undecodable byte (OEM 0x81 is `u-umlaut`).
        let bad_script = b"<Arguments>-File C:\\Users\\J\x81rg\\k.ps1</Arguments>";
        assert_eq!(
            lookup_from_task_xml_bytes(bad_script),
            TaskLookup::Unreadable
        );
        let bad_flag = b"<Arguments>-File C:\\k.ps1 -DisableFlag C:\\J\x81rg\\f</Arguments>";
        assert_eq!(lookup_from_task_xml_bytes(bad_flag), TaskLookup::Unreadable);
    }

    #[test]
    fn disable_flag_precedence() {
        let task_flag = Some(PathBuf::from(r"D:\off.flag"));
        assert_eq!(
            resolve_disable_flag(Some(r"E:\env.flag".into()), task_flag.clone()),
            PathBuf::from(r"E:\env.flag")
        );
        assert_eq!(
            resolve_disable_flag(Some("  ".into()), task_flag.clone()),
            PathBuf::from(r"D:\off.flag")
        );
        assert_eq!(
            resolve_disable_flag(None, task_flag),
            PathBuf::from(r"D:\off.flag")
        );
        assert_eq!(
            resolve_disable_flag(None, None),
            PathBuf::from(DEFAULT_KEEPALIVE_DISABLE_FLAG)
        );
    }

    #[test]
    fn keepalive_script_resolution_precedence() {
        use std::cell::Cell;

        // An env override wins and the task is never asked.
        let asked = Cell::new(false);
        let r = resolve_keepalive_script(Some(r"D:\x.ps1".into()), || {
            asked.set(true);
            live_task()
        });
        assert_eq!(
            r,
            ResolvedScript::Known {
                script: PathBuf::from(r"D:\x.ps1"),
                task_disable_flag: None,
                source: KeepaliveSource::Env,
            }
        );
        assert!(!asked.get(), "task lookup must not run under an override");

        // A blank override is no override; the task beats the legacy default.
        let from_task = ResolvedScript::Known {
            script: PathBuf::from(LIVE_SCRIPT),
            task_disable_flag: None,
            source: KeepaliveSource::Task,
        };
        assert_eq!(
            resolve_keepalive_script(Some("  ".into()), live_task),
            from_task
        );
        assert_eq!(resolve_keepalive_script(None, live_task), from_task);
        // Only a genuinely unregistered task falls back to the legacy default.
        assert_eq!(
            resolve_keepalive_script(None, || TaskLookup::NotRegistered),
            ResolvedScript::Known {
                script: PathBuf::from(DEFAULT_KEEPALIVE_SCRIPT),
                task_disable_flag: None,
                source: KeepaliveSource::Default,
            }
        );
        // A lookup that could not be answered is UNKNOWN, never the default.
        assert_eq!(
            resolve_keepalive_script(None, || TaskLookup::Unreadable),
            ResolvedScript::Unresolved
        );
    }

    #[test]
    fn run_bounded_reports_a_program_that_will_not_spawn() {
        let mut cmd = std::process::Command::new("qontinui-no-such-program-0f3a");
        assert_eq!(run_bounded(&mut cmd, Duration::from_secs(2)), None);
    }

    #[cfg(windows)]
    #[test]
    fn run_bounded_drains_output_larger_than_the_pipe_buffer() {
        // ~340 KB written before exit. If stdout were read only after exit, the
        // child would block on a full pipe and this would time out.
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "for /L %i in (1,1,20000) do @echo 0123456789abcdef"]);
        let (ok, bytes) = run_bounded(&mut cmd, Duration::from_secs(30)).expect("finished");
        assert!(ok);
        assert!(bytes.len() > 64 * 1024, "got {} bytes", bytes.len());
    }

    #[cfg(windows)]
    #[test]
    fn run_bounded_kills_a_child_that_overruns_the_timeout() {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        let started = Instant::now();
        assert_eq!(run_bounded(&mut cmd, Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(windows)]
    #[test]
    fn run_bounded_reports_a_nonzero_exit() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "exit 3"]);
        let (ok, _) = run_bounded(&mut cmd, Duration::from_secs(10)).expect("finished");
        assert!(!ok);
    }

    #[test]
    fn non_literal_script_paths_are_unknown_not_absent() {
        for bad in [
            r"%USERPROFILE%\k\wsl-keepalive.ps1",
            r"scripts\wsl-keepalive.ps1",
            r"wsl-keepalive.ps1",
            r"\rooted\no\drive.ps1",
        ] {
            let xml = task_xml(&format!("-File \"{bad}\""));
            assert_eq!(
                lookup_from_task_xml_bytes(xml.as_bytes()),
                TaskLookup::Unreadable,
                "{bad}"
            );
        }
        // Absolute forms are accepted: drive with either slash, and UNC.
        for good in [r"C:\k\a.ps1", "D:/k/a.ps1", r"\\host\share\a.ps1"] {
            let xml = task_xml(&format!("-File \"{good}\""));
            assert_eq!(
                lookup_from_task_xml_bytes(xml.as_bytes()),
                TaskLookup::Task {
                    script: PathBuf::from(good),
                    disable_flag: None
                },
                "{good}"
            );
        }
    }

    #[test]
    fn listing_match_survives_a_bom_and_prefers_the_root_task() {
        let bom = "\u{FEFF}\"\\QontinuiWslKeepalive\",\"N/A\",\"Ready\"";
        assert_eq!(
            find_task_in_listing(bom, TASK),
            Some("\\QontinuiWslKeepalive".to_string())
        );
        // The same name in two folders: the root task wins whatever the order.
        let clash = concat!(
            "\"\\Old\\QontinuiWslKeepalive\",\"N/A\",\"Ready\"\r\n",
            "\"\\QontinuiWslKeepalive\",\"N/A\",\"Ready\"\r\n",
        );
        assert_eq!(
            find_task_in_listing(clash, TASK),
            Some("\\QontinuiWslKeepalive".to_string())
        );
    }

    #[test]
    fn task_lookup_flow_maps_every_failure_arm() {
        let listing = b"\"\\QontinuiWslKeepalive\",\"At log on\",\"Ready\"\r\n".to_vec();
        let xml = task_xml(&format!("-File \"{LIVE_SCRIPT}\"")).into_bytes();

        // Happy path asks for the listing, then the task's XML by its full name.
        let mut calls: Vec<String> = Vec::new();
        let got = lookup_task_via(|args| {
            calls.push(args.join(" "));
            Some((
                true,
                if args.contains(&"CSV") {
                    listing.clone()
                } else {
                    xml.clone()
                },
            ))
        });
        assert_eq!(got, live_task());
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1].contains("/TN \\QontinuiWslKeepalive /XML"),
            "{calls:?}"
        );

        // A task missing from a listing that read fine is NotRegistered.
        assert_eq!(
            lookup_task_via(|_| Some((true, b"\"\\Other\",\"N/A\",\"Ready\"\r\n".to_vec()))),
            TaskLookup::NotRegistered
        );
        // A listing that cannot be read is UNKNOWN, never NotRegistered.
        assert_eq!(lookup_task_via(|_| None), TaskLookup::Unreadable);
        assert_eq!(
            lookup_task_via(|_| Some((false, Vec::new()))),
            TaskLookup::Unreadable
        );
        // A task that is listed but whose XML cannot be read is UNKNOWN.
        for xml_call in [None, Some((false, Vec::new()))] {
            let got = lookup_task_via(|args| {
                if args.contains(&"CSV") {
                    Some((true, listing.clone()))
                } else {
                    xml_call.clone()
                }
            });
            assert_eq!(got, TaskLookup::Unreadable);
        }
    }
}

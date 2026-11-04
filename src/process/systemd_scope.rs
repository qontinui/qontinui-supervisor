//! Linux spawn isolation for user-owned runners: a transient systemd scope.
//!
//! **Why this exists.** On Linux the supervisor is meant to live in a transient
//! systemd user service with `KillMode=mixed` (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 2). Inside such a unit every child of the supervisor dies with it —
//! which is the point for a TEMP runner (the Linux analogue of being assigned
//! to the Windows kill-on-close job) and a destructive accident for a
//! user-owned one (primary, named, external). Served policy `runner-lifecycle`
//! forbids stopping those, and the 2026-07-27 incident is what happens when a
//! containment mechanism reaps them. So on Linux a user-owned runner leaves
//! the supervisor's cgroup at spawn time, into its own transient scope:
//!
//! ```text
//! systemd-run --user --scope --quiet --collect --unit qontinui-runner-<id>-<utc-ts> -- <exe>
//! ```
//!
//! That mirrors Windows exactly: a temp runner stays in the container ("is
//! assigned to the job"), a user-owned runner is scoped out ("is never
//! assigned"). The ownership authority is the same single predicate,
//! [`crate::process::job::should_assign_to_ephemeral_job`] — an `is_temp()`
//! allowlist, so a new `RunnerKind` variant defaults to "scoped out, never
//! reaped".
//!
//! **Isolation is chosen at `Command` construction**, not by mutating a built
//! command: neither tokio's nor std's `Command` can change its program after
//! construction. [`isolation_command`] returns the `Command` (whose program is
//! either the runner exe or `systemd-run`) and the verdict; everything the
//! caller sets afterwards — env, cwd, stdio, process group — applies to
//! `systemd-run`, which passes it all through when it execs the runner in the
//! same process. So the pid `spawn()` returns is the runner's.
//!
//! **A scope start that does not land is a FAILED start, never an unscoped
//! retry.** `systemd-run --scope` has been measured hanging before it execs
//! (`qontinui-claude-config/scripts/lib/agent-build-scope.sh`, the 20 s
//! budget there), and it can exit without exec'ing. [`await_scope_landing`]
//! confirms the landing through a proc-root seam within a configurable bound
//! (default 20 s); on expiry it re-checks the pid's identity, kills a
//! still-pending `systemd-run`, and only then reports the failure — so a late
//! exec cannot start a runner the supervisor already reported as failed. A
//! silent unscoped re-spawn would put a user-owned runner back into the
//! reapable cgroup, which is the defect this module exists to prevent.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use qontinui_types::wire::runner_kind::RunnerKind;
use serde::Serialize;

/// The binary that moves a spawn into a transient scope.
pub const SYSTEMD_RUN: &str = "systemd-run";

/// Every scope unit this module creates starts with this. Phase 2's
/// `supervisor-verbs.sh status` lists `qontinui-runner-*.scope` units by it.
pub const UNIT_PREFIX: &str = "qontinui-runner-";

/// Env override for the scope-landing bound, in seconds (> 0).
pub const LANDING_TIMEOUT_ENV: &str = "QONTINUI_SUPERVISOR_SCOPE_LANDING_TIMEOUT_SECS";

/// Default bound on a `systemd-run --scope` landing — the prior art's measured
/// budget (`lib/agent-build-scope.sh`).
pub const DEFAULT_LANDING_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the landing check re-reads `/proc/<pid>/{exe,cgroup}`.
pub const LANDING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on the `systemctl --user show -p Version --value` probe — the same
/// `timeout 10` the shell prior art (`abs_decide`) uses.
pub const MANAGER_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the runner-id part of a unit name. systemd caps a unit name
/// at 255 bytes; the prefix, the timestamp and `.scope` take ~45.
const MAX_ID_IN_UNIT: usize = 120;

/// The isolation verdict for one runner spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolation {
    /// Spawned through `systemd-run --user --scope` into this unit. The string
    /// is the full unit name, `.scope` suffix included — what `systemctl` and
    /// `/proc/<pid>/cgroup` both name.
    Scoped(String),
    /// A user-owned runner that could NOT be scoped out, and why. It shares the
    /// supervisor's cgroup, so it dies with the supervisor's unit.
    Unscoped(String),
    /// A temp runner (it belongs in the supervisor's container), or a platform
    /// with no systemd scope mechanism (anything but Linux).
    NotApplicable,
}

impl Isolation {
    /// The scope unit, when the runner was scoped out.
    pub fn unit(&self) -> Option<&str> {
        match self {
            Isolation::Scoped(unit) => Some(unit),
            _ => None,
        }
    }

    /// The `/runners` and log rendering: `{mechanism, unit, reason}`.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Isolation::Scoped(unit) => serde_json::json!({
                "mechanism": "systemd-scope",
                "unit": unit,
                "reason": null,
            }),
            Isolation::Unscoped(reason) => serde_json::json!({
                "mechanism": "none",
                "unit": null,
                "reason": reason,
            }),
            Isolation::NotApplicable => serde_json::json!({
                "mechanism": "not_applicable",
                "unit": null,
                "reason": "temp runner (it belongs to the supervisor's lifetime) or a platform \
                           with no systemd scope",
            }),
        }
    }

    /// One human sentence for the per-spawn log line.
    pub fn describe(&self) -> String {
        match self {
            Isolation::Scoped(unit) => {
                format!("started in its own systemd scope {unit}, outside the supervisor's cgroup")
            }
            Isolation::Unscoped(reason) => format!(
                "UNSCOPED ({reason}) — it shares the supervisor's cgroup and dies with the \
                 supervisor's systemd unit, if the supervisor runs in one"
            ),
            Isolation::NotApplicable => "no spawn isolation applies".to_string(),
        }
    }
}

/// What the systemd preflight found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemdProbe {
    /// `systemd-run` resolved to this path and the user manager answered.
    Available { systemd_run: PathBuf },
    /// Scoping is not possible here, and why.
    Unavailable(String),
}

/// The `/health` `runner_isolation` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunnerIsolationReport {
    /// `"systemd-scope"` when every user-owned runner this supervisor spawns
    /// (and has spawned and still runs) leaves its cgroup; `"none"` otherwise.
    pub mechanism: &'static str,
    /// Why — always present, never blank.
    pub reason: String,
}

/// The pure spawn decision: which program, which leading args, which verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub isolation: Isolation,
}

/// `qontinui-runner-<id>-<utc-ts>` — the `--unit` argument (systemd appends
/// `.scope` for a `--scope` run). The id is reduced to `[A-Za-z0-9_-]` (any
/// other byte becomes `_`) and capped, so the name is always a valid unit
/// name; the timestamp carries milliseconds so a stop-then-start of the same
/// runner inside one second cannot collide with a scope still being collected.
pub fn scope_unit_base(runner_id: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let mut id: String = runner_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() {
        id.push('_');
    }
    id.truncate(MAX_ID_IN_UNIT);
    format!("{UNIT_PREFIX}{id}-{}", now.format("%Y%m%dT%H%M%S%3fZ"))
}

/// Decide how a runner of `kind` is spawned. Pure: every input the decision
/// reads is a parameter, so every arm is tested with literal argv.
///
/// - not Linux, or a temp runner → the exe itself, [`Isolation::NotApplicable`];
/// - user-owned and systemd [`SystemdProbe::Available`] → `systemd-run --user
///   --scope --quiet --collect --unit <base> -- <exe>`, [`Isolation::Scoped`];
/// - user-owned and systemd unavailable → the exe itself,
///   [`Isolation::Unscoped`] naming why.
pub fn plan_spawn(
    kind: &RunnerKind,
    runner_id: &str,
    exe: &Path,
    probe: &SystemdProbe,
    is_linux: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> SpawnPlan {
    let direct = |isolation| SpawnPlan {
        program: exe.to_path_buf(),
        args: Vec::new(),
        isolation,
    };
    if !is_linux || crate::process::job::should_assign_to_ephemeral_job(kind) {
        return direct(Isolation::NotApplicable);
    }
    match probe {
        SystemdProbe::Unavailable(why) => direct(Isolation::Unscoped(why.clone())),
        SystemdProbe::Available { systemd_run } => {
            let base = scope_unit_base(runner_id, now);
            let args = vec![
                OsString::from("--user"),
                OsString::from("--scope"),
                OsString::from("--quiet"),
                OsString::from("--collect"),
                OsString::from("--unit"),
                OsString::from(&base),
                OsString::from("--"),
                exe.as_os_str().to_os_string(),
            ];
            SpawnPlan {
                program: systemd_run.clone(),
                args,
                isolation: Isolation::Scoped(format!("{base}.scope")),
            }
        }
    }
}

/// Resolve `name` on `path_var` (a `PATH` value) to an executable file.
pub fn find_on_path(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Turn the manager probe's outcome into a verdict. Pure, so "the manager did
/// not answer" is tested without a hanging `systemctl`.
///
/// `systemctl_output` is `None` when the probe timed out, `Some(Err)` when it
/// could not run, and `Some(Ok((success, stdout)))` otherwise. Only a
/// successful run with a non-empty version is an answer.
pub fn probe_verdict(
    systemd_run: Option<PathBuf>,
    systemctl_found: bool,
    systemctl_output: Option<Result<(bool, String), String>>,
) -> SystemdProbe {
    let Some(systemd_run) = systemd_run else {
        return SystemdProbe::Unavailable(format!("{SYSTEMD_RUN} is not on PATH"));
    };
    if !systemctl_found {
        return SystemdProbe::Unavailable("systemctl is not on PATH".to_string());
    }
    match systemctl_output {
        None => SystemdProbe::Unavailable(format!(
            "the systemd user manager did not answer `systemctl --user show -p Version` within \
             {}s",
            MANAGER_PROBE_TIMEOUT.as_secs()
        )),
        Some(Err(e)) => {
            SystemdProbe::Unavailable(format!("`systemctl --user show` could not run: {e}"))
        }
        Some(Ok((false, _))) => SystemdProbe::Unavailable(
            "`systemctl --user show -p Version` failed — no reachable systemd user manager"
                .to_string(),
        ),
        Some(Ok((true, version))) if version.trim().is_empty() => SystemdProbe::Unavailable(
            "`systemctl --user show -p Version` answered with an empty version".to_string(),
        ),
        Some(Ok((true, _))) => SystemdProbe::Available { systemd_run },
    }
}

/// Run the systemd preflight: `systemd-run` on PATH and a bounded
/// `systemctl --user show -p Version --value` answering non-empty. Off Linux it
/// answers [`SystemdProbe::Unavailable`] without running anything.
pub async fn probe_systemd() -> SystemdProbe {
    if !cfg!(target_os = "linux") {
        return SystemdProbe::Unavailable("not Linux".to_string());
    }
    // A unit-test build never probes — and so never scopes into — the HOST's
    // systemd: a test that starts a stub runner must not create real transient
    // units on the box it runs on (state-neutral), nor behave differently on a
    // box with a user manager than on one without. The decision and the
    // landing check are tested directly, through their own seams.
    if cfg!(test) {
        return SystemdProbe::Unavailable(
            "unit-test build: the host's systemd is never probed".to_string(),
        );
    }
    let path_var = std::env::var_os("PATH");
    let systemd_run = find_on_path(SYSTEMD_RUN, path_var.as_deref());
    let systemctl = find_on_path("systemctl", path_var.as_deref());
    let output = match (&systemd_run, &systemctl) {
        (Some(_), Some(systemctl)) => {
            let mut cmd = tokio::process::Command::new(systemctl);
            cmd.args(["--user", "show", "-p", "Version", "--value"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            match tokio::time::timeout(MANAGER_PROBE_TIMEOUT, cmd.output()).await {
                Err(_) => None,
                Ok(Err(e)) => Some(Err(e.to_string())),
                Ok(Ok(out)) => Some(Ok((
                    out.status.success(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                ))),
            }
        }
        _ => None,
    };
    probe_verdict(systemd_run, systemctl.is_some(), output)
}

/// Build the runner's `Command` with its isolation decided at construction.
///
/// The returned `Command` has its program (and, when scoped, the `systemd-run`
/// leading args) set and nothing else: the caller applies env, cwd, stdio and
/// the process group exactly as for a direct spawn, and `systemd-run` passes
/// all of them through to the runner it execs.
pub async fn isolation_command(
    kind: &RunnerKind,
    runner_id: &str,
    exe_path: &Path,
) -> (tokio::process::Command, Isolation, Option<SystemdProbe>) {
    let is_linux = cfg!(target_os = "linux");
    // Probe only when the answer can change the spawn: a user-owned runner on
    // Linux. A temp runner never pays for a `systemctl` round-trip.
    let probe = if is_linux && !crate::process::job::should_assign_to_ephemeral_job(kind) {
        Some(probe_systemd().await)
    } else {
        None
    };
    let plan = plan_spawn(
        kind,
        runner_id,
        exe_path,
        probe
            .as_ref()
            .unwrap_or(&SystemdProbe::Unavailable("not probed".to_string())),
        is_linux,
        chrono::Utc::now(),
    );
    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args);
    (cmd, plan.isolation, probe)
}

/// The landing bound: [`LANDING_TIMEOUT_ENV`] when it parses to a positive
/// number of seconds, else [`DEFAULT_LANDING_TIMEOUT`].
pub fn landing_timeout() -> Duration {
    landing_timeout_from(std::env::var(LANDING_TIMEOUT_ENV).ok().as_deref())
}

/// [`landing_timeout`] with the env read passed in.
pub fn landing_timeout_from(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LANDING_TIMEOUT)
}

/// One read of a spawned pid through the proc-root seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingObservation {
    /// The exe is no longer `systemd-run` and the cgroup ends in the unit.
    Landed,
    /// The exe is still `systemd-run`: the scope start has not exec'd yet.
    Pending,
    /// The exe is no longer `systemd-run`, but the process is NOT in the unit's
    /// cgroup — it exec'd somewhere the supervisor did not ask for.
    Elsewhere { cgroup: String },
    /// The process has exited (a zombie, not yet reaped): its landing can no
    /// longer be observed. `systemd-run` failing to start the scope and a
    /// runner that exec'd and died at once look identical from here.
    Exited,
    /// `/proc/<pid>/exe` or `/proc/<pid>/cgroup` could not be read and the
    /// process is not known to have exited. UNKNOWN — never a landing.
    Unreadable(String),
}

/// The one-letter state of `<proc_root>/<pid>/stat` (`R`, `S`, `Z`, …): the
/// field after the `)` closing the command name.
pub fn proc_state(proc_root: &Path, pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().next()?.chars().next()
}

/// The cgroup v2 path of a `/proc/<pid>/cgroup` body: the `0::` line, else the
/// path field of the last line (a v1/hybrid layout).
pub fn cgroup_path(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    if let Some(v2) = lines.iter().find_map(|l| l.strip_prefix("0::")) {
        return Some(v2.trim().to_string());
    }
    lines
        .last()
        .and_then(|l| l.splitn(3, ':').nth(2))
        .map(|p| p.trim().to_string())
}

/// Read `<proc_root>/<pid>/exe` and `<proc_root>/<pid>/cgroup` once and
/// classify the landing of `unit` (full name, `.scope` included).
pub fn observe_landing(proc_root: &Path, pid: u32, unit: &str) -> LandingObservation {
    let pid_dir = proc_root.join(pid.to_string());
    let exe = match std::fs::read_link(pid_dir.join("exe")) {
        Ok(exe) => exe,
        // A zombie's `exe` link is unreadable; its `stat` still says `Z`.
        Err(e) => {
            return match proc_state(proc_root, pid) {
                Some('Z') | Some('X') => LandingObservation::Exited,
                _ => LandingObservation::Unreadable(format!("exe: {e}")),
            }
        }
    };
    if exe.file_name().and_then(|n| n.to_str()) == Some(SYSTEMD_RUN) {
        return LandingObservation::Pending;
    }
    let body = match std::fs::read_to_string(pid_dir.join("cgroup")) {
        Ok(body) => body,
        Err(e) => return LandingObservation::Unreadable(format!("cgroup: {e}")),
    };
    let Some(path) = cgroup_path(&body) else {
        return LandingObservation::Unreadable("cgroup: no cgroup path in the file".to_string());
    };
    if path.rsplit('/').next() == Some(unit) {
        LandingObservation::Landed
    } else {
        LandingObservation::Elsewhere { cgroup: path }
    }
}

/// How a scope start ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingOutcome {
    /// The runner is running inside the unit.
    Landed,
    /// The start failed; the reason names the shape. The spawned process has
    /// been killed and reaped — nothing of this start survives.
    Failed(String),
    /// The spawned process exited before its landing could be observed: either
    /// `systemd-run` could not start the scope, or the runner exec'd and died
    /// at once. The process is left UNREAPED, so the caller's ordinary
    /// exit handling (exit monitor, panic log, early log — which carries
    /// `systemd-run`'s own stderr) reports it. Nothing of this start is alive,
    /// so nothing user-owned can be left in the supervisor's cgroup.
    ExitedBeforeLanding(String),
}

/// SIGKILL the spawned process and its process group (the runner spawn site
/// makes every runner a group leader), then reap it, bounded at 2 s.
///
/// Identity is guaranteed by the handle: `child` has not been reaped, so its
/// pid cannot have been reused by another process.
async fn kill_spawned(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: scalar arguments only. `pid` is our own un-reaped child and
        // the leader of its own group (`process_group(0)` at the spawn site),
        // so `-pid` names that group and nothing else; ESRCH is harmless.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

/// Confirm a `systemd-run --scope` spawn landed in `unit`, within `bound`.
///
/// Polls `<proc_root>/<pid>/{exe,cgroup,stat}` every `poll`, never reaping
/// the child while it does (an unreaped child's pid cannot be reused, which
/// is what makes every kill below hit the right process):
/// - landed → [`LandingOutcome::Landed`];
/// - exited before landing → [`LandingOutcome::ExitedBeforeLanding`], naming
///   both shapes it could be (`systemd-run` never exec'd, or the runner died
///   at once), the child left for the caller's exit handling;
/// - exec'd into a cgroup that is not the unit → killed, failed;
/// - at the bound, still pending or unreadable → the identity is re-read
///   once (a landing at that last instant is accepted), otherwise the pending
///   `systemd-run` is killed and only THEN is the failure returned, so a late
///   exec cannot start a runner already reported as failed.
///
/// It never retries the spawn, scoped or otherwise.
pub async fn await_scope_landing(
    child: &mut tokio::process::Child,
    proc_root: &Path,
    unit: &str,
    bound: Duration,
    poll: Duration,
) -> LandingOutcome {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        let Some(pid) = child.id() else {
            return LandingOutcome::Failed(format!(
                "the spawned process vanished before it landed in {unit}"
            ));
        };
        match observe_landing(proc_root, pid, unit) {
            LandingObservation::Landed => return LandingOutcome::Landed,
            LandingObservation::Exited => {
                return LandingOutcome::ExitedBeforeLanding(format!(
                    "PID {pid} exited before it was observed in {unit}: either {SYSTEMD_RUN} could \
                     not start the scope (it never exec'd the runner) or the runner exec'd and \
                     died at once — the runner's early log carries {SYSTEMD_RUN}'s own error"
                ));
            }
            LandingObservation::Elsewhere { cgroup } => {
                kill_spawned(child).await;
                return LandingOutcome::Failed(format!(
                    "the runner (PID {pid}) started in cgroup {cgroup}, not in {unit}; killed it"
                ));
            }
            LandingObservation::Pending | LandingObservation::Unreadable(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            // Identity check before the kill: what is this pid NOW?
            let last = observe_landing(proc_root, pid, unit);
            if last == LandingObservation::Landed {
                return LandingOutcome::Landed;
            }
            if last == LandingObservation::Exited {
                return LandingOutcome::ExitedBeforeLanding(format!(
                    "PID {pid} exited at the {}s bound before it was observed in {unit}",
                    bound.as_secs_f64()
                ));
            }
            kill_spawned(child).await;
            let shape = match last {
                LandingObservation::Pending => format!("{SYSTEMD_RUN} was still pending"),
                LandingObservation::Unreadable(why) => {
                    format!("the pid could not be read ({why})")
                }
                LandingObservation::Elsewhere { cgroup } => {
                    format!("the runner was in cgroup {cgroup}")
                }
                LandingObservation::Landed | LandingObservation::Exited => {
                    unreachable!("handled above")
                }
            };
            return LandingOutcome::Failed(format!(
                "the scope start did not land in {unit} within {}s ({shape}); killed PID {pid}",
                bound.as_secs_f64()
            ));
        }
        tokio::time::sleep(poll).await;
    }
}

/// Summarise the `/health` `runner_isolation` block. Pure.
///
/// `probe` is the most recent systemd preflight (`None` = not run yet, which
/// is UNKNOWN). `spawned` holds, for each registered runner this supervisor
/// started and that is still running, its name and its spawn verdict.
pub fn summarize_isolation(
    is_linux: bool,
    probe: Option<&SystemdProbe>,
    spawned: &[(String, Isolation)],
) -> RunnerIsolationReport {
    if !is_linux {
        return RunnerIsolationReport {
            mechanism: "none",
            reason: "not Linux — the systemd scope isolation is Linux-only (on Windows a \
                     user-owned runner is simply never assigned to the kill-on-close job)"
                .to_string(),
        };
    }
    let unscoped: Vec<String> = spawned
        .iter()
        .filter_map(|(name, iso)| match iso {
            Isolation::Unscoped(why) => Some(format!("'{name}' ({why})")),
            _ => None,
        })
        .collect();
    if !unscoped.is_empty() {
        return RunnerIsolationReport {
            mechanism: "none",
            reason: format!(
                "user-owned runner(s) running in the supervisor's cgroup: {}",
                unscoped.join(", ")
            ),
        };
    }
    match probe {
        None => RunnerIsolationReport {
            mechanism: "none",
            reason: "UNKNOWN — the systemd preflight has not run yet".to_string(),
        },
        Some(SystemdProbe::Unavailable(why)) => RunnerIsolationReport {
            mechanism: "none",
            reason: why.clone(),
        },
        Some(SystemdProbe::Available { .. }) => {
            let scoped = spawned
                .iter()
                .filter(|(_, iso)| matches!(iso, Isolation::Scoped(_)))
                .count();
            RunnerIsolationReport {
                mechanism: "systemd-scope",
                reason: format!(
                    "user-owned runners are spawned into their own transient systemd scope \
                     ({scoped} running now)"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 11, 4, 23, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    fn available() -> SystemdProbe {
        SystemdProbe::Available {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
        }
    }

    fn named() -> RunnerKind {
        RunnerKind::Named {
            name: "named-9880".to_string(),
        }
    }

    fn argv(plan: &SpawnPlan) -> Vec<String> {
        std::iter::once(plan.program.to_string_lossy().into_owned())
            .chain(plan.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    const EXE: &str = "/ws/target/debug/qontinui-runner-named-9880";

    #[test]
    fn named_runner_is_wrapped_in_its_own_scope() {
        let plan = plan_spawn(
            &named(),
            "named-9880",
            Path::new(EXE),
            &available(),
            true,
            ts(),
        );
        assert_eq!(
            argv(&plan),
            vec![
                "/usr/bin/systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit",
                "qontinui-runner-named-9880-20261104T230000000Z",
                "--",
                EXE,
            ]
        );
        assert_eq!(
            plan.isolation,
            Isolation::Scoped("qontinui-runner-named-9880-20261104T230000000Z.scope".to_string())
        );
    }

    #[test]
    fn primary_and_external_runners_are_wrapped_too() {
        for (kind, id) in [
            (RunnerKind::Primary, "primary"),
            (RunnerKind::External, "ext-1"),
        ] {
            let plan = plan_spawn(&kind, id, Path::new(EXE), &available(), true, ts());
            let unit = format!("qontinui-runner-{id}-20261104T230000000Z");
            assert_eq!(
                argv(&plan),
                vec![
                    "/usr/bin/systemd-run",
                    "--user",
                    "--scope",
                    "--quiet",
                    "--collect",
                    "--unit",
                    unit.as_str(),
                    "--",
                    EXE,
                ],
                "{kind:?}"
            );
            assert!(matches!(plan.isolation, Isolation::Scoped(_)), "{kind:?}");
        }
    }

    /// A temp runner belongs to the supervisor's lifetime: never scoped out,
    /// whatever systemd says.
    #[test]
    fn temp_runner_is_never_wrapped() {
        let kind = RunnerKind::Temp {
            id: "test-abc123".to_string(),
        };
        let plan = plan_spawn(
            &kind,
            "test-abc123",
            Path::new(EXE),
            &available(),
            true,
            ts(),
        );
        assert_eq!(argv(&plan), vec![EXE]);
        assert_eq!(plan.isolation, Isolation::NotApplicable);
    }

    #[test]
    fn off_linux_nothing_is_wrapped() {
        let plan = plan_spawn(
            &named(),
            "named-9880",
            Path::new(EXE),
            &available(),
            false,
            ts(),
        );
        assert_eq!(argv(&plan), vec![EXE]);
        assert_eq!(plan.isolation, Isolation::NotApplicable);
    }

    #[test]
    fn systemd_absent_spawns_unscoped_and_says_why() {
        let probe = probe_verdict(None, true, None);
        assert_eq!(
            probe,
            SystemdProbe::Unavailable("systemd-run is not on PATH".to_string())
        );
        let plan = plan_spawn(&named(), "named-9880", Path::new(EXE), &probe, true, ts());
        assert_eq!(argv(&plan), vec![EXE]);
        assert_eq!(
            plan.isolation,
            Isolation::Unscoped("systemd-run is not on PATH".to_string())
        );
    }

    #[test]
    fn manager_not_answering_spawns_unscoped() {
        let run = Some(PathBuf::from("/usr/bin/systemd-run"));
        // Timed out.
        let timed_out = probe_verdict(run.clone(), true, None);
        assert!(
            matches!(&timed_out, SystemdProbe::Unavailable(w) if w.contains("did not answer")),
            "{timed_out:?}"
        );
        // Ran and failed (no user bus).
        let failed = probe_verdict(run.clone(), true, Some(Ok((false, String::new()))));
        assert!(matches!(failed, SystemdProbe::Unavailable(_)), "{failed:?}");
        // Answered with nothing.
        let empty = probe_verdict(run.clone(), true, Some(Ok((true, "  \n".to_string()))));
        assert!(matches!(empty, SystemdProbe::Unavailable(_)), "{empty:?}");
        // Could not run at all.
        let err = probe_verdict(run.clone(), true, Some(Err("EACCES".to_string())));
        assert!(matches!(err, SystemdProbe::Unavailable(_)), "{err:?}");
        // No systemctl.
        let no_ctl = probe_verdict(run.clone(), false, None);
        assert_eq!(
            no_ctl,
            SystemdProbe::Unavailable("systemctl is not on PATH".to_string())
        );
        for probe in [timed_out, failed, empty, err, no_ctl] {
            let plan = plan_spawn(&named(), "named-9880", Path::new(EXE), &probe, true, ts());
            assert_eq!(argv(&plan), vec![EXE]);
            assert!(matches!(plan.isolation, Isolation::Unscoped(_)));
        }
        // And only a real answer is Available.
        assert_eq!(
            probe_verdict(run, true, Some(Ok((true, "257\n".to_string())))),
            available()
        );
    }

    #[test]
    fn unit_name_is_sanitised_and_bounded() {
        assert_eq!(
            scope_unit_base("we ird/id.ü", ts()),
            "qontinui-runner-we_ird_id__-20261104T230000000Z"
        );
        let long = "x".repeat(400);
        assert!(scope_unit_base(&long, ts()).len() < 200);
        assert_eq!(
            scope_unit_base("", ts()),
            "qontinui-runner-_-20261104T230000000Z"
        );
    }

    #[test]
    fn landing_timeout_defaults_to_20s_and_is_configurable() {
        assert_eq!(landing_timeout_from(None), Duration::from_secs(20));
        assert_eq!(landing_timeout_from(Some("0")), Duration::from_secs(20));
        assert_eq!(landing_timeout_from(Some("junk")), Duration::from_secs(20));
        assert_eq!(landing_timeout_from(Some(" 45 ")), Duration::from_secs(45));
    }

    #[test]
    fn cgroup_path_reads_v2_and_hybrid_layouts() {
        assert_eq!(
            cgroup_path("0::/user.slice/user-1000.slice/x.scope\n").as_deref(),
            Some("/user.slice/user-1000.slice/x.scope")
        );
        assert_eq!(
            cgroup_path("12:pids:/a\n1:name=systemd:/user.slice/y.scope\n0::/user.slice/y.scope\n")
                .as_deref(),
            Some("/user.slice/y.scope")
        );
        assert_eq!(
            cgroup_path("1:name=systemd:/z.scope\n").as_deref(),
            Some("/z.scope")
        );
        assert_eq!(cgroup_path(""), None);
    }

    // ---- proc-root seam -------------------------------------------------

    const UNIT: &str = "qontinui-runner-x-20261104T230000000Z.scope";
    const SUPERVISOR_CGROUP: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-supervisor-x.service";

    /// Write a fake `<root>/<pid>/{exe,cgroup}`.
    #[cfg(unix)]
    fn fake_proc(root: &Path, pid: u32, exe: &str, cgroup: &str) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).expect("fake pid dir");
        let link = dir.join("exe");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(exe, &link).expect("fake exe link");
        std::fs::write(dir.join("cgroup"), format!("0::{cgroup}\n")).expect("fake cgroup");
    }

    #[cfg(unix)]
    #[test]
    fn observe_reads_landed_pending_and_elsewhere() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fake_proc(
            root,
            10,
            "/ws/target/debug/qontinui-runner-x",
            &format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{UNIT}"),
        );
        assert_eq!(observe_landing(root, 10, UNIT), LandingObservation::Landed);

        fake_proc(root, 11, "/usr/bin/systemd-run", SUPERVISOR_CGROUP);
        assert_eq!(observe_landing(root, 11, UNIT), LandingObservation::Pending);

        fake_proc(
            root,
            12,
            "/ws/target/debug/qontinui-runner-x",
            SUPERVISOR_CGROUP,
        );
        assert_eq!(
            observe_landing(root, 12, UNIT),
            LandingObservation::Elsewhere {
                cgroup: SUPERVISOR_CGROUP.to_string()
            }
        );
        assert!(matches!(
            observe_landing(root, 13, UNIT),
            LandingObservation::Unreadable(_)
        ));

        // A zombie: no readable exe, `stat` says Z.
        std::fs::create_dir_all(root.join("14")).expect("fake pid dir");
        std::fs::write(
            root.join("14").join("stat"),
            "14 (systemd-run) Z 1 14 14 0 -1\n",
        )
        .expect("fake stat");
        assert_eq!(observe_landing(root, 14, UNIT), LandingObservation::Exited);
    }

    /// A real, live dummy child (its own process group, as at the spawn site)
    /// standing in for the spawned pid; the fake proc root decides what the
    /// landing check SEES for it.
    #[cfg(unix)]
    fn dummy_child() -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        crate::process::unix_kill::lead_own_process_group(&mut cmd);
        cmd.spawn().expect("spawn dummy")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn landing_in_the_unit_is_scoped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut child = dummy_child();
        let pid = child.id().expect("pid");
        fake_proc(
            tmp.path(),
            pid,
            "/ws/target/debug/qontinui-runner-x",
            &format!("/app.slice/{UNIT}"),
        );
        let outcome = await_scope_landing(
            &mut child,
            tmp.path(),
            UNIT,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(outcome, LandingOutcome::Landed);
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "a landed runner must be left running"
        );
        let _ = child.kill().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn landing_in_the_parents_cgroup_is_a_failed_start_and_kills_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut child = dummy_child();
        let pid = child.id().expect("pid");
        fake_proc(
            tmp.path(),
            pid,
            "/ws/target/debug/qontinui-runner-x",
            SUPERVISOR_CGROUP,
        );
        let outcome = await_scope_landing(
            &mut child,
            tmp.path(),
            UNIT,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        match outcome {
            LandingOutcome::Failed(reason) => {
                assert!(reason.contains(SUPERVISOR_CGROUP), "{reason}");
            }
            other => {
                panic!("a runner in the supervisor's cgroup must be a failed start: {other:?}")
            }
        }
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "a runner that landed outside its scope must be killed, not left running"
        );
    }

    /// `systemd-run` still pending at the bound: identity re-read, killed,
    /// THEN reported failed — and nothing is retried.
    #[cfg(unix)]
    #[tokio::test]
    async fn still_pending_at_the_bound_is_killed_then_failed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut child = dummy_child();
        let pid = child.id().expect("pid");
        fake_proc(tmp.path(), pid, "/usr/bin/systemd-run", SUPERVISOR_CGROUP);
        let started = std::time::Instant::now();
        let outcome = await_scope_landing(
            &mut child,
            tmp.path(),
            UNIT,
            Duration::from_millis(300),
            Duration::from_millis(10),
        )
        .await;
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "the bound must be waited out before giving up"
        );
        match outcome {
            LandingOutcome::Failed(reason) => {
                assert!(reason.contains("still pending"), "{reason}");
                assert!(reason.contains(&format!("killed PID {pid}")), "{reason}");
            }
            other => panic!("a pending scope start at the bound must fail: {other:?}"),
        }
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "the pending systemd-run must be killed BEFORE the failure is reported"
        );
    }

    /// The spawned process exits before it lands (`systemd-run` could not
    /// start the scope): reported as exited-before-landing, naming both shapes,
    /// and the child is left UNREAPED for the caller's exit handling. Read
    /// through the REAL `/proc`, where an exited, unreaped child is a zombie.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exiting_before_landing_is_reported_and_left_unreaped() {
        let mut child = tokio::process::Command::new("false")
            .spawn()
            .expect("spawn false");
        let pid = child.id().expect("pid");
        // Let it exit first (a zombie until reaped): a check that catches it
        // still running reads `/usr/bin/false` outside the unit, which is the
        // Elsewhere arm, not this one.
        let started = std::time::Instant::now();
        while proc_state(Path::new("/proc"), pid) != Some('Z') {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "`false` never exited"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let outcome = await_scope_landing(
            &mut child,
            Path::new("/proc"),
            UNIT,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        match &outcome {
            LandingOutcome::ExitedBeforeLanding(reason) => {
                assert!(reason.contains("could not start the scope"), "{reason}");
                assert!(reason.contains(&format!("PID {pid}")), "{reason}");
            }
            other => panic!("an exit before landing must be reported as such: {other:?}"),
        }
        assert_eq!(
            child.id(),
            Some(pid),
            "the landing check must not reap the child — the caller's exit handling owns it"
        );
        let status = child.wait().await.expect("wait");
        assert!(!status.success());
    }

    // ---- /health summary -------------------------------------------------

    #[test]
    fn health_summary_is_systemd_scope_only_with_a_probe_and_no_unscoped_runner() {
        let scoped = ("n1".to_string(), Isolation::Scoped(UNIT.to_string()));
        let unscoped = (
            "n2".to_string(),
            Isolation::Unscoped("systemd-run is not on PATH".to_string()),
        );

        let ok = summarize_isolation(true, Some(&available()), std::slice::from_ref(&scoped));
        assert_eq!(ok.mechanism, "systemd-scope");

        let mixed = summarize_isolation(true, Some(&available()), &[scoped.clone(), unscoped]);
        assert_eq!(mixed.mechanism, "none");
        assert!(mixed.reason.contains("'n2'"), "{}", mixed.reason);

        let unknown = summarize_isolation(true, None, &[]);
        assert_eq!(unknown.mechanism, "none");
        assert!(unknown.reason.contains("UNKNOWN"), "{}", unknown.reason);

        let down = summarize_isolation(
            true,
            Some(&SystemdProbe::Unavailable("no user manager".to_string())),
            &[],
        );
        assert_eq!(
            down,
            RunnerIsolationReport {
                mechanism: "none",
                reason: "no user manager".to_string()
            }
        );

        assert_eq!(
            summarize_isolation(false, Some(&available()), &[scoped]).mechanism,
            "none"
        );
    }
}

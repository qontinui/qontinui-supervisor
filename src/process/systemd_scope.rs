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
    /// (and has spawned and still runs) leaves its cgroup; `"unknown"` before
    /// the systemd preflight has answered; `"none"` otherwise.
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

/// The systemd cgroup path of a `/proc/<pid>/cgroup` body: the unified
/// (`0::`) line; else, on a v1 layout, the `name=systemd` hierarchy (the one
/// systemd places units in); else the path field of the last line.
pub fn cgroup_path(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    if let Some(v2) = lines.iter().find_map(|l| l.strip_prefix("0::")) {
        return Some(v2.trim().to_string());
    }
    let path_of = |l: &str| l.splitn(3, ':').nth(2).map(|p| p.trim().to_string());
    if let Some(named) = lines
        .iter()
        .find(|l| l.split(':').nth(1) == Some("name=systemd"))
    {
        return path_of(named);
    }
    lines.last().and_then(|l| path_of(l))
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
    // Pending is recognised by the exe's NAME. A `systemd-run` reached through
    // a wrapper script (the exe is then its interpreter) reads Elsewhere and
    // fails the start; that is the fail-safe direction — the runner is killed
    // and never runs unscoped — so it is accepted rather than widening
    // "pending" to "anything that is not yet the runner".
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
    /// at once. The process is left UNREAPED and its stdio untouched, so the
    /// caller can reap it and read `systemd-run`'s own error off its stderr;
    /// the spawn site reports the start as FAILED. Nothing of this start is
    /// alive, so nothing user-owned can be left in the supervisor's cgroup.
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
///   at once), the child left unreaped for the caller to reap and report;
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
                     died at once"
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
        // Not "none": nothing has been measured yet, and a consumer must not
        // read an unmeasured answer as a negative one.
        None => RunnerIsolationReport {
            mechanism: "unknown",
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

// ---- the supervisor's OWN unit (self-restart) ------------------------------

/// Where this process sits relative to a systemd SERVICE unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnUnit {
    /// Not inside a `*.service` leaf cgroup (a scope, a slice, or not Linux).
    NotAService,
    /// Inside service `unit`, whose main process is `main_pid`.
    Service { unit: String, main_pid: u32 },
    /// Whether this process is a service's main process could not be read.
    Unknown { why: String },
}

/// The `*.service` leaf of a cgroup path, if that is what the path ends in. Pure.
pub fn service_leaf(cgroup: &str) -> Option<&str> {
    let leaf = cgroup.rsplit('/').next()?;
    (leaf.len() > ".service".len() && leaf.ends_with(".service")).then_some(leaf)
}

/// Why a self-restart must be refused, or `None` when it may proceed. Pure.
///
/// A replacement spawned by a service's MAIN process lives in the same unit,
/// and systemd stops the unit and kills the rest of its cgroup the moment the
/// main process exits (`KillMode=mixed`, and the default `control-group`;
/// plan Phase 0 P2c) — so the replacement dies with the process it replaces.
/// That is how `supervisor-verbs.sh start` runs the supervisor on Linux once
/// plan Phase 2 lands. An UNKNOWN answer is refused too: the input gates an
/// action.
pub fn self_restart_refusal(own: &OwnUnit, self_pid: u32) -> Option<String> {
    const HOW: &str = "Restart it with `supervisor-verbs.sh stop` and then `start`";
    match own {
        OwnUnit::NotAService => None,
        OwnUnit::Service { main_pid, .. } if *main_pid != self_pid => None,
        OwnUnit::Service { unit, .. } => Some(format!(
            "this supervisor is the main process of systemd unit {unit}: a replacement it \
             spawned would run in that unit and be killed with it the moment this process \
             exits. {HOW}."
        )),
        OwnUnit::Unknown { why } => Some(format!(
            "whether this supervisor is the main process of a systemd service is UNKNOWN \
             ({why}); a replacement spawned by a service's main process is killed with it, so \
             the restart is refused. {HOW}."
        )),
    }
}

/// Which unit to ask about, read from a `/proc/self/cgroup` body: `Ok(None)`
/// when this process is not in a `*.service` leaf, `Ok(Some((unit, user)))`
/// when it is — `user` true for a unit under the user manager's cgroup
/// (`…/user@<uid>.service/…`), which is then asked with `systemctl --user`.
/// Pure.
pub fn own_service_unit(cgroup_body: &str) -> Result<Option<(String, bool)>, String> {
    let Some(path) = cgroup_path(cgroup_body) else {
        return Err("/proc/self/cgroup names no cgroup path".to_string());
    };
    Ok(service_leaf(&path).map(|unit| (unit.to_string(), path.contains("/user@"))))
}

/// Classify `systemctl show -p MainPID --value <unit>`'s answer — `Ok((exited
/// successfully, stdout))`, or `Err(why it did not answer)`. A main pid of 0
/// (inactive, or a unit with no main process) is UNKNOWN, never "someone
/// else's". Pure.
pub fn own_unit_from_main_pid(unit: &str, answer: Result<(bool, String), String>) -> OwnUnit {
    let why = match answer {
        Err(why) => why,
        Ok((false, _)) => format!("`systemctl show -p MainPID {unit}` failed"),
        Ok((true, stdout)) => match stdout.trim().parse::<u32>() {
            Ok(main_pid) if main_pid > 0 => {
                return OwnUnit::Service {
                    unit: unit.to_string(),
                    main_pid,
                }
            }
            _ => format!("`systemctl show -p MainPID {unit}` named no main process"),
        },
    };
    OwnUnit::Unknown { why }
}

/// Why a user-owned runner must NOT be spawned unscoped, or `None`. A runner
/// that could not be scoped out shares this supervisor's cgroup; when this
/// supervisor is the MAIN process of its own service (how
/// `supervisor-verbs.sh start` runs it once plan Phase 2 lands, with
/// `KillMode=mixed`) that runner is killed with the unit on ANY supervisor
/// exit, a crash included. UNKNOWN refuses too: the input gates a spawn.
/// Everything that is not `Unscoped` is not this function's business. Pure.
pub fn unscoped_spawn_refusal(
    isolation: &Isolation,
    own: &OwnUnit,
    self_pid: u32,
) -> Option<String> {
    let Isolation::Unscoped(why) = isolation else {
        return None;
    };
    match own {
        OwnUnit::NotAService => None,
        OwnUnit::Service { main_pid, .. } if *main_pid != self_pid => None,
        OwnUnit::Service { unit, .. } => Some(format!(
            "it could not be scoped out ({why}), and this supervisor is the main process of \
             systemd unit {unit}, so the runner would share that unit's cgroup and be killed \
             with it on any supervisor exit"
        )),
        OwnUnit::Unknown { why: own_why } => Some(format!(
            "it could not be scoped out ({why}), and whether this supervisor is the main \
             process of a systemd service is UNKNOWN ({own_why})"
        )),
    }
}

/// Read this process's own unit: `<proc_root>/self/cgroup`, and — when that
/// ends in a `*.service` — the unit's `MainPID` from a bounded
/// `systemctl [--user] show`. Off Linux, and in a unit-test build (which never
/// queries the host's systemd), [`OwnUnit::NotAService`]; the decisions are
/// [`own_service_unit`] and [`own_unit_from_main_pid`], tested directly.
pub async fn own_unit(proc_root: &Path) -> OwnUnit {
    if !cfg!(target_os = "linux") || cfg!(test) {
        return OwnUnit::NotAService;
    }
    let body = match std::fs::read_to_string(proc_root.join("self").join("cgroup")) {
        Ok(body) => body,
        Err(e) => {
            return OwnUnit::Unknown {
                why: format!("/proc/self/cgroup: {e}"),
            }
        }
    };
    let (unit, user) = match own_service_unit(&body) {
        Ok(Some(found)) => found,
        Ok(None) => return OwnUnit::NotAService,
        Err(why) => return OwnUnit::Unknown { why },
    };
    let mut cmd = tokio::process::Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["show", "-p", "MainPID", "--value", &unit])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let answer = match tokio::time::timeout(MANAGER_PROBE_TIMEOUT, cmd.output()).await {
        Err(_) => Err(format!(
            "`systemctl show -p MainPID {unit}` did not answer within {}s",
            MANAGER_PROBE_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => Err(format!(
            "`systemctl show -p MainPID {unit}` could not run: {e}"
        )),
        Ok(Ok(out)) => Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )),
    };
    own_unit_from_main_pid(&unit, answer)
}

// ---- small decisions the HTTP and startup paths share ----------------------

/// `GET /runners` `isolation_unit`: the scope of a RUNNING runner, or `None` —
/// a stopped runner's scope has already been collected, so naming it would
/// point at nothing. Pure.
pub fn isolation_unit_for(isolation: Option<&Isolation>, running: bool) -> Option<String> {
    isolation
        .filter(|_| running)
        .and_then(|i| i.unit().map(str::to_string))
}

/// Store the STARTUP systemd probe only when no spawn-time probe has already
/// answered: a spawn re-probes and records its own, newer result, and an older
/// reading must not overwrite it. Returns whether it was stored. Pure.
pub fn record_startup_probe(slot: &mut Option<SystemdProbe>, probe: SystemdProbe) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(probe);
    true
}

/// Reap a spawned process that has already exited, and read the tail of its
/// piped stderr — both bounded, since a grandchild could still hold the pipe.
/// Returns `(exit status, stderr tail)` as display strings; either reads as
/// UNKNOWN with the reason, never as an empty success. The tail is at most
/// [`STDERR_TAIL_BYTES`] of the end of what was written.
pub async fn reap_with_stderr_tail(child: &mut tokio::process::Child) -> (String, String) {
    use tokio::io::AsyncReadExt;
    const BOUND: Duration = Duration::from_secs(2);

    let status = match tokio::time::timeout(BOUND, child.wait()).await {
        Ok(Ok(status)) => status.to_string(),
        Ok(Err(e)) => format!("UNKNOWN (wait failed: {e})"),
        Err(_) => format!("UNKNOWN (not reaped within {}s)", BOUND.as_secs()),
    };
    let tail = match child.stderr.take() {
        None => "UNKNOWN (no stderr pipe)".to_string(),
        Some(mut stderr) => {
            let mut buf = Vec::new();
            match tokio::time::timeout(BOUND, stderr.read_to_end(&mut buf)).await {
                Ok(Ok(_)) => {
                    let start = buf.len().saturating_sub(STDERR_TAIL_BYTES);
                    let text = String::from_utf8_lossy(&buf[start..]).trim().to_string();
                    if text.is_empty() {
                        "(empty)".to_string()
                    } else {
                        text
                    }
                }
                Ok(Err(e)) => format!("UNKNOWN (read failed: {e})"),
                Err(_) => format!(
                    "UNKNOWN (the pipe stayed open past {}s — a child may still hold it)",
                    BOUND.as_secs()
                ),
            }
        }
    };
    (status, tail)
}

/// How much of the end of a failed scope start's stderr is reported.
pub const STDERR_TAIL_BYTES: usize = 2048;

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
        // The unified (`0::`) line wins wherever it sits — here it is NOT the
        // last line, so a last-line reader would answer the v1 path.
        assert_eq!(
            cgroup_path("0::/user.slice/y.scope\n1:name=systemd:/user.slice/other.scope\n")
                .as_deref(),
            Some("/user.slice/y.scope")
        );
        assert_eq!(
            cgroup_path("1:name=systemd:/z.scope\n").as_deref(),
            Some("/z.scope")
        );
        // A v1 layout: the `name=systemd` hierarchy wins even when it is not
        // the last line, so a last-line reader would answer `/mem`.
        assert_eq!(
            cgroup_path("2:cpu,cpuacct:/cpu\n1:name=systemd:/user.slice/a.scope\n3:memory:/mem\n")
                .as_deref(),
            Some("/user.slice/a.scope")
        );
        assert_eq!(
            cgroup_path("2:cpu,cpuacct:/cpu\n3:memory:/mem\n").as_deref(),
            Some("/mem")
        );
        assert_eq!(cgroup_path(""), None);
    }

    // ---- proc-root seam -------------------------------------------------

    const UNIT: &str = "qontinui-runner-x-20261104T230000000Z.scope";
    // Read only by the `#[cfg(unix)]` proc-root tests below; ungated, a
    // Windows `clippy --all-targets -D warnings` fails on it as dead code.
    #[cfg(unix)]
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

        // Other SCOPES are elsewhere too — the hazard the plan names is itself
        // a scope (`qontinui-pty-holder-*`), and a sibling runner's scope
        // shares the unit prefix. Only the unit's own leaf is a landing.
        let pty_holder =
            "/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-pty-holder-abc.scope";
        fake_proc(root, 15, "/ws/target/debug/qontinui-runner-x", pty_holder);
        assert_eq!(
            observe_landing(root, 15, UNIT),
            LandingObservation::Elsewhere {
                cgroup: pty_holder.to_string()
            }
        );
        let sibling = "/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-runner-y-20261104T230000000Z.scope";
        fake_proc(root, 16, "/ws/target/debug/qontinui-runner-x", sibling);
        assert_eq!(
            observe_landing(root, 16, UNIT),
            LandingObservation::Elsewhere {
                cgroup: sibling.to_string()
            }
        );

        // A zombie: no readable exe, `stat` says Z.
        std::fs::create_dir_all(root.join("14")).expect("fake pid dir");
        std::fs::write(
            root.join("14").join("stat"),
            "14 (systemd-run) Z 1 14 14 0 -1\n",
        )
        .expect("fake stat");
        assert_eq!(observe_landing(root, 14, UNIT), LandingObservation::Exited);
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
        assert_eq!(unknown.mechanism, "unknown");
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

    // ---- the supervisor's own unit (self-restart) -------------------------

    #[test]
    fn service_leaf_names_only_a_service() {
        assert_eq!(
            service_leaf("/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-supervisor-20261105T080000Z.service"),
            Some("qontinui-supervisor-20261105T080000Z.service")
        );
        assert_eq!(
            service_leaf("/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-pty-holder-abc.scope"),
            None
        );
        assert_eq!(
            service_leaf("/user.slice/user-1000.slice/session-3.scope"),
            None
        );
        assert_eq!(service_leaf("/.service"), None);
        assert_eq!(service_leaf(""), None);
    }

    #[test]
    fn self_restart_is_refused_only_for_a_service_main_process_or_unknown() {
        assert_eq!(self_restart_refusal(&OwnUnit::NotAService, 4242), None);
        // Inside a service whose main process is someone else (a bare launch
        // from a runner pane): the replacement outlives this process.
        assert_eq!(
            self_restart_refusal(
                &OwnUnit::Service {
                    unit: "qontinui-runner.service".to_string(),
                    main_pid: 100,
                },
                4242
            ),
            None
        );
        let own = self_restart_refusal(
            &OwnUnit::Service {
                unit: "qontinui-supervisor-20261105T080000Z.service".to_string(),
                main_pid: 4242,
            },
            4242,
        )
        .expect("the main process of its own unit must be refused");
        assert!(
            own.contains("qontinui-supervisor-20261105T080000Z.service")
                && own.contains("supervisor-verbs.sh stop"),
            "{own}"
        );
        let unknown = self_restart_refusal(
            &OwnUnit::Unknown {
                why: "systemctl did not answer".to_string(),
            },
            4242,
        )
        .expect("an UNKNOWN own unit must be refused");
        assert!(unknown.contains("UNKNOWN"), "{unknown}");
    }

    #[test]
    fn own_service_unit_reads_the_leaf_and_the_manager() {
        assert_eq!(
            own_service_unit(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-supervisor-20261105T080000Z.service\n"
            ),
            Ok(Some((
                "qontinui-supervisor-20261105T080000Z.service".to_string(),
                true
            )))
        );
        // A system service: asked WITHOUT `--user`.
        assert_eq!(
            own_service_unit("0::/system.slice/qontinui-supervisor.service\n"),
            Ok(Some(("qontinui-supervisor.service".to_string(), false)))
        );
        assert_eq!(
            own_service_unit("0::/user.slice/user-1000.slice/session-3.scope\n"),
            Ok(None)
        );
        assert!(own_service_unit("").is_err());
    }

    #[test]
    fn a_main_pid_of_zero_or_a_failed_answer_is_unknown() {
        let unit = "qontinui-supervisor-20261105T080000Z.service";
        assert_eq!(
            own_unit_from_main_pid(unit, Ok((true, "4242\n".to_string()))),
            OwnUnit::Service {
                unit: unit.to_string(),
                main_pid: 4242
            }
        );
        for answer in [
            Ok((true, "0\n".to_string())),
            Ok((true, "\n".to_string())),
            Ok((false, "4242\n".to_string())),
            Err("did not answer".to_string()),
        ] {
            assert!(
                matches!(
                    own_unit_from_main_pid(unit, answer.clone()),
                    OwnUnit::Unknown { .. }
                ),
                "{answer:?} must read UNKNOWN"
            );
        }
    }

    #[test]
    fn isolation_unit_is_named_only_for_a_running_runner() {
        let scoped = Isolation::Scoped(UNIT.to_string());
        assert_eq!(
            isolation_unit_for(Some(&scoped), true).as_deref(),
            Some(UNIT)
        );
        assert_eq!(isolation_unit_for(Some(&scoped), false), None);
        assert_eq!(
            isolation_unit_for(Some(&Isolation::Unscoped("x".to_string())), true),
            None
        );
        assert_eq!(isolation_unit_for(None, true), None);
    }

    #[test]
    fn the_startup_probe_never_overwrites_a_newer_one() {
        let mut slot = None;
        assert!(record_startup_probe(&mut slot, available()));
        assert_eq!(slot, Some(available()));
        let newer = SystemdProbe::Unavailable("a spawn-time probe answered".to_string());
        let mut slot = Some(newer.clone());
        assert!(!record_startup_probe(&mut slot, available()));
        assert_eq!(slot, Some(newer));
    }

    #[test]
    fn an_unscoped_user_owned_spawn_is_refused_inside_its_own_service_or_when_unknown() {
        let unscoped = Isolation::Unscoped("`systemctl --user show` did not answer".to_string());
        let own = OwnUnit::Service {
            unit: "qontinui-supervisor-20261105T080000Z.service".to_string(),
            main_pid: 4242,
        };
        let refused = unscoped_spawn_refusal(&unscoped, &own, 4242)
            .expect("inside its own KillMode=mixed service an unscoped runner dies with it");
        assert!(
            refused.contains("qontinui-supervisor-20261105T080000Z.service"),
            "{refused}"
        );
        assert!(unscoped_spawn_refusal(
            &unscoped,
            &OwnUnit::Unknown {
                why: "systemctl did not answer".to_string()
            },
            4242
        )
        .is_some_and(|why| why.contains("UNKNOWN")));
        // A fallback launch outside any service, or inside someone else's
        // service: unscoped is the honest outcome, and it is reported as such.
        assert_eq!(
            unscoped_spawn_refusal(&unscoped, &OwnUnit::NotAService, 4242),
            None
        );
        assert_eq!(
            unscoped_spawn_refusal(
                &unscoped,
                &OwnUnit::Service {
                    unit: "qontinui-runner.service".to_string(),
                    main_pid: 100
                },
                4242
            ),
            None
        );
        // Only an UNSCOPED spawn is this function's business.
        assert_eq!(
            unscoped_spawn_refusal(&Isolation::Scoped(UNIT.to_string()), &own, 4242),
            None
        );
        assert_eq!(
            unscoped_spawn_refusal(&Isolation::NotApplicable, &own, 4242),
            None
        );
    }
}

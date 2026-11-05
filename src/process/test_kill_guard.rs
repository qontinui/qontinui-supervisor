#![cfg(test)]
//! A unit test never signals a process it did not spawn: the guard every
//! signal-sending primitive asserts in a unit-test build (plan
//! `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`, Phase 4, D3(c)).
//!
//! # Why
//!
//! The stop ladder kills whatever listens on a runner's port with no image
//! check (`UntrackedListenerTree`, `PortTree`), and the purge's Windows arm
//! taskkills every listener on a purged temp runner's port. So a unit test
//! whose "dead" port another process held, or whose fixture registered a
//! runner on a live temp-runner port, killed a process it never started: on
//! Windows the purge tests taskkilled whatever sat on 9877-9879. Phase 2 moved
//! every fixture onto ports nothing holds; this makes the class unreachable
//! rather than avoided, by checking the property itself at the primitives
//! every kill path ends in.
//!
//! # The rule
//!
//! Before a signal is sent, every process it reaches must be a strict
//! descendant of this test process: in
//! `subtree_census::inclusive_subtree_by(std::process::id(), rows, ParentLink::Proven)`
//! over one `subtree_census::snapshot_rows` snapshot, and not this process
//! itself, whose signal would end every test in the binary. The walk carries
//! the census's visited-set cycle guard, and follows a parent link only when
//! both start times are known and the parent started no later than the child.
//! A real link always passes (both times are floors of one clock), and an
//! orphan whose stale parent pid (Windows keeps one) now names a later process
//! does not: the census's own rule, which errs toward over-counting, would keep
//! a link inside its skew or of unknown age.
//!
//! - **A pid** absent from the snapshot is allowed: there is nothing to signal
//!   (it exited between its caller's check and this one). A thread's id is
//!   judged as its process, which is what a signal to it reaches: sysinfo lists
//!   every Linux thread as a row parented to its process, so the test's own
//!   threads would otherwise pass as its descendants. `0`, and any value
//!   `kill()` would read as a negative pid, are refused: they name a process
//!   group, or every process, rather than one process.
//! - **A group** (`kill(-pgid)`): every live member ([`ProcRow::pgrp`]) must
//!   be a descendant. A group with no live member is allowed, because nothing
//!   is signalled. Group `1` is refused outright: `kill(-1)` is a signal to
//!   every process this user may signal, not to a group.
//! - **A tree** (`taskkill /T`): every process taskkill reaches — the pid and
//!   everything under it by the raw parent links taskkill follows, stale ones
//!   included — must be a descendant.
//! - A snapshot that cannot be read refuses: no answer is not a descendant.
//!
//! A refusal **panics before the signal**, so even a panic a spawned task
//! swallows kills nothing. Its message names the pid, its image and the rule.
//!
//! # Where it runs
//!
//! [`assert_may_signal_pid`] in `unix_kill::raw_kill`, in `windows::kill_by_pid`
//! and before the sysinfo kill in `windows::cleanup_orphaned_slot_processes`;
//! [`assert_may_signal_group`] in `unix_kill::raw_kill_group`;
//! [`assert_may_signal_tree`] in `windows::kill_by_pid_tree`. Every kill path
//! ends in one of them: `stop_runner_by_id`, the purge's Windows `kill_by_port`,
//! `reap_stale_test_runners`, `reconcile_orphaned_temp_runners` and
//! `cleanup_orphaned_slot_processes`.
//!
//! Two signal sites are exempt, each because of what it can reach.
//! `unix_kill::pid_alive`'s `kill(pid, 0)` delivers nothing.
//! `guarded_command::kill_tree` ends the process group (Unix) or JobObject
//! (Windows) that process-wrap made for a child this process spawned, and
//! `KillOnDrop` ends the same one when that child is dropped unreaped. Only
//! that child's own tree can join either: a group through `setpgid` from
//! inside it, a job by inheritance from a member. So every member is a process
//! the test started, reparented or not, and a strict-descendant check would
//! wrongly refuse the reparented ones.
//!
//! `tests::no_signal_is_sent_outside_a_guarded_primitive` holds that set. The
//! per-primitive asserts cannot see a primitive added after them; that test
//! fails any signal call site in production code outside them (a `kill` from
//! libc or nix, a raw signal syscall, a sysinfo kill, a process-wrap kill, a
//! `taskkill`, `kill` or `pkill` command), any listed primitive whose guard is
//! missing, is the wrong one, or comes after its signal, and an exemption that
//! sends anything but what it is exempt for.
//!
//! # Bound
//!
//! This module exists only in a unit-test build. An integration test under
//! `tests/` links the library without `cfg(test)`, so its kills are
//! unguarded; those tests spawn and kill their own processes by design
//! (`tests/orphan_kill_unix.rs`) and must make sure themselves that what they
//! kill is theirs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use crate::process::subtree_census::{
    inclusive_subtree_by, snapshot_rows, CensusError, ParentLink, ProcRow,
};

/// How every refusal starts, so a test can tell the guard's panic from another.
pub(crate) const REFUSAL: &str = "test kill guard: refusing to signal";

/// The rule a refusal names.
const RULE: &str = "a unit test may signal only a process it spawned (a descendant of the \
                    test process), and the guard refuses before any signal is sent (plan \
                    2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy, Phase 4)";

/// Whether `pid` is a strict descendant of this process, read from one
/// process-table snapshot. False for this process itself, for a pid not in the
/// table, and when the table cannot be read.
pub(crate) fn is_descendant_of_this_process(pid: u32) -> bool {
    snapshot_rows().is_ok_and(|rows| descendants(&rows, std::process::id()).contains(&pid))
}

/// Panic, before any signal is sent, unless a signal to `pid` reaches only a
/// process this test spawned.
pub(crate) fn assert_may_signal_pid(pid: u32) {
    assert_may_signal(Target::Pid(pid));
}

/// Panic, before any signal is sent, unless every live member of process group
/// `pgid` descends from this test process.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn assert_may_signal_group(pgid: u32) {
    assert_may_signal(Target::Group(pgid));
}

/// Panic, before any signal is sent, unless everything `taskkill /T` would end
/// from `pid` descends from this test process.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn assert_may_signal_tree(pid: u32) {
    assert_may_signal(Target::Tree(pid));
}

fn assert_may_signal(target: Target) {
    let rows = snapshot_rows();
    if let Some(refusal) = refusal(rows.as_deref(), std::process::id(), target) {
        panic!("{refusal}");
    }
}

/// What a signal is addressed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// One process: `kill(pid)`, `taskkill /PID`, a sysinfo kill.
    Pid(u32),
    /// Every member of a process group: `kill(-pgid)`.
    Group(u32),
    /// A process and everything under it by parent links: `taskkill /T /PID`.
    Tree(u32),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Pid(pid) => write!(f, "pid {pid}"),
            Target::Group(pgid) => write!(f, "process group {pgid}"),
            Target::Tree(pid) => write!(f, "the process tree of pid {pid}"),
        }
    }
}

/// The strict descendants of `own` in `rows`, by proven parent links only.
fn descendants(rows: &[ProcRow], own: u32) -> HashSet<u32> {
    let mut set: HashSet<u32> = inclusive_subtree_by(own, rows, ParentLink::Proven)
        .into_iter()
        .collect();
    set.remove(&own);
    set
}

/// Everything `taskkill /T /PID <pid>` ends: `pid` and every process under it
/// by the raw parent links Windows keeps, a stale one included, and none when
/// `pid` is not in the table.
fn raw_tree(rows: &[ProcRow], pid: u32) -> Vec<&ProcRow> {
    let mut children: HashMap<u32, Vec<&ProcRow>> = HashMap::new();
    for row in rows {
        if let Some(ppid) = row.ppid {
            children.entry(ppid).or_default().push(row);
        }
    }
    let mut reached: Vec<&ProcRow> = rows.iter().filter(|r| r.pid == pid).collect();
    let mut visited: HashSet<u32> = reached.iter().map(|r| r.pid).collect();
    let mut queue: VecDeque<u32> = visited.iter().copied().collect();
    while let Some(parent) = queue.pop_front() {
        for &child in children.get(&parent).into_iter().flatten() {
            if visited.insert(child.pid) {
                reached.push(child);
                queue.push_back(child.pid);
            }
        }
    }
    reached
}

/// What a signal to `pid` reaches: its process, which for a thread's id is the
/// process the thread belongs to; none when `pid` is not in the table.
fn process_of(rows: &[ProcRow], pid: u32) -> Vec<&ProcRow> {
    let process = rows
        .iter()
        .find(|r| r.pid == pid)
        .map(|r| match (r.thread, r.ppid) {
            (true, Some(owner)) => owner,
            _ => r.pid,
        });
    rows.iter().filter(|r| Some(r.pid) == process).collect()
}

/// A process's image for a refusal: its exe path, else its name.
fn image(row: &ProcRow) -> String {
    match &row.exe {
        Some(exe) => exe.display().to_string(),
        None if !row.name.is_empty() => row.name.clone(),
        None => "image unknown".to_string(),
    }
}

/// Why a signal to `target` would reach a process this test did not spawn, as
/// the refusal message; `None` when it may be sent. `own` is the test process.
fn refusal(rows: Result<&[ProcRow], &CensusError>, own: u32, target: Target) -> Option<String> {
    let (Target::Pid(raw) | Target::Group(raw) | Target::Tree(raw)) = target;
    if raw == 0 || i32::try_from(raw).is_err() {
        return Some(format!(
            "{REFUSAL} {target}: kill() reads {raw} as a process group or as every process, \
             not as one process this test spawned; {RULE}"
        ));
    }
    if target == Target::Group(1) {
        return Some(format!(
            "{REFUSAL} {target}: kill(-1) is a signal to every process this user may signal, \
             not to a group; {RULE}"
        ));
    }
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            return Some(format!(
                "{REFUSAL} {target}: the process table could not be read ({e}), so nothing it \
                 reaches can be shown to descend from this test process (pid {own}); {RULE}"
            ))
        }
    };
    let descendants = descendants(rows, own);
    let reached: Vec<&ProcRow> = match target {
        Target::Pid(pid) => process_of(rows, pid),
        Target::Group(pgid) => rows.iter().filter(|r| r.pgrp == Some(pgid)).collect(),
        Target::Tree(pid) => raw_tree(rows, pid),
    };
    let foreign = reached
        .into_iter()
        .find(|r| !descendants.contains(&r.pid))?;
    Some(format!(
        "{REFUSAL} {target}: it reaches pid {} ({}), which is not a descendant of this test \
         process (pid {own}); {RULE}",
        foreign.pid,
        image(foreign)
    ))
}

/// A process this test made but is not the parent of, for a test that needs
/// one: alive, this user's, and not a descendant. Dropping it ends it
/// ([`Orphan::end`]), directly and never through a guarded primitive, which
/// would refuse.
///
/// Unix: `sh` starts `sleep` in the background, prints its pid and exits, so
/// `sleep` is reparented away from this process. Windows: `cmd /c start /b`
/// starts `ping` and exits, and the orphan is the `ping` whose stale parent
/// pid is that `cmd`'s.
pub(crate) struct Orphan {
    pub(crate) pid: u32,
    /// Its start time when it was made, so [`Orphan::end`] signals the pid only
    /// while it still names this process: once the orphan has exited, the pid
    /// can be reused by a process this test never made.
    start_time_secs: Option<u64>,
}

impl Orphan {
    pub(crate) fn spawn() -> Self {
        let pid = spawn_orphan_pid();
        Self {
            pid,
            start_time_secs: crate::process::temp_runner_ledger::process_start_time_secs(pid),
        }
    }

    /// End the orphan, if its pid still names it. Best effort.
    pub(crate) fn end(&self) {
        let still_it = self.start_time_secs.is_some()
            && crate::process::temp_runner_ledger::process_start_time_secs(self.pid)
                == self.start_time_secs;
        if !still_it {
            return;
        }
        #[cfg(unix)]
        {
            if let Ok(pid) = libc::pid_t::try_from(self.pid) {
                // SAFETY: a scalar pid and signal; no memory is touched.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &self.pid.to_string()])
                .output();
        }
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        self.end();
    }
}

/// Start the orphan process (see [`Orphan`]) and return its pid.
fn spawn_orphan_pid() -> u32 {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("sh")
            .args(["-c", "sleep 60 </dev/null >/dev/null 2>&1 & echo $!"])
            .output()
            .expect("run sh to start the orphan");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("sh printed the orphan's pid")
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("cmd")
            .args(["/c", "start", "/b", "ping", "-n", "60", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("run cmd to start the orphan");
        let cmd_pid = cmd.id();
        let status = cmd.wait().expect("wait for cmd");
        assert!(status.success(), "cmd /c start failed: {status:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = snapshot_rows().expect("a process table");
            if let Some(ping) = rows
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case("ping.exe") && r.ppid == Some(cmd_pid))
            {
                return ping.pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the orphaned ping never appeared"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests;

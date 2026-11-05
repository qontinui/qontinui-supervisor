#![cfg(test)]
//! The guard's predicate on the real process table, its decisions over
//! hand-built tables, and the tripwire that keeps every signal call site in
//! production code inside a guarded primitive.

use super::*;
use crate::source_scan::{self, TokenKind};
use std::path::{Path, PathBuf};

// --- the predicate, on the real process table -------------------------------

/// This test's own child, which runs until the test ends it: it waits for end of
/// file on its stdin, whose write end the returned handle holds, so only a kill,
/// or dropping that stdin, ends it, however long the test is held up.
fn a_child() -> std::process::Child {
    #[cfg(unix)]
    let mut command = std::process::Command::new("sh");
    #[cfg(unix)]
    command.args(["-c", "read unused"]);
    #[cfg(windows)]
    let mut command = std::process::Command::new("cmd");
    #[cfg(windows)]
    command.args(["/C", "set /p unused="]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn a child")
}

#[test]
fn an_own_child_is_a_descendant() {
    let mut child = a_child();
    let descends = is_descendant_of_this_process(child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(descends, "a child this test spawned is its descendant");
}

#[test]
fn an_orphan_the_test_made_is_not_a_descendant() {
    let orphan = Orphan::spawn();
    let descends = is_descendant_of_this_process(orphan.pid);
    let alive = crate::process::proc_kill::pid_alive(orphan.pid);
    let pid = orphan.pid;
    drop(orphan);
    assert!(alive, "the orphan {pid} is alive");
    assert!(
        !descends,
        "an orphan this test made but is not the parent of ({pid}) is not its descendant"
    );
}

#[test]
fn pid_1_this_process_and_a_reaped_child_are_not_descendants() {
    assert!(!is_descendant_of_this_process(1), "pid 1");
    assert!(
        !is_descendant_of_this_process(std::process::id()),
        "the test process is not its own descendant"
    );
    let mut child = a_child();
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        !is_descendant_of_this_process(pid),
        "a reaped child's pid {pid} names no process"
    );
}

/// An orphan's cleanup signals its pid only while the pid still names it: once
/// the orphan has exited, the pid may name a process this test never made.
#[test]
fn an_orphan_is_ended_only_while_its_pid_still_names_it() {
    let mut child = a_child();
    // Held for the whole test: `Child::wait` would close a stdin it still owns,
    // and the child's `read` would then end it with no signal at all, so a
    // cleanup that never signalled would read as one that did.
    let _lifeline = child.stdin.take();
    let pid = child.id();
    let started = crate::process::temp_runner_ledger::process_start_time_secs(pid);
    assert!(started.is_some(), "a live child's start time reads");

    // A record of another process that once had this pid: left alone. A
    // signalled child is not marked exited the instant the kill returns, so
    // it is watched for a second: a SIGKILL lands well inside that, and the
    // spared child lives until this test ends it.
    Orphan::record(pid, started.map(|s| s.saturating_sub(3600))).end();
    let watch_until = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut spared = true;
    while spared && std::time::Instant::now() < watch_until {
        spared = matches!(child.try_wait(), Ok(None));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // A record of this process: ended.
    Orphan::record(pid, started).end();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("poll the child") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => break None,
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(spared, "a pid that names another process is not signalled");
    // Its stdin is still open, so only a signal can have ended it.
    #[cfg(unix)]
    let ended = {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|status| status.signal()) == Some(libc::SIGKILL)
    };
    #[cfg(windows)]
    let ended = status.is_some_and(|status| !status.success());
    assert!(
        ended,
        "the orphan's own process is ended by its cleanup: {status:?}"
    );
}

// --- the decisions, over hand-built tables ----------------------------------

const OWN: u32 = 100;

fn row(pid: u32, ppid: Option<u32>, name: &str, start: u64) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        thread: false,
        name: name.to_string(),
        exe: Some(PathBuf::from(format!("/bin/{name}"))),
        start_time_secs: start,
    }
}

/// A row for a thread of `process`, as sysinfo lists one on Linux.
fn thread_of(tid: u32, process: &ProcRow) -> ProcRow {
    ProcRow {
        pid: tid,
        ppid: Some(process.pid),
        thread: true,
        start_time_secs: process.start_time_secs + 1,
        ..process.clone()
    }
}

/// A hand-built process table and the process group of each of its rows, which
/// the guard reads with `getpgid` beside the census's rows.
struct Table {
    rows: Vec<ProcRow>,
    groups: HashMap<u32, u32>,
}

impl Table {
    fn push(&mut self, row: ProcRow, group: Option<u32>) {
        if let Some(group) = group {
            self.groups.insert(row.pid, group);
        }
        self.rows.push(row);
    }

    fn find(&self, pid: u32) -> ProcRow {
        self.rows
            .iter()
            .find(|r| r.pid == pid)
            .cloned()
            .unwrap_or_else(|| panic!("pid {pid} is in the table"))
    }

    fn decide(&self, target: Target) -> Option<String> {
        refusal(
            Ok(&self.rows),
            |pid| self.groups.get(&pid).copied(),
            OWN,
            target,
        )
    }
}

/// init; cargo, the test's parent; the test process in cargo's group; its two
/// children, one leading its own group with a grandchild in it; and a live
/// runner the test did not start.
fn table() -> Table {
    let mut table = Table {
        rows: Vec::new(),
        groups: HashMap::new(),
    };
    for (pid, ppid, group, name, start) in [
        (1, None, 1, "init", 1),
        (50, Some(1), 50, "cargo", 5),
        (OWN, Some(50), 50, "test-binary", 10),
        (101, Some(OWN), 50, "stub", 20),
        (102, Some(OWN), 102, "runner", 21),
        (103, Some(102), 102, "pane", 22),
        (200, Some(1), 200, "live-runner", 15),
    ] {
        table.push(row(pid, ppid, name, start), Some(group));
    }
    table
}

#[test]
fn a_signal_to_a_descendant_is_allowed() {
    let table = table();
    for pid in [101, 102, 103] {
        assert_eq!(table.decide(Target::Pid(pid)), None, "pid {pid}");
    }
}

#[test]
fn a_signal_to_a_process_the_test_did_not_spawn_is_refused_and_named() {
    let table = table();
    let refused = table
        .decide(Target::Pid(200))
        .expect("a foreign pid is refused");
    for part in [
        REFUSAL,
        "pid 200",
        "/bin/live-runner",
        "(pid 100)",
        "a unit test may signal only a process it spawned",
    ] {
        assert!(refused.contains(part), "{part:?} missing from: {refused}");
    }
    for pid in [OWN, 50, 1] {
        assert!(
            table.decide(Target::Pid(pid)).is_some(),
            "pid {pid} is not a descendant"
        );
    }
}

#[test]
fn a_pid_absent_from_the_table_is_allowed_because_nothing_is_signalled() {
    assert_eq!(table().decide(Target::Pid(999)), None);
    assert_eq!(table().decide(Target::Tree(999)), None);
}

#[test]
fn zero_and_values_kill_reads_as_negative_are_refused() {
    let table = table();
    for target in [
        Target::Pid(0),
        Target::Pid(u32::MAX),
        Target::Pid(1 << 31),
        Target::Group(0),
        Target::Group(u32::MAX),
        Target::Tree(0),
    ] {
        let refused = table
            .decide(target)
            .unwrap_or_else(|| panic!("{target} allowed"));
        assert!(
            refused.contains("as a process group or as every process"),
            "{refused}"
        );
    }
}

/// `kill(-1)` signals every process the user may signal. It is refused for
/// what it is, not because some row happens to sit in group 1: this table has
/// none.
#[test]
fn group_1_is_refused_as_the_signal_to_every_process_it_is() {
    let mut table = table();
    for group in table.groups.values_mut() {
        if *group == 1 {
            *group = 7;
        }
    }
    assert!(!table.groups.values().any(|g| *g == 1));
    let refused = table.decide(Target::Group(1)).expect("group 1 is refused");
    assert!(
        refused.contains("kill(-1) is a signal to every process"),
        "{refused}"
    );
}

#[test]
fn an_unreadable_table_refuses() {
    let unreadable = CensusError::Unreadable("no table".to_string());
    let refused = refusal(Err(&unreadable), |_| None, OWN, Target::Pid(101)).expect("refused");
    assert!(refused.contains("could not be read (process table unreadable: no table)"));
}

/// A pid's group is asked for only when a group signal is judged: a pid or a
/// tree signal reads no process group at all.
#[test]
fn only_a_group_signal_reads_process_groups() {
    let table = table();
    let asked = std::cell::Cell::new(0usize);
    let counting = |pid: u32| {
        asked.set(asked.get() + 1);
        table.groups.get(&pid).copied()
    };
    for target in [Target::Pid(101), Target::Pid(200), Target::Tree(102)] {
        let _ = refusal(Ok(&table.rows), counting, OWN, target);
    }
    assert_eq!(asked.get(), 0, "a pid or tree signal read a process group");
    let _ = refusal(Ok(&table.rows), counting, OWN, Target::Group(102));
    assert_eq!(
        asked.get(),
        table.rows.len(),
        "a group signal reads every row's group once"
    );
}

#[test]
fn a_group_is_allowed_only_when_every_live_member_descends_from_the_test() {
    let mut table = table();
    assert_eq!(table.decide(Target::Group(102)), None, "the runner's group");
    assert_eq!(
        table.decide(Target::Group(777)),
        None,
        "a group with no live member signals nothing"
    );
    let cargo_group = table
        .decide(Target::Group(50))
        .expect("cargo's group is refused");
    assert!(cargo_group.contains("pid 50 (/bin/cargo)"), "{cargo_group}");
    // One stranger in an otherwise descendant group refuses it.
    table.push(row(300, Some(1), "stranger", 30), Some(102));
    let refused = table
        .decide(Target::Group(102))
        .expect("a stranger in the group");
    assert!(refused.contains("pid 300 (/bin/stranger)"), "{refused}");
}

/// A recorded parent pid can be stale: Windows records a process's creator pid
/// and can hand that pid to a new process once the creator exits. When a later
/// descendant holds such a pid, the orphan that recorded it is not that
/// descendant's child, and a tree signal on the descendant, taken to follow
/// recorded links, would reach it.
#[test]
fn a_stale_parent_link_does_not_make_a_descendant_and_refuses_a_tree() {
    let mut table = table();
    table.push(row(400, Some(102), "orphan", 3), None);
    assert!(
        table.decide(Target::Pid(400)).is_some(),
        "an orphan that started before its parent pid's process is not under it"
    );
    assert_eq!(table.decide(Target::Pid(102)), None, "the descendant alone");
    let tree = table
        .decide(Target::Tree(102))
        .expect("the tree reaches the orphan");
    assert!(tree.contains("pid 400 (/bin/orphan)"), "{tree}");
    assert_eq!(table.decide(Target::Tree(101)), None, "a clean tree");
}

/// The guard proves every parent link: a parent that started in a later second
/// than its row, or a start time it cannot read, is no link, although the
/// census follows both. Start times are whole seconds, so a parent that started
/// in its row's own second is a link.
#[test]
fn an_unproven_parent_link_does_not_make_a_descendant() {
    let mut table = table();
    // pid 102's process started at 21: one that "started" at 18 cannot be its
    // child, only an orphan its reused pid now names.
    table.push(
        row(401, Some(102), "orphan-started-before-its-parent", 18),
        None,
    );
    table.push(row(402, Some(102), "orphan-of-unknown-age", 0), None);
    for (pid, name) in [
        (401, "orphan-started-before-its-parent"),
        (402, "orphan-of-unknown-age"),
    ] {
        let refused = table
            .decide(Target::Pid(pid))
            .unwrap_or_else(|| panic!("{name} allowed"));
        assert!(refused.contains(&format!("(/bin/{name})")), "{refused}");
    }
    // The census walks both, as it walks every recorded link.
    let walked = inclusive_subtree(102, &table.rows);
    assert!(walked.contains(&401) && walked.contains(&402), "{walked:?}");

    // One that started in pid 102's own second reads as its child: the clock
    // cannot tell the two apart.
    table.push(row(403, Some(102), "child-in-its-parents-second", 21), None);
    assert_eq!(
        table.decide(Target::Pid(403)),
        None,
        "a child started in its parent's second"
    );
}

/// A signal to a thread's id reaches its whole process, so a thread is judged
/// as its process: the test's own thread is the test, and a descendant's
/// thread is that descendant.
#[test]
fn a_thread_is_judged_as_its_process() {
    let mut table = table();
    let own = table.find(OWN);
    let stub = table.find(101);
    table.push(thread_of(150, &own), Some(50));
    table.push(thread_of(151, &stub), Some(50));
    let refused = table
        .decide(Target::Pid(150))
        .expect("a thread of the test is the test");
    assert!(
        refused.contains("reaches pid 100 (/bin/test-binary)"),
        "{refused}"
    );
    assert_eq!(table.decide(Target::Pid(151)), None, "a thread of a child");
}

/// The predicate judges a pid as the guard does, a thread's id as its process:
/// a thread of the test is not its descendant, though the thread's row is the
/// test's child, and a thread of a descendant is that descendant.
#[test]
fn the_predicate_judges_a_thread_as_its_process() {
    let mut table = table();
    let own = table.find(OWN);
    let stub = table.find(101);
    table.push(thread_of(150, &own), Some(50));
    table.push(thread_of(151, &stub), Some(50));
    for (pid, descends, what) in [
        (150, false, "a thread of the test"),
        (151, true, "a thread of a child"),
        (101, true, "a child"),
        (103, true, "a grandchild"),
        (OWN, false, "the test itself"),
        (200, false, "a process the test did not spawn"),
        (999, false, "a pid absent from the table"),
    ] {
        assert_eq!(
            is_descendant(&table.rows, OWN, pid),
            descends,
            "{what} ({pid})"
        );
    }
}

/// On the real table: a thread of this test is not its descendant. sysinfo
/// lists a Linux thread as a row parented to its process, so by its own row the
/// thread would read as the test's child.
#[cfg(target_os = "linux")]
#[test]
fn a_thread_of_this_test_is_not_its_descendant() {
    let (tid_tx, tid_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        // SAFETY: `gettid` takes no argument and touches no memory.
        let tid = unsafe { libc::gettid() };
        tid_tx
            .send(u32::try_from(tid).expect("a positive thread id"))
            .expect("send the thread id");
        let _ = done_rx.recv();
    });
    let tid = tid_rx.recv().expect("the thread's id");
    let listed = snapshot_rows().is_ok_and(|rows| rows.iter().any(|r| r.pid == tid && r.thread));
    let descends = is_descendant_of_this_process(tid);
    drop(done_tx);
    thread.join().expect("the thread ends");
    assert!(
        listed,
        "the table lists thread {tid} as a thread row, or this test proves nothing"
    );
    assert!(
        !descends,
        "a thread of this test ({tid}) is the test, not its descendant"
    );
}

// --- the tripwire: every signal call site sits in a guarded primitive -------

/// What kind of signal a call site sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteKind {
    /// `kill` / `killpg` from libc or nix.
    Kill,
    /// A kill command started as a process (`taskkill`, `kill`, `pkill`, …).
    KillCommand,
    /// sysinfo's `Process::kill` / `kill_with`.
    Sysinfo,
    /// process-wrap's group or job kill (`ChildWrapper::start_kill` / `kill`).
    ProcessWrap,
    /// A raw signal syscall or the Win32 terminate call.
    Syscall,
}

/// One place production code sends a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SignalSite {
    /// Byte offset into the scanned text.
    offset: usize,
    line: usize,
    kind: SiteKind,
    what: String,
}

/// Commands that kill by pid.
const KILL_COMMANDS: [&str; 6] = [
    "taskkill",
    "taskkill.exe",
    "tskill",
    "kill",
    "pkill",
    "killall",
];

/// The kill commands whose name means nothing else, flagged as a string
/// literal anywhere: a `const TASKKILL: &str = "taskkill"` handed to `new` is
/// the same command. `kill` alone is a word too, so it is flagged only as the
/// one argument of a `new(…)`.
const UNAMBIGUOUS_KILL_COMMANDS: [&str; 5] =
    ["taskkill", "taskkill.exe", "tskill", "pkill", "killall"];

/// Names that send a signal however they are reached: the raw syscalls (and
/// their numbers, for `syscall(SYS_…)`) and the Win32 terminate calls.
const SIGNAL_NAMES: [&str; 11] = [
    "TerminateProcess",
    "NtTerminateProcess",
    "pidfd_send_signal",
    "tgkill",
    "tkill",
    "sigqueue",
    "rt_sigqueueinfo",
    "SYS_kill",
    "SYS_tkill",
    "SYS_tgkill",
    "SYS_pidfd_send_signal",
];

/// A string literal's contents, its prefix, hashes and quotes removed.
fn unquote(literal: &str) -> &str {
    let inner = literal
        .trim_start_matches(['b', 'c', 'r'])
        .trim_matches('#');
    inner
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(inner)
}

/// Whether a string literal spells a kill command with an option after it, the
/// way a shell command does: `"taskkill /F /PID 7"`, `"kill -9 7"`.
fn is_a_shell_kill(literal: &str) -> bool {
    let words: Vec<&str> = unquote(literal).split_whitespace().collect();
    words
        .windows(2)
        .any(|w| KILL_COMMANDS.contains(&w[0]) && (w[1].starts_with('/') || w[1].starts_with('-')))
}

/// Every signal call site in `text`, comments and string contents excluded:
///
/// - a `libc::kill` / `libc::killpg` path, called or not, `signal::kill`
///   (nix's spelling), and `kill` / `killpg` in a `use libc::{…}` group;
/// - a kill command: an [`UNAMBIGUOUS_KILL_COMMANDS`] literal anywhere,
///   `new("kill")`, and a [`KILL_COMMANDS`] word followed by an option inside
///   a literal (a shell command);
/// - in a file that names `sysinfo`, a `.kill(` or `.kill_with(` method call —
///   sysinfo's `Process::kill`. A `Child` handle's kill is the process this one
///   spawned, and a file with no sysinfo in it holds no sysinfo process;
/// - in a file that names `process_wrap`, a `.start_kill(` or `.kill(` method
///   call — process-wrap's group or job kill;
/// - any of the [`SIGNAL_NAMES`].
fn signal_sites(text: &str) -> Vec<SignalSite> {
    let tokens = source_scan::tokens(text).expect("the production source lexes");
    let names = |crate_name: &str| {
        tokens
            .iter()
            .any(|t| t.kind == TokenKind::Ident && t.text == crate_name)
    };
    let (names_sysinfo, names_process_wrap) = (names("sysinfo"), names("process_wrap"));
    let base = text.as_ptr() as usize;
    let mut sites = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let at = |k: usize| tokens.get(i + k);
        let is = |k: usize, kind: TokenKind, texts: &[&str]| {
            at(k).is_some_and(|n| n.kind == kind && texts.contains(&n.text))
        };
        let site = |kind: SiteKind, what: String| Some((kind, what));
        let found = if t.kind == TokenKind::Ident
            && ["libc", "signal"].contains(&t.text)
            && is(1, TokenKind::Punct, &["::"])
            && is(2, TokenKind::Ident, &["kill", "killpg"])
        {
            at(2).and_then(|n| site(SiteKind::Kill, format!("{}::{}", t.text, n.text)))
        } else if t.kind == TokenKind::Ident
            && t.text == "libc"
            && is(1, TokenKind::Punct, &["::"])
            && is(2, TokenKind::Open, &["{"])
        {
            // A `use libc::{…}` group: flag a `kill` or `killpg` inside it.
            let mut depth = 0usize;
            let mut imported = None;
            for n in &tokens[i + 2..] {
                match n.kind {
                    TokenKind::Open => depth += 1,
                    TokenKind::Close => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    TokenKind::Ident if ["kill", "killpg"].contains(&n.text) => {
                        imported = Some(n.text);
                    }
                    _ => {}
                }
            }
            imported.and_then(|name| site(SiteKind::Kill, format!("libc::{{{name}}}")))
        } else if t.kind == TokenKind::Str && UNAMBIGUOUS_KILL_COMMANDS.contains(&unquote(t.text)) {
            site(
                SiteKind::KillCommand,
                format!("the kill command {}", t.text),
            )
        } else if t.kind == TokenKind::Str && is_a_shell_kill(t.text) {
            site(
                SiteKind::KillCommand,
                format!("the kill command inside {}", t.text),
            )
        } else if t.kind == TokenKind::Ident
            && t.text == "new"
            && is(1, TokenKind::Open, &["("])
            && at(2).is_some_and(|n| n.kind == TokenKind::Str && unquote(n.text) == "kill")
            && is(3, TokenKind::Close, &[")"])
        {
            at(2).and_then(|n| site(SiteKind::KillCommand, format!("new({})", n.text)))
        } else if names_sysinfo
            && t.kind == TokenKind::Punct
            && t.text == "."
            && is(1, TokenKind::Ident, &["kill", "kill_with"])
            && is(2, TokenKind::Open, &["("])
        {
            at(1).and_then(|n| site(SiteKind::Sysinfo, format!("a sysinfo .{}()", n.text)))
        } else if names_process_wrap
            && t.kind == TokenKind::Punct
            && t.text == "."
            && is(1, TokenKind::Ident, &["start_kill", "kill"])
            && is(2, TokenKind::Open, &["("])
        {
            at(1).and_then(|n| {
                site(
                    SiteKind::ProcessWrap,
                    format!("a process-wrap .{}()", n.text),
                )
            })
        } else if t.kind == TokenKind::Ident && SIGNAL_NAMES.contains(&t.text) {
            site(SiteKind::Syscall, t.text.to_string())
        } else {
            None
        };
        if let Some((kind, what)) = found {
            sites.push(SignalSite {
                offset: t.text.as_ptr() as usize - base,
                line: t.line,
                kind,
                what,
            });
        }
    }
    sites
}

/// What a function on the [`PRIMITIVES`] list may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Allowed {
    /// Send a signal, after a call to each of these `test_kill_guard` asserts
    /// and to no other one.
    After(&'static [&'static str]),
    /// `kill(pid, 0)` only, which delivers nothing: `unix_kill::pid_alive`.
    SignalZero,
    /// A process-wrap kill only, of the group or job it made for a child this
    /// process spawned, which that child's own tree normally fills (the module
    /// doc says who else can join): `guarded_command::kill_tree`.
    OwnGroupOrJob,
}

/// A function production code may send a signal from, by its file under
/// `src/` and the signature that opens it.
struct Primitive {
    file: &'static str,
    signature: &'static str,
    allowed: Allowed,
}

const PRIMITIVES: [Primitive; 7] = [
    Primitive {
        file: "process/unix_kill.rs",
        signature: "pub(crate) fn pid_alive(",
        allowed: Allowed::SignalZero,
    },
    Primitive {
        file: "process/unix_kill.rs",
        signature: "fn raw_kill(",
        allowed: Allowed::After(&["assert_may_signal_pid"]),
    },
    Primitive {
        file: "process/unix_kill.rs",
        signature: "fn raw_kill_group(",
        allowed: Allowed::After(&["assert_may_signal_group"]),
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn kill_by_pid(",
        allowed: Allowed::After(&["assert_may_signal_pid"]),
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn kill_by_pid_tree(",
        allowed: Allowed::After(&["assert_may_signal_tree"]),
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn cleanup_orphaned_slot_processes(",
        allowed: Allowed::After(&["assert_may_signal_pid"]),
    },
    Primitive {
        file: "process/guarded_command.rs",
        signature: "async fn kill_tree(",
        allowed: Allowed::OwnGroupOrJob,
    },
];

/// Every `test_kill_guard::assert_may_signal_*` call in `body`, as (offset in
/// `body`, the assert's name).
fn guard_calls(body: &str) -> Vec<(usize, &str)> {
    let Some(tokens) = source_scan::tokens(body) else {
        return Vec::new();
    };
    tokens
        .windows(3)
        .filter(|w| {
            w[0].text == "test_kill_guard"
                && w[1].text == "::"
                && w[2].text.starts_with("assert_may_signal_")
        })
        .map(|w| {
            (
                w[0].text.as_ptr() as usize - body.as_ptr() as usize,
                w[2].text,
            )
        })
        .collect()
}

/// Whether the `libc::kill(…)` call at `call` passes the literal `0` as its
/// last argument.
fn signals_zero(call: &str) -> bool {
    let Some(tokens) = source_scan::tokens(call) else {
        return false;
    };
    let Some(open) = tokens.iter().position(|t| t.text == "(") else {
        return false;
    };
    let mut depth = 0usize;
    let mut last_argument: Vec<&str> = Vec::new();
    for t in &tokens[open..] {
        match t.kind {
            TokenKind::Open => {
                depth += 1;
                if depth > 1 {
                    last_argument.push(t.text);
                }
            }
            TokenKind::Close => {
                depth -= 1;
                if depth == 0 {
                    return last_argument == ["0"];
                }
                last_argument.push(t.text);
            }
            TokenKind::Punct if t.text == "," && depth == 1 => last_argument.clear(),
            _ => last_argument.push(t.text),
        }
    }
    false
}

/// Every problem with `sources` — `(path under src/, LF text)` — against
/// `primitives`: a signal site outside every listed primitive; a listed
/// primitive that is gone or sends nothing; a guarded one whose guard is
/// missing, comes after its signal or is the wrong one; and an exempt one that
/// sends anything but what it is exempt for.
fn signal_problems(sources: &[(String, String)], primitives: &[Primitive]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut sends = vec![0usize; primitives.len()];
    for (path, text) in sources {
        if source_scan::is_test_only_file(text) {
            continue;
        }
        let production = source_scan::production_span(text);
        let base = production.as_ptr() as usize;
        let mut bodies: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        for (i, primitive) in primitives.iter().enumerate() {
            if primitive.file != path.as_str() {
                continue;
            }
            match source_scan::fn_body(production, primitive.signature) {
                Some(body) => {
                    let start = body.as_ptr() as usize - base;
                    bodies.push((i, start..start + body.len()));
                }
                None => problems.push(format!(
                    "{path}: no `{}`, which the primitives table lists",
                    primitive.signature
                )),
            }
        }
        let sites = signal_sites(production);
        for site in &sites {
            match bodies
                .iter()
                .find(|(_, range)| range.contains(&site.offset))
            {
                Some((i, _)) => sends[*i] += 1,
                None => problems.push(format!(
                    "{path}:{} sends a signal ({}) outside every guarded primitive",
                    site.line, site.what
                )),
            }
        }
        for (i, range) in &bodies {
            let primitive = &primitives[*i];
            let inside: Vec<&SignalSite> =
                sites.iter().filter(|s| range.contains(&s.offset)).collect();
            match primitive.allowed {
                Allowed::After(guards) => {
                    let calls = guard_calls(&production[range.clone()]);
                    for (_, name) in calls.iter().filter(|(_, name)| !guards.contains(name)) {
                        problems.push(format!(
                            "{path}: `{}` calls the wrong guard `test_kill_guard::{name}`; it \
                             must call {guards:?}",
                            primitive.signature
                        ));
                    }
                    let first_signal = inside.first().map(|s| s.offset);
                    for guard in guards {
                        let guarded = calls
                            .iter()
                            .find(|(_, name)| name == guard)
                            .map(|(at, _)| at + range.start);
                        if !matches!((guarded, first_signal), (Some(g), Some(f)) if g < f) {
                            problems.push(format!(
                                "{path}: `{}` must call `test_kill_guard::{guard}` before it \
                                 sends a signal",
                                primitive.signature
                            ));
                        }
                    }
                }
                Allowed::SignalZero => {
                    for site in &inside {
                        if site.kind != SiteKind::Kill
                            || !signals_zero(&production[site.offset..range.end])
                        {
                            problems.push(format!(
                                "{path}:{} `{}` may send only signal 0, which delivers nothing",
                                site.line, primitive.signature
                            ));
                        }
                    }
                }
                Allowed::OwnGroupOrJob => {
                    for site in inside.iter().filter(|s| s.kind != SiteKind::ProcessWrap) {
                        problems.push(format!(
                            "{path}:{} `{}` may send only process-wrap's kill of its own group \
                             or job, not {}",
                            site.line, primitive.signature, site.what
                        ));
                    }
                }
            }
        }
    }
    for (primitive, sent) in primitives.iter().zip(&sends) {
        if *sent == 0 {
            problems.push(format!(
                "{}: `{}` sends no signal, or its file was not scanned — the primitives \
                 table is stale",
                primitive.file, primitive.signature
            ));
        }
    }
    problems
}

/// Every `.rs` file under `dir`, as `(path relative to dir with `/`, LF text)`.
fn sources_under(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap_or_else(|e| panic!("list {next:?}: {e}")) {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {path:?}: {e}"))
                    .replace("\r\n", "\n");
                let rel = path
                    .strip_prefix(dir)
                    .expect("under the dir")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, text));
            }
        }
    }
    out
}

#[test]
fn no_signal_is_sent_outside_a_guarded_primitive() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = sources_under(&src);
    assert!(
        sources.len() > 20,
        "scanned only {} source file(s)",
        sources.len()
    );
    let problems = signal_problems(&sources, &PRIMITIVES);
    assert!(
        problems.is_empty(),
        "a unit test must never signal a process it did not spawn, so production code \
         sends a signal only from a primitive that asserts `test_kill_guard` first. Route \
         a new kill through `proc_kill` (or guard the new primitive and list it in \
         `PRIMITIVES`):\n  {}",
        problems.join("\n  ")
    );
}

/// The scan's own fixtures: each shape a signal call site takes is flagged, and
/// each near miss is not.
#[test]
fn the_signal_scan_flags_each_shape_and_nothing_else() {
    let table = [
        Primitive {
            file: "unix.rs",
            signature: "pub(crate) fn pid_alive(",
            allowed: Allowed::SignalZero,
        },
        Primitive {
            file: "unix.rs",
            signature: "fn raw_kill(",
            allowed: Allowed::After(&["assert_may_signal_pid"]),
        },
        Primitive {
            file: "win.rs",
            signature: "pub async fn sweep(",
            allowed: Allowed::After(&["assert_may_signal_pid"]),
        },
        Primitive {
            file: "wrap.rs",
            signature: "async fn kill_tree(",
            allowed: Allowed::OwnGroupOrJob,
        },
    ];
    let pid_alive = "pub(crate) fn pid_alive(pid: u32) -> bool {\n    \
                     let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };\n    rc == 0\n}\n";
    let raw_kill = "fn raw_kill(pid: u32, signal: libc::c_int) -> bool {\n    \
                    #[cfg(test)]\n    \
                    crate::process::test_kill_guard::assert_may_signal_pid(pid);\n    \
                    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };\n    rc == 0\n}\n";
    let sweep = "use sysinfo::System;\npub async fn sweep(system: &System) {\n    \
                 for process in system.processes().values() {\n        \
                 crate::process::test_kill_guard::assert_may_signal_pid(process.pid().as_u32());\n        \
                 if process.kill() {}\n    }\n}\n";
    let wrap = "use process_wrap::tokio::ChildWrapper;\n\
                async fn kill_tree(child: &mut Box<dyn ChildWrapper>) {\n    \
                let _ = child.start_kill();\n}\n";
    let clean = |extra_unix: &str, extra_win: &str, extra_wrap: &str| {
        vec![
            (
                "unix.rs".to_string(),
                format!("{pid_alive}{raw_kill}{extra_unix}"),
            ),
            ("win.rs".to_string(), format!("{sweep}{extra_win}")),
            ("wrap.rs".to_string(), format!("{wrap}{extra_wrap}")),
        ]
    };
    let problems = |extra_unix: &str, extra_win: &str, extra_wrap: &str| {
        signal_problems(&clean(extra_unix, extra_win, extra_wrap), &table)
    };
    let edited = |from: &str, to: &str| {
        let sources: Vec<(String, String)> = clean("", "", "")
            .into_iter()
            .map(|(path, text)| (path, text.replace(from, to)))
            .collect();
        signal_problems(&sources, &table)
    };

    // The guarded table itself is clean, and so are the near misses.
    assert_eq!(problems("", "", ""), Vec::<String>::new());
    let near_misses = "// libc::kill(pid, 9) in a comment\n\
                       fn quiet(child: &mut std::process::Child) {\n    \
                       let s = \"libc::kill(pid, 9)\";\n    \
                       let _ = Command::new(\"cmd\").arg(\"/C\");\n    \
                       info!(\"taskkill: killed PID {}\", 1);\n    \
                       warn!(\"falling through to taskkill\");\n    \
                       warn!(\"NO kill strategy ever ran\");\n    \
                       let _ = child.kill();\n    \
                       let _ = s;\n}\n\
                       #[cfg(test)]\nmod tests {\n    \
                       fn cleanup(pid: i32) { unsafe { libc::kill(pid, 9); } }\n}\n";
    assert_eq!(problems(near_misses, "", ""), Vec::<String>::new());
    // A Child handle's kill is this process's own child: fine in a file with no
    // sysinfo and no process-wrap in it. A file that is test code in its
    // entirety is not production.
    for (path, text) in [
        (
            "expo.rs",
            "async fn stop(child: &mut tokio::process::Child) {\n    \
             let _ = child.kill().await;\n    child.start_kill().ok();\n}\n",
        ),
        (
            "helper.rs",
            "#![cfg(test)]\nfn end(pid: i32) { unsafe { libc::kill(pid, 9); } }\n",
        ),
    ] {
        let mut sources = clean("", "", "");
        sources.push((path.to_string(), text.to_string()));
        assert_eq!(
            signal_problems(&sources, &table),
            Vec::<String>::new(),
            "{path}"
        );
    }

    // Each shape outside a primitive is flagged, once, naming it.
    for (shape, what) in [
        (
            "fn sneaky(pid: u32) {\n    unsafe { libc::kill(pid as i32, 9) };\n}\n",
            "libc::kill",
        ),
        (
            "fn group(pgid: i32) {\n    unsafe { libc::killpg(pgid, 9) };\n}\n",
            "libc::killpg",
        ),
        (
            "use libc::{kill, SIGTERM};\nfn bare(pid: i32) {\n    unsafe { kill(pid, SIGTERM) };\n}\n",
            "libc::{kill}",
        ),
        (
            "fn nix(pid: Pid) {\n    let _ = nix::sys::signal::kill(pid, None);\n}\n",
            "signal::kill",
        ),
        (
            "fn thread_signal(tid: i64) {\n    unsafe { libc::syscall(libc::SYS_tgkill, 1, tid, 9) };\n}\n",
            "SYS_tgkill",
        ),
        (
            "fn pidfd(fd: i32) {\n    \
             unsafe { libc::pidfd_send_signal(fd, 9, std::ptr::null_mut(), 0) };\n}\n",
            "pidfd_send_signal",
        ),
        (
            "fn tk(pid: u32) {\n    let _ = Command::new(\"taskkill\").args([\"/F\"]);\n}\n",
            "the kill command \"taskkill\"",
        ),
        (
            "const TASKKILL: &str = \"taskkill\";\n\
             fn tk(pid: u32) {\n    let _ = Command::new(TASKKILL).arg(pid.to_string());\n}\n",
            "the kill command \"taskkill\"",
        ),
        (
            "fn shell(pid: u32) {\n    \
             let _ = Command::new(\"cmd\").args([\"/C\", &format!(\"taskkill /F /PID {pid}\")]);\n}\n",
            "the kill command inside \"taskkill /F /PID {pid}\"",
        ),
        (
            "fn k(pid: u32) {\n    let _ = tokio::process::Command::new(\"kill\").arg(\"-9\");\n}\n",
            "new(\"kill\")",
        ),
        (
            "fn w(h: HANDLE) {\n    unsafe { TerminateProcess(h, 1) };\n}\n",
            "TerminateProcess",
        ),
    ] {
        let found = problems(shape, "", "");
        assert!(
            found.len() == 1 && found[0].contains(what) && found[0].contains("outside"),
            "{what}: {found:?}"
        );
    }
    let found = problems(
        "",
        "fn other(system: &sysinfo::System) {\n    \
         for p in system.processes().values() { p.kill(); }\n}\n",
        "",
    );
    assert!(
        found.len() == 1 && found[0].contains("a sysinfo .kill()"),
        "{found:?}"
    );
    let found = problems(
        "",
        "",
        "fn other(child: &mut Box<dyn ChildWrapper>) {\n    let _ = child.start_kill();\n}\n",
    );
    assert!(
        found.len() == 1 && found[0].contains("a process-wrap .start_kill()"),
        "{found:?}"
    );

    // A guarded primitive whose guard is gone, comes after its signal, or is
    // the wrong one, is flagged.
    let found = edited(
        "    #[cfg(test)]\n    crate::process::test_kill_guard::assert_may_signal_pid(pid);\n",
        "",
    );
    assert!(
        found.len() == 1 && found[0].contains("`fn raw_kill(` must call"),
        "{found:?}"
    );
    let found = edited(
        "crate::process::test_kill_guard::assert_may_signal_pid(pid);",
        "crate::process::test_kill_guard::assert_may_signal_group(pid);",
    );
    assert!(
        found.len() == 2
            && found.iter().any(|p| p.contains("calls the wrong guard"))
            && found.iter().any(|p| p.contains("`fn raw_kill(` must call")),
        "{found:?}"
    );
    let late = clean("", "", "")
        .into_iter()
        .map(|(path, text)| {
            (
                path,
                text.replace(
                    "if process.kill() {}\n",
                    "if process.kill() {}\n        \
                     crate::process::test_kill_guard::assert_may_signal_pid(1);\n",
                )
                .replace(
                    "        crate::process::test_kill_guard::assert_may_signal_pid(\
                     process.pid().as_u32());\n",
                    "",
                ),
            )
        })
        .collect::<Vec<_>>();
    let found = signal_problems(&late, &table);
    assert!(
        found.len() == 1 && found[0].contains("`pub async fn sweep(` must call"),
        "{found:?}"
    );

    // An exempt primitive may send only what it is exempt for.
    let found = edited(
        "libc::kill(pid as libc::pid_t, 0)",
        "libc::kill(pid as libc::pid_t, libc::SIGTERM)",
    );
    assert!(
        found.len() == 1 && found[0].contains("may send only signal 0"),
        "{found:?}"
    );
    let found = edited(
        "    let _ = child.start_kill();\n",
        "    let _ = child.start_kill();\n    unsafe { libc::kill(-1, 9) };\n",
    );
    assert!(
        found.len() == 1 && found[0].contains("may send only process-wrap's kill"),
        "{found:?}"
    );

    // A stale table is flagged: a listed primitive that is gone, or that sends
    // nothing.
    let found = edited("fn raw_kill(", "fn renamed_kill(");
    assert!(
        found.iter().any(|p| p.contains("no `fn raw_kill(`"))
            && found
                .iter()
                .any(|p| p.contains("outside every guarded primitive")),
        "{found:?}"
    );
    let found = edited(
        "let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };",
        "let rc = 0;",
    );
    assert!(
        found.len() == 2
            && found
                .iter()
                .any(|p| p.contains("`fn raw_kill(` sends no signal"))
            && found.iter().any(|p| p.contains("`fn raw_kill(` must call")),
        "{found:?}"
    );
}

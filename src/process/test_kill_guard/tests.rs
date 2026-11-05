#![cfg(test)]
//! The guard's predicate on the real process table, its decisions over
//! hand-built tables, and the tripwire that keeps every signal call site in
//! production code inside a guarded primitive.

use super::*;
use crate::source_scan::{self, TokenKind};
use std::path::{Path, PathBuf};

// --- the predicate, on the real process table -------------------------------

/// A process that runs for half a minute unless killed: this test's own child.
fn a_child() -> std::process::Child {
    #[cfg(unix)]
    let mut command = std::process::Command::new("sleep");
    #[cfg(unix)]
    command.arg("30");
    #[cfg(windows)]
    let mut command = std::process::Command::new("ping");
    #[cfg(windows)]
    command.args(["-n", "30", "127.0.0.1"]);
    command
        .stdin(std::process::Stdio::null())
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
    let orphan = spawn_an_orphan();
    let descends = is_descendant_of_this_process(orphan);
    let alive = crate::process::proc_kill::pid_alive(orphan);
    end_an_orphan(orphan);
    assert!(alive, "the orphan {orphan} is alive");
    assert!(
        !descends,
        "an orphan this test made but is not the parent of ({orphan}) is not its descendant"
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

// --- the decisions, over hand-built tables ----------------------------------

const OWN: u32 = 100;

fn row(pid: u32, ppid: Option<u32>, pgrp: Option<u32>, name: &str, start: u64) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        pgrp,
        name: name.to_string(),
        exe: Some(PathBuf::from(format!("/bin/{name}"))),
        start_time_secs: start,
    }
}

/// init; cargo, the test's parent; the test process in cargo's group; its two
/// children, one leading its own group with a grandchild in it; and a live
/// runner the test did not start.
fn table() -> Vec<ProcRow> {
    vec![
        row(1, None, Some(1), "init", 1),
        row(50, Some(1), Some(50), "cargo", 5),
        row(OWN, Some(50), Some(50), "test-binary", 10),
        row(101, Some(OWN), Some(50), "stub", 20),
        row(102, Some(OWN), Some(102), "runner", 21),
        row(103, Some(102), Some(102), "pane", 22),
        row(200, Some(1), Some(200), "live-runner", 15),
    ]
}

fn decide(rows: &[ProcRow], target: Target) -> Option<String> {
    refusal(Ok(rows), OWN, target)
}

#[test]
fn a_signal_to_a_descendant_is_allowed() {
    let rows = table();
    for pid in [101, 102, 103] {
        assert_eq!(decide(&rows, Target::Pid(pid)), None, "pid {pid}");
    }
}

#[test]
fn a_signal_to_a_process_the_test_did_not_spawn_is_refused_and_named() {
    let rows = table();
    let refused = decide(&rows, Target::Pid(200)).expect("a foreign pid is refused");
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
            decide(&rows, Target::Pid(pid)).is_some(),
            "pid {pid} is not a descendant"
        );
    }
}

#[test]
fn a_pid_absent_from_the_table_is_allowed_because_nothing_is_signalled() {
    assert_eq!(decide(&table(), Target::Pid(999)), None);
    assert_eq!(decide(&table(), Target::Tree(999)), None);
}

#[test]
fn zero_and_values_kill_reads_as_negative_are_refused() {
    let rows = table();
    for target in [
        Target::Pid(0),
        Target::Pid(u32::MAX),
        Target::Pid(1 << 31),
        Target::Group(0),
        Target::Group(u32::MAX),
        Target::Tree(0),
    ] {
        let refused = decide(&rows, target).unwrap_or_else(|| panic!("{target} allowed"));
        assert!(
            refused.contains("as a process group or as every process"),
            "{refused}"
        );
    }
}

#[test]
fn an_unreadable_table_refuses() {
    let unreadable = CensusError::Unreadable("no table".to_string());
    let refused = refusal(Err(&unreadable), OWN, Target::Pid(101)).expect("refused");
    assert!(refused.contains("could not be read (process table unreadable: no table)"));
}

#[test]
fn a_group_is_allowed_only_when_every_live_member_descends_from_the_test() {
    let mut rows = table();
    assert_eq!(
        decide(&rows, Target::Group(102)),
        None,
        "the runner's group"
    );
    assert_eq!(
        decide(&rows, Target::Group(777)),
        None,
        "a group with no live member signals nothing"
    );
    let cargo_group = decide(&rows, Target::Group(50)).expect("cargo's group is refused");
    assert!(cargo_group.contains("pid 50 (/bin/cargo)"), "{cargo_group}");
    // One stranger in an otherwise descendant group refuses it.
    rows.push(row(300, Some(1), Some(102), "stranger", 30));
    let refused = decide(&rows, Target::Group(102)).expect("a stranger in the group");
    assert!(refused.contains("pid 300 (/bin/stranger)"), "{refused}");
}

/// Windows keeps an orphan's parent pid after the parent exits. When a later
/// descendant reuses that pid, `taskkill /T` on the descendant ends the orphan
/// too, and the orphan is not the descendant's child.
#[test]
fn a_stale_parent_link_does_not_make_a_descendant_and_refuses_a_tree() {
    let mut rows = table();
    rows.push(row(400, Some(102), None, "orphan", 3));
    assert!(
        decide(&rows, Target::Pid(400)).is_some(),
        "an orphan that started before its parent pid's process is not under it"
    );
    assert_eq!(
        decide(&rows, Target::Pid(102)),
        None,
        "the descendant alone"
    );
    let tree = decide(&rows, Target::Tree(102)).expect("the tree reaches the orphan");
    assert!(tree.contains("pid 400 (/bin/orphan)"), "{tree}");
    assert_eq!(decide(&rows, Target::Tree(101)), None, "a clean tree");
}

// --- the tripwire: every signal call site sits in a guarded primitive -------

/// One place production code sends a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SignalSite {
    /// Byte offset into the scanned text.
    offset: usize,
    line: usize,
    what: String,
}

/// Commands that kill by pid, as the one argument of a `new(…)`.
const KILL_COMMANDS: [&str; 6] = [
    "taskkill",
    "taskkill.exe",
    "tskill",
    "kill",
    "pkill",
    "killall",
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

/// Every signal call site in `text`, comments and string contents excluded:
///
/// - a `libc::kill` / `libc::killpg` path, called or not, and `signal::kill`
///   (nix's spelling);
/// - `new("taskkill")` and the other [`KILL_COMMANDS`], however the command
///   type is spelled;
/// - in a file that names `sysinfo`, a `.kill(` or `.kill_with(` method call —
///   sysinfo's `Process::kill`. A `Child` handle's kill is the process this one
///   spawned, and a file with no sysinfo in it holds no sysinfo process;
/// - `TerminateProcess` / `NtTerminateProcess`.
fn signal_sites(text: &str) -> Vec<SignalSite> {
    let tokens = source_scan::tokens(text).expect("the production source lexes");
    let names_sysinfo = tokens
        .iter()
        .any(|t| t.kind == TokenKind::Ident && t.text == "sysinfo");
    let base = text.as_ptr() as usize;
    let mut sites = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let at = |k: usize| tokens.get(i + k);
        let is = |k: usize, kind: TokenKind, texts: &[&str]| {
            at(k).is_some_and(|n| n.kind == kind && texts.contains(&n.text))
        };
        let what = if t.kind == TokenKind::Ident
            && ["libc", "signal"].contains(&t.text)
            && is(1, TokenKind::Punct, &["::"])
            && is(2, TokenKind::Ident, &["kill", "killpg"])
        {
            at(2).map(|n| format!("{}::{}", t.text, n.text))
        } else if t.kind == TokenKind::Ident
            && t.text == "new"
            && is(1, TokenKind::Open, &["("])
            && at(2).is_some_and(|n| {
                n.kind == TokenKind::Str && KILL_COMMANDS.contains(&unquote(n.text))
            })
            && is(3, TokenKind::Close, &[")"])
        {
            at(2).map(|n| format!("new({})", n.text))
        } else if names_sysinfo
            && t.kind == TokenKind::Punct
            && t.text == "."
            && is(1, TokenKind::Ident, &["kill", "kill_with"])
            && is(2, TokenKind::Open, &["("])
        {
            at(1).map(|n| format!("a sysinfo .{}()", n.text))
        } else if t.kind == TokenKind::Ident
            && ["TerminateProcess", "NtTerminateProcess"].contains(&t.text)
        {
            Some(t.text.to_string())
        } else {
            None
        };
        if let Some(what) = what {
            sites.push(SignalSite {
                offset: t.text.as_ptr() as usize - base,
                line: t.line,
                what,
            });
        }
    }
    sites
}

/// What a function on the [`PRIMITIVES`] list may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Allowed {
    /// Send a signal, after a `test_kill_guard::assert_may_signal_*` call.
    AfterTheGuard,
    /// `kill(pid, 0)` only, which delivers nothing: `unix_kill::pid_alive`.
    SignalZero,
}

/// A function production code may send a signal from, by its file under
/// `src/` and the signature that opens it.
struct Primitive {
    file: &'static str,
    signature: &'static str,
    allowed: Allowed,
}

const PRIMITIVES: [Primitive; 6] = [
    Primitive {
        file: "process/unix_kill.rs",
        signature: "pub(crate) fn pid_alive(",
        allowed: Allowed::SignalZero,
    },
    Primitive {
        file: "process/unix_kill.rs",
        signature: "fn raw_kill(",
        allowed: Allowed::AfterTheGuard,
    },
    Primitive {
        file: "process/unix_kill.rs",
        signature: "fn raw_kill_group(",
        allowed: Allowed::AfterTheGuard,
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn kill_by_pid(",
        allowed: Allowed::AfterTheGuard,
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn kill_by_pid_tree(",
        allowed: Allowed::AfterTheGuard,
    },
    Primitive {
        file: "process/windows.rs",
        signature: "pub async fn cleanup_orphaned_slot_processes(",
        allowed: Allowed::AfterTheGuard,
    },
];

/// The offset in `body` of its first `test_kill_guard::assert_may_signal_*`.
fn guard_offset(body: &str) -> Option<usize> {
    let tokens = source_scan::tokens(body)?;
    tokens.windows(3).find_map(|w| {
        (w[0].text == "test_kill_guard"
            && w[1].text == "::"
            && w[2].text.starts_with("assert_may_signal_"))
        .then(|| w[0].text.as_ptr() as usize - body.as_ptr() as usize)
    })
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
/// primitive that is gone, sends nothing, signals before its guard, or (the
/// `pid_alive` exception) sends anything but signal 0.
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
            let first = sites.iter().find(|s| range.contains(&s.offset));
            match primitive.allowed {
                Allowed::AfterTheGuard => {
                    let guard = guard_offset(&production[range.clone()]).map(|g| g + range.start);
                    if !matches!((guard, first), (Some(g), Some(f)) if g < f.offset) {
                        problems.push(format!(
                            "{path}: `{}` must call `test_kill_guard::assert_may_signal_*` \
                             before it sends a signal",
                            primitive.signature
                        ));
                    }
                }
                Allowed::SignalZero => {
                    for site in sites.iter().filter(|s| range.contains(&s.offset)) {
                        if !signals_zero(&production[site.offset..range.end]) {
                            problems.push(format!(
                                "{path}:{} `{}` may send only signal 0, which delivers nothing",
                                site.line, primitive.signature
                            ));
                        }
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
            allowed: Allowed::AfterTheGuard,
        },
        Primitive {
            file: "win.rs",
            signature: "pub async fn sweep(",
            allowed: Allowed::AfterTheGuard,
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
    let clean = |extra_unix: &str, extra_win: &str| {
        vec![
            (
                "unix.rs".to_string(),
                format!("{pid_alive}{raw_kill}{extra_unix}"),
            ),
            ("win.rs".to_string(), format!("{sweep}{extra_win}")),
        ]
    };
    let problems =
        |extra_unix: &str, extra_win: &str| signal_problems(&clean(extra_unix, extra_win), &table);

    // The guarded table itself is clean, and so are the near misses.
    assert_eq!(problems("", ""), Vec::<String>::new());
    let near_misses = "// libc::kill(pid, 9) in a comment\n\
                       fn quiet(child: &mut std::process::Child) {\n    \
                       let s = \"libc::kill(pid, 9)\";\n    \
                       let _ = Command::new(\"cmd\").arg(\"taskkill\");\n    \
                       info!(\"taskkill: killed PID {}\", 1);\n    \
                       let _ = s;\n}\n\
                       #[cfg(test)]\nmod tests {\n    \
                       fn cleanup(pid: i32) { unsafe { libc::kill(pid, 9); } }\n}\n";
    assert_eq!(problems(near_misses, ""), Vec::<String>::new());
    // A Child handle's kill is this process's own child: fine in a file with no
    // sysinfo in it.
    assert_eq!(
        signal_problems(
            &[
                clean("", "").remove(0),
                clean("", "").remove(1),
                (
                    "expo.rs".to_string(),
                    "async fn stop(child: &mut tokio::process::Child) {\n    \
                     let _ = child.kill().await;\n}\n"
                        .to_string(),
                ),
            ],
            &table
        ),
        Vec::<String>::new()
    );
    // A file that is test code in its entirety is not production.
    assert_eq!(
        signal_problems(
            &[
                clean("", "").remove(0),
                clean("", "").remove(1),
                (
                    "helper.rs".to_string(),
                    "#![cfg(test)]\nfn end(pid: i32) { unsafe { libc::kill(pid, 9); } }\n"
                        .to_string(),
                ),
            ],
            &table
        ),
        Vec::<String>::new()
    );

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
            "fn nix(pid: Pid) {\n    let _ = nix::sys::signal::kill(pid, None);\n}\n",
            "signal::kill",
        ),
        (
            "fn tk(pid: u32) {\n    let _ = Command::new(\"taskkill\").args([\"/F\"]);\n}\n",
            "new(\"taskkill\")",
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
        let found = problems(shape, "");
        assert!(
            found.len() == 1 && found[0].contains(what) && found[0].contains("outside"),
            "{what}: {found:?}"
        );
    }
    let found = problems(
        "",
        "fn other(system: &sysinfo::System) {\n    \
         for p in system.processes().values() { p.kill(); }\n}\n",
    );
    assert!(
        found.len() == 1 && found[0].contains("a sysinfo .kill()"),
        "{found:?}"
    );

    // A primitive whose guard is gone, or comes after its signal, is flagged.
    let unguarded = clean("", "")
        .into_iter()
        .map(|(path, text)| {
            (
                path,
                text.replace(
                    "    #[cfg(test)]\n    \
                     crate::process::test_kill_guard::assert_may_signal_pid(pid);\n",
                    "",
                ),
            )
        })
        .collect::<Vec<_>>();
    let found = signal_problems(&unguarded, &table);
    assert!(
        found.len() == 1 && found[0].contains("`fn raw_kill(` must call"),
        "{found:?}"
    );
    let late = clean("", "")
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

    // `pid_alive` may send signal 0 and nothing else.
    let signalling = clean("", "")
        .into_iter()
        .map(|(path, text)| {
            (
                path,
                text.replace(
                    "libc::kill(pid as libc::pid_t, 0)",
                    "libc::kill(pid as libc::pid_t, libc::SIGTERM)",
                ),
            )
        })
        .collect::<Vec<_>>();
    let found = signal_problems(&signalling, &table);
    assert!(
        found.len() == 1 && found[0].contains("may send only signal 0"),
        "{found:?}"
    );

    // A stale table is flagged: a listed primitive that is gone, or that sends
    // nothing.
    let gone = clean("", "")
        .into_iter()
        .map(|(path, text)| (path, text.replace("fn raw_kill(", "fn renamed_kill(")))
        .collect::<Vec<_>>();
    let found = signal_problems(&gone, &table);
    assert!(
        found.iter().any(|p| p.contains("no `fn raw_kill(`"))
            && found
                .iter()
                .any(|p| p.contains("outside every guarded primitive")),
        "{found:?}"
    );
    let silent = clean("", "")
        .into_iter()
        .map(|(path, text)| {
            (
                path,
                text.replace(
                    "let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };",
                    "let rc = 0;",
                ),
            )
        })
        .collect::<Vec<_>>();
    let found = signal_problems(&silent, &table);
    assert!(
        found.len() == 2
            && found
                .iter()
                .any(|p| p.contains("`fn raw_kill(` sends no signal"))
            && found.iter().any(|p| p.contains("`fn raw_kill(` must call")),
        "{found:?}"
    );
}

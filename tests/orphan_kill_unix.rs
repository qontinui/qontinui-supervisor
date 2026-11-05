//! Verification for the D7 orphan-leak fix on Unix: the cross-platform
//! process-kill primitives must actually terminate a real process holding a
//! port and free that port — the behavior the reaper now relies on so it
//! never drops a temp-runner record while its OS process stays alive.
//!
//! This file is Unix-only; Windows uses a different (netstat/taskkill)
//! implementation. **That implementation has no end-to-end test anywhere**, and
//! calling it "tested elsewhere" — as this header used to — is the
//! absence-reads-as-coverage shape the three-state discipline below exists to
//! refuse. What IS gated is its load-bearing predicate, "is this PID LISTENING
//! on this port": that lives outside the `#[cfg(target_os = "windows")]` module
//! in `qontinui_supervisor::process::netstat_parse`, as a pure function over
//! captured text, precisely so `ubuntu-latest` can execute it. The kill and
//! probe behaviour around it is UNKNOWN, not covered — CI here is
//! `ubuntu-latest` only, so a Windows integration test would be gated by
//! nothing at all.
//!
//! **Every test here SKIPS rather than fails on an environmental unknown**, and
//! that is deliberate: an `lsof` that cannot run, an absent `python3` and an
//! occupied port are all UNKNOWN *about the kill primitives* rather than
//! verdicts on them, so a developer on such a box is not handed a red that
//! blames code it never reached. The cost of that choice is that a skip PASSES,
//! and three skips are indistinguishable from three real runs.
//!
//! So each test prints one machine-readable line saying which it was:
//! [`EXERCISED_MARKER`] when the primitive really was driven against a live
//! listener, [`SKIP_MARKER`] with the reason when it was not. CI re-runs this
//! target with `--nocapture` and requires an exercised line naming each
//! primitive on its roster — the assertion lives THERE, not here, because
//! `ubuntu-latest` is a controlled machine where an unrunnable probe is a
//! broken runner rather than a fact to accept, while a developer's laptop is
//! not.
//!
//! That split leaves the two halves agreeing only by string match across a
//! language boundary, so `tests/orphan_kill_contract.rs` pins them together and
//! runs everywhere — including on Windows, where this file compiles to nothing.
#![cfg(not(target_os = "windows"))]

use std::process::Stdio;
use std::time::Duration;

use qontinui_supervisor::process::port::is_port_listening;
use qontinui_supervisor::process::proc_kill;

/// The one poll budget every wait helper in this file shares: 50 x 100ms = 5s.
///
/// Named rather than spelled twice on purpose. The whole point of
/// [`wait_until_probe_sees`] is that it waits *as long as* the bind probe did,
/// so a listener the first observer accepted gets a full budget to become
/// visible to the second. Two matching literals make that agreement a
/// coincidence; one constant makes it structural.
const POLL_ATTEMPTS: usize = 50;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The ports the three tests listen on, one each, so they can run in parallel.
///
/// Below every default ephemeral range (Linux 32768, Windows and macOS 49152),
/// so no kernel allocation hands one out; outside the temp-runner range
/// 9877-9899, where a live temp runner may sit, which these tests used until
/// 2026-11-05 (plan `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`);
/// and clear of the unit-test fixture blocks (`test_ports`, 27_000-28_999) and
/// the older literals 29_871-29_884. Only a process that binds one explicitly
/// can hold it, and then the child's bind fails and its test skips: the kill
/// primitives here are only ever pointed at a listener the test itself spawned
/// (see [`spawn_bound_listener`] and [`ProbeView::Child`]).
const FIND_PID_ON_PORT_PORT: u16 = 26_896;
const KILL_BY_PID_TREE_PORT: u16 = 26_897;
const KILL_BY_PORT_PORT: u16 = 26_898;

/// What the listener child prints once its `listen()` returned: its own word
/// that it holds the port, which no probe of the port can give.
const LISTENING: &str = "listening";

/// Prefix on the one line a test prints when it DECLINES to run.
///
/// Every skip below prints its reason and `return`s, so the test PASSES; CI
/// re-runs this target with `--nocapture` so those reasons reach the log. A
/// reason nothing counts is still only evidence to a reader who happens to
/// look, and the failure this suite exists to catch is precisely the one nobody
/// looks for. The prefix is what lets the workflow step print the reasons back
/// when its census comes up short, instead of leaving an operator to scroll.
const SKIP_MARKER: &str = "ORPHAN_KILL_SKIP";

/// Prefix on the one line a test prints when it DID exercise its primitive
/// against a live listener, all assertions passed and nothing was skipped.
///
/// **This is the countable half, and the one the CI step gates on.** Counting
/// SKIPS would be an absence check and fails OPEN: delete the skip calls, rename
/// the prefix, or delete the tests, and a skip census reads zero and reports
/// green — the same absence-reads-as-coverage shape this file's header refuses.
/// A census of POSITIVE evidence fails CLOSED on every one of those paths,
/// because each removes an `ORPHAN_KILL_EXERCISED` line the step requires to be
/// present.
///
/// One line per D7 primitive — `kill_by_port`, `kill_by_pid_tree`,
/// `find_pid_on_port` — and the workflow gates on those NAMES, not on how many
/// lines appeared. A count is weaker than the contract it was written to
/// enforce: two announcements of one primitive plus one of another totals three
/// while a third primitive was never exercised. The `primitive` argument is
/// therefore load-bearing, not decoration.
///
/// `tests/orphan_kill_contract.rs` pins the pair: the names announced here and
/// the `EXPECTED_PRIMITIVES` roster in `.github/workflows/ci.yml` must be the
/// same set, so renaming a primitive, or adding a test that announces nothing,
/// fails on any developer's box rather than arriving in CI as a census that
/// reads short and blames the runner.
const EXERCISED_MARKER: &str = "ORPHAN_KILL_EXERCISED";

/// Announce a skip. The single place the wording lives; the probe-unavailable
/// message used to be spelled out separately at four call sites.
fn skip(reason: &str) {
    eprintln!("{SKIP_MARKER}: {reason}");
}

/// Announce that `primitive` was exercised against a real, live listener.
///
/// Call this ONLY where the fixture was established and every assertion ran. A
/// marker printed on a skip path would make the census a tautology — which is
/// the defect it exists to detect.
fn exercised(primitive: &str) {
    eprintln!("{EXERCISED_MARKER}: {primitive}");
}

/// A spawned listener child, OWNED so it cannot outlive the test.
///
/// Every assertion in this file panics on failure, and a panic unwinds straight
/// past any hand-written `child.wait()`. Without this guard a failed test leaks
/// a live process holding one of this file's ports, and nothing on the box
/// would ever clean it up: the leak squats the port until reboot, and every
/// later run of the test skips on it. A red test becoming a lasting one is what
/// makes this load-bearing rather than tidiness.
struct Listener {
    child: std::process::Child,
}

impl Listener {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Reap a child the test already killed *through the code under test*, so
    /// the port assertions that follow see a fully exited process rather than a
    /// zombie. Drop would do this anyway; calling it explicitly keeps the
    /// ordering the assertions were written against.
    fn reap(&mut self) {
        let _ = self.child.wait();
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Both are no-ops on a child the test already killed and reaped.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a real child process that binds and LISTENs on `port`, and wait until
/// it holds the port. We use Python (present on macOS/most Linux CI) so the
/// child is a genuine OS process with its own PID and a held TCP listener —
/// exactly the orphan shape D7 describes.
///
/// **The child says itself that it holds the port**, by printing [`LISTENING`]
/// after its `listen()` returned. This used to wait for the port to read held
/// instead, and took that for "my child holds it": a process that took the
/// port between the caller's pre-check and the child's bind read exactly the
/// same, the child's bind failed, and `kill_by_port` then killed that process —
/// on these tests' old ports, a live temp runner. A bind that fails now exits
/// the child before it prints, so the test skips and kills nothing. The child
/// binds without `SO_REUSEADDR`, which on macOS would let it share the port with
/// a listener on the wildcard address, so its bind fails while anything else
/// holds the port. A listener that coexists with it anyway (an IPv6-only socket
/// on Linux) is what [`ProbeView::Other`] catches.
///
/// `None` means **the fixture could not be established**, which is UNKNOWN about
/// the primitives under test rather than a verdict on them. That is the same
/// three-state discipline this file already applies to the `lsof` probe (`Err`
/// is not `Ok(None)`) and to port occupancy, extended to the one remaining place
/// that hard-failed on a purely environmental fact. Two causes qualify, and both
/// are reported on stderr rather than skipped in silence:
///
/// * `python3` could not be spawned at all — absent, or the stock-macOS
///   command-line-tools stub;
/// * the child EXITED before it listened: another process holds the port, or the
///   interpreter ran and the fixture is unusable.
///
/// A child that is still ALIVE and never listened within the budget is **not**
/// environmental — the interpreter is running and the socket never appeared — so
/// that case still panics.
async fn spawn_bound_listener(port: u16) -> Option<Listener> {
    let script = format!(
        "import socket,time\n\
         s=socket.socket(socket.AF_INET,socket.SOCK_STREAM)\n\
         s.bind(('127.0.0.1',{port}))\n\
         s.listen(8)\n\
         print('{LISTENING}',flush=True)\n\
         while True: time.sleep(1)\n"
    );
    let mut child = match std::process::Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            skip(&format!(
                "python3 could not be spawned ({e}) (fixture unavailable)"
            ));
            return None;
        }
    };
    let stdout = child.stdout.take().expect("the listener's stdout is piped");
    let mut listener = Listener { child };
    // The child's first line, or "" once it exits without one.
    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line);
        let _ = said.send(line);
    });

    for _ in 0..POLL_ATTEMPTS {
        match heard.try_recv() {
            Ok(line) if line.trim() == LISTENING => return Some(listener),
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let status = listener
                    .child
                    .wait()
                    .map_or_else(|e| format!("an unknown status ({e})"), |s| s.to_string());
                skip(&format!(
                    "python3 listener exited with {status} before it listened on port {port}: \
                     another process holds the port, or the fixture is unusable"
                ));
                return None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }

    panic!(
        "listener (pid {}) is still alive but never listened on port {port} within the budget",
        listener.pid()
    );
}

/// What the `lsof`-backed probe was observed doing while we waited for it.
#[derive(Debug)]
enum ProbeView {
    /// The probe saw this test's child listening on the port, and nothing else:
    /// every listener `kill_by_port` would kill is the child.
    Child,
    /// The probe saw a process other than the child listening on the port,
    /// alone or beside the child. `kill_by_port` kills every listener, so it
    /// would reach that process, and nothing is killed.
    Other(u32),
    /// The probe RAN for the whole budget and never saw the listener.
    NeverSaw,
    /// The probe could not RUN at all — UNKNOWN, never "nothing is listening".
    Unavailable(String),
}

/// Block until the `lsof`-backed probe sees the child `pid` as the port's only
/// listener, sees another process there, errors, or the shared budget is spent.
/// It reads every listener (`proc_kill::find_pids_on_port`, the probe
/// `kill_by_port` itself iterates), not just the first.
///
/// [`spawn_bound_listener`] waits on `is_port_listening`, which BINDS the port
/// to test it, so it succeeds the moment the listen backlog exists.
/// `find_pid_on_port` shells out to `lsof`, which reads a different view and can
/// lag behind that moment. The two probes are therefore unsynchronised, and
/// `Ok(None)` from the second means "not visible YET", not "nothing is
/// listening" -- so asserting on ONE observation races them. Observed
/// 2026-09-05: `find_pid_on_port_locates_listener` failed with `left: None` on
/// CI and passed on a rerun of the identical commit.
///
/// `Err` is UNKNOWN and will not change by retrying (the standing case is a box
/// with no `lsof`), so it returns straight away rather than burning the whole
/// budget.
///
/// **Why this returns an outcome rather than `()`.** When the budget IS spent,
/// the caller's assertion fires with `left: None` — byte-identical to the CI red
/// this wait was added to remove. A reader then cannot tell "the wait did not
/// help" from "`find_pid_on_port` returned the wrong PID", which is precisely
/// the ambiguity that made the original failure cost a rerun to diagnose.
/// Callers name the view in their failure message so the next red says which it
/// was.
async fn wait_until_probe_sees(port: u16, pid: u32) -> ProbeView {
    for _ in 0..POLL_ATTEMPTS {
        match proc_kill::find_pids_on_port(port).await {
            Ok(found) if found == [pid] => return ProbeView::Child,
            Ok(found) => match found.into_iter().find(|&other| other != pid) {
                Some(other) => return ProbeView::Other(other),
                None => tokio::time::sleep(POLL_INTERVAL).await,
            },
            Err(e) => return ProbeView::Unavailable(e.to_string()),
        }
    }
    ProbeView::NeverSaw
}

/// kill_by_port must terminate the listener and free the port — the primitive
/// the reaper and reconcile sweep use to close the orphan leak.
#[tokio::test]
async fn kill_by_port_frees_a_real_held_port() {
    let port = KILL_BY_PORT_PORT;
    if is_port_listening(port) {
        skip(&format!("port {port} already in use (env not clean)"));
        return;
    }

    let Some(mut listener) = spawn_bound_listener(port).await else {
        return;
    };

    // kill_by_port kills every listener on the port, so it runs only once the
    // probe names this test's child as the listener: then the port's listener
    // is a process this test spawned. Waiting also clears the race
    // find_pid_on_port_locates_listener waits out, one layer down: a probe that
    // has not yet seen the socket would kill nothing and report `false`, which
    // the assertion below would read as a defect in the kill logic.
    let view = wait_until_probe_sees(port, listener.pid()).await;
    match &view {
        ProbeView::Child => {}
        ProbeView::Other(other) => {
            // Never kill by port while the probe names another process: that
            // kill would reach it. The Listener guard ends the child.
            skip(&format!(
                "port {port}'s listener reads as pid {other}, not this test's child {}; \
                 kill_by_port would reach a process this test did not spawn",
                listener.pid()
            ));
            return;
        }
        ProbeView::NeverSaw => panic!(
            "the lsof probe ran for the whole {POLL_ATTEMPTS}-attempt budget and never saw \
             the child (pid {}) its own word says is listening on port {port}: the probe is \
             blind or still lagging, so kill_by_port would find nothing to kill",
            listener.pid()
        ),
        ProbeView::Unavailable(e) => {
            // The `lsof` probe could not RUN at all, which is UNKNOWN, not a
            // defect in the kill logic under test — skip rather than assert
            // against a probe that never answered. The Listener guard kills and
            // reaps on the way out.
            skip(&format!("listener probe unavailable ({e})"));
            return;
        }
    }

    // Still reachable independently of the view above: `lsof` can start failing
    // between the two calls, and this is the call whose `Err` the test is
    // actually asserting against.
    let killed = match proc_kill::kill_by_port(port).await {
        Ok(killed) => killed,
        Err(e) => {
            skip(&format!("listener probe unavailable ({e})"));
            return;
        }
    };
    assert!(
        killed,
        "kill_by_port reported no kill for a held port (lsof view while waiting: {view:?})"
    );

    // Port must be free now — the whole point of D7.
    let freed = qontinui_supervisor::process::port::wait_for_port_free(port, 5).await;
    assert!(freed, "port {port} still in use after kill_by_port");

    // And the OS process must be gone (no orphan).
    listener.reap();
    assert!(
        !is_port_listening(port),
        "orphan still holding port {port} after kill"
    );

    // Last statement, after every assertion: reaching it is what the marker
    // asserts. Printed earlier it would claim coverage a later assertion had not
    // yet earned.
    exercised("kill_by_port");
}

/// kill_by_pid_tree on the tracked PID must terminate the process — the
/// atomic "kill the owned process as the record is removed" path in the
/// reaper.
#[tokio::test]
async fn kill_by_pid_tree_terminates_tracked_pid() {
    let port = KILL_BY_PID_TREE_PORT;
    if is_port_listening(port) {
        skip(&format!("port {port} already in use (env not clean)"));
        return;
    }

    let Some(listener) = spawn_bound_listener(port).await else {
        return;
    };
    let pid = listener.pid();

    // Deliberately probe-free: this test addresses the child by its own PID and
    // never consults `lsof`, so the unsynchronised-observer race the other two
    // tests wait out cannot reach it, and its kill reaches only the child.
    let killed = proc_kill::kill_by_pid_tree(pid)
        .await
        .expect("kill_by_pid_tree");
    assert!(killed, "kill_by_pid_tree reported no kill for a live PID");

    let freed = qontinui_supervisor::process::port::wait_for_port_free(port, 5).await;
    assert!(freed, "port {port} still held after kill_by_pid_tree(pid)");

    // The Listener guard reaps the killed child, so no zombie lingers.
    exercised("kill_by_pid_tree");
}

/// find_pid_on_port must locate the real listener so the reconcile sweep can
/// identify the orphan to kill.
#[tokio::test]
async fn find_pid_on_port_locates_listener() {
    let port = FIND_PID_ON_PORT_PORT;
    if is_port_listening(port) {
        skip(&format!("port {port} already in use (env not clean)"));
        return;
    }

    let Some(listener) = spawn_bound_listener(port).await else {
        return;
    };
    let expected_pid = listener.pid();

    // Let the lsof view catch up with the bind view before asserting; see
    // wait_until_probe_sees. Without this the assertion below races the two
    // probes and fails with `left: None` on a perfectly good listener.
    let view = wait_until_probe_sees(port, expected_pid).await;
    if let ProbeView::Unavailable(e) = &view {
        // Three-state: the probe (lsof) could not run at all — that is UNKNOWN,
        // not "nothing was listening", so it must not be asserted against as a
        // wrong PID. Skip rather than fail a box with no lsof.
        skip(&format!("listener probe unavailable ({e})"));
        return;
    }

    match proc_kill::find_pid_on_port(port).await {
        Ok(found) => {
            assert_eq!(
                found,
                Some(expected_pid),
                "find_pid_on_port did not return the listener PID \
                 (lsof view while waiting: {view:?} — `NeverSaw` means the probe ran for the \
                 whole {POLL_ATTEMPTS}-attempt budget and never saw a listener the child \
                 says it is, i.e. the probe is blind or still lagging, NOT find_pid_on_port \
                 returning a wrong PID)"
            );
            // Only inside the `Ok` arm, and only after the assertion: the `Err`
            // arm below is a skip, and a marker printed outside this match would
            // claim the probe answered when it had not.
            exercised("find_pid_on_port");
        }
        Err(e) => skip(&format!("listener probe unavailable ({e})")),
    }

    // Cleanup is the Listener guard's Drop — it kills and reaps on every exit
    // path, including the assertion panic above.
}

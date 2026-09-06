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

/// A spawned listener child, OWNED so it cannot outlive the test.
///
/// Every assertion in this file panics on failure, and a panic unwinds straight
/// past any hand-written `child.wait()`. Without this guard a failed test leaks
/// a live process holding a port in the temp-runner range (9877-9899) — and the
/// reconcile sweep deliberately SPARES a listener it cannot match to its own
/// spawn ledger (`process::temp_runner_ledger`), so nothing on the box will ever
/// clean it up: the leak squats a spawn-test port until reboot. A red test
/// becoming a *destructive* event is what makes this load-bearing rather than
/// tidiness.
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
/// `None` means **the fixture could not be established**, which is UNKNOWN about
/// the primitives under test rather than a verdict on them. That is the same
/// three-state discipline this file already applies to the `lsof` probe (`Err`
/// is not `Ok(None)`) and to port occupancy, extended to the one remaining place
/// that hard-failed on a purely environmental fact. Two causes qualify, and both
/// are reported on stderr rather than skipped in silence:
///
/// * `python3` could not be spawned at all — absent, or the stock-macOS
///   command-line-tools stub;
/// * the child EXITED before binding, i.e. the interpreter ran and the fixture
///   is unusable.
///
/// A child that is still ALIVE and never bound within the budget is **not**
/// environmental — the interpreter is running and the socket never appeared — so
/// that case still panics.
async fn spawn_bound_listener(port: u16) -> Option<Listener> {
    let script = format!(
        "import socket,time\n\
         s=socket.socket(socket.AF_INET,socket.SOCK_STREAM)\n\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n\
         s.bind(('127.0.0.1',{port}))\n\
         s.listen(8)\n\
         while True: time.sleep(1)\n"
    );
    let child = match std::process::Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("python3 could not be spawned ({e}); skipping (fixture unavailable)");
            return None;
        }
    };
    let mut listener = Listener { child };

    for _ in 0..POLL_ATTEMPTS {
        if is_port_listening(port) {
            return Some(listener);
        }
        if let Ok(Some(status)) = listener.child.try_wait() {
            eprintln!(
                "python3 listener exited with {status} before binding port {port}; \
                 skipping (fixture unusable)"
            );
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    panic!(
        "listener (pid {}) is still alive but never bound port {port} within the budget",
        listener.pid()
    );
}

/// What the `lsof`-backed probe was observed doing while we waited for it.
#[derive(Debug)]
enum ProbeView {
    /// The probe positively saw a listener on the port.
    Saw,
    /// The probe RAN for the whole budget and never saw the listener.
    NeverSaw,
    /// The probe could not RUN at all — UNKNOWN, never "nothing is listening".
    Unavailable(String),
}

/// Block until the `lsof`-backed probe can SEE the listener, the probe errors,
/// or the shared budget is spent.
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
async fn wait_until_probe_sees(port: u16) -> ProbeView {
    for _ in 0..POLL_ATTEMPTS {
        match proc_kill::find_pid_on_port(port).await {
            Ok(Some(_)) => return ProbeView::Saw,
            Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(e) => return ProbeView::Unavailable(e.to_string()),
        }
    }
    ProbeView::NeverSaw
}

/// kill_by_port must terminate the listener and free the port — the primitive
/// the reaper and reconcile sweep use to close the orphan leak.
#[tokio::test]
async fn kill_by_port_frees_a_real_held_port() {
    // A temp-runner-range port unlikely to collide with the live supervisor's
    // runners (9877 is the bottom of the spawn range but this test runs in
    // isolation; pick a high one in-range to reduce collision odds).
    let port: u16 = 9898;
    if is_port_listening(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let Some(mut listener) = spawn_bound_listener(port).await else {
        return;
    };

    // Same race as in find_pid_on_port_locates_listener, one layer down:
    // kill_by_port iterates `find_pids_on_port`, so a probe that has not yet
    // seen the socket kills nothing and reports `false` — which the assertion
    // below would read as a defect in the kill logic. Wait for the probe view
    // first; this test was not observed failing, but it is the same defect and
    // fixing only the one that happened to fire would be half a fix.
    let view = wait_until_probe_sees(port).await;
    if let ProbeView::Unavailable(e) = &view {
        // The `lsof` probe could not RUN at all, which is UNKNOWN, not a defect
        // in the kill logic under test — skip rather than assert against a probe
        // that never answered. The Listener guard kills and reaps on the way out.
        eprintln!("listener probe unavailable ({e}); skipping");
        return;
    }

    // Still reachable independently of the view above: `lsof` can start failing
    // between the two calls, and this is the call whose `Err` the test is
    // actually asserting against.
    let killed = match proc_kill::kill_by_port(port).await {
        Ok(killed) => killed,
        Err(e) => {
            eprintln!("listener probe unavailable ({e}); skipping");
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
}

/// kill_by_pid_tree on the tracked PID must terminate the process — the
/// atomic "kill the owned process as the record is removed" path in the
/// reaper.
#[tokio::test]
async fn kill_by_pid_tree_terminates_tracked_pid() {
    let port: u16 = 9897;
    if is_port_listening(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let Some(listener) = spawn_bound_listener(port).await else {
        return;
    };
    let pid = listener.pid();

    // Deliberately probe-free: this test addresses the child by its own PID and
    // never consults `lsof`, so the unsynchronised-observer race the other two
    // tests wait out cannot reach it.
    let killed = proc_kill::kill_by_pid_tree(pid)
        .await
        .expect("kill_by_pid_tree");
    assert!(killed, "kill_by_pid_tree reported no kill for a live PID");

    let freed = qontinui_supervisor::process::port::wait_for_port_free(port, 5).await;
    assert!(freed, "port {port} still held after kill_by_pid_tree(pid)");

    // The Listener guard reaps the killed child, so no zombie lingers.
}

/// find_pid_on_port must locate the real listener so the reconcile sweep can
/// identify the orphan to kill.
#[tokio::test]
async fn find_pid_on_port_locates_listener() {
    let port: u16 = 9896;
    if is_port_listening(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let Some(listener) = spawn_bound_listener(port).await else {
        return;
    };
    let expected_pid = listener.pid();

    // Let the lsof view catch up with the bind view before asserting; see
    // wait_until_probe_sees. Without this the assertion below races the two
    // probes and fails with `left: None` on a perfectly good listener.
    let view = wait_until_probe_sees(port).await;
    if let ProbeView::Unavailable(e) = &view {
        // Three-state: the probe (lsof) could not run at all — that is UNKNOWN,
        // not "nothing was listening", so it must not be asserted against as a
        // wrong PID. Skip rather than fail a box with no lsof.
        eprintln!("listener probe unavailable ({e}); skipping");
        return;
    }

    match proc_kill::find_pid_on_port(port).await {
        Ok(found) => assert_eq!(
            found,
            Some(expected_pid),
            "find_pid_on_port did not return the listener PID \
             (lsof view while waiting: {view:?} — `NeverSaw` means the probe ran for the \
             whole {POLL_ATTEMPTS}-attempt budget and never saw a listener the bind probe \
             says is up, i.e. the probe is blind or still lagging, NOT find_pid_on_port \
             returning a wrong PID)"
        ),
        Err(e) => eprintln!("listener probe unavailable ({e}); skipping"),
    }

    // Cleanup is the Listener guard's Drop — it kills and reaps on every exit
    // path, including the assertion panic above.
}

//! Verification for the D7 orphan-leak fix on Unix: the cross-platform
//! process-kill primitives must actually terminate a real process holding a
//! port and free that port — the behavior the reaper now relies on so it
//! never drops a temp-runner record while its OS process stays alive.
//!
//! Windows uses a different (netstat/taskkill) implementation tested
//! elsewhere; this file is Unix-only.
#![cfg(not(target_os = "windows"))]

use std::process::Stdio;
use std::time::Duration;

use qontinui_supervisor::process::port::is_port_in_use;
use qontinui_supervisor::process::proc_kill;

/// Spawn a real child process that binds and LISTENs on `port` until killed.
/// We use Python (always present on macOS/most Linux CI) so the child is a
/// genuine OS process with its own PID and a held TCP listener — exactly the
/// orphan shape D7 describes.
fn spawn_listener(port: u16) -> std::process::Child {
    let script = format!(
        "import socket,time\n\
         s=socket.socket(socket.AF_INET,socket.SOCK_STREAM)\n\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n\
         s.bind(('127.0.0.1',{port}))\n\
         s.listen(8)\n\
         while True: time.sleep(1)\n"
    );
    std::process::Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3 listener")
}

/// Block until `port` is in use or we give up. Mirrors the supervisor's own
/// readiness assumption.
async fn wait_until_listening(port: u16) -> bool {
    for _ in 0..50 {
        if is_port_in_use(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// kill_by_port must terminate the listener and free the port — the primitive
/// the reaper and reconcile sweep use to close the orphan leak.
#[tokio::test]
async fn kill_by_port_frees_a_real_held_port() {
    // A temp-runner-range port unlikely to collide with the live supervisor's
    // runners (9877 is the bottom of the spawn range but this test runs in
    // isolation; pick a high one in-range to reduce collision odds).
    let port: u16 = 9898;
    if is_port_in_use(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let mut child = spawn_listener(port);
    assert!(
        wait_until_listening(port).await,
        "listener never bound port {port}"
    );

    // The actual fix path.
    let killed = proc_kill::kill_by_port(port).await.expect("kill_by_port");
    assert!(killed, "kill_by_port reported no kill for a held port");

    // Port must be free now — the whole point of D7.
    let freed = qontinui_supervisor::process::port::wait_for_port_free(port, 5).await;
    assert!(freed, "port {port} still in use after kill_by_port");

    // And the OS process must be gone (no orphan).
    let _ = child.wait();
    assert!(
        !is_port_in_use(port),
        "orphan still holding port {port} after kill"
    );
}

/// kill_by_pid_tree on the tracked PID must terminate the process — the
/// atomic "kill the owned process as the record is removed" path in the
/// reaper.
#[tokio::test]
async fn kill_by_pid_tree_terminates_tracked_pid() {
    let port: u16 = 9897;
    if is_port_in_use(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let mut child = spawn_listener(port);
    let pid = child.id();
    assert!(
        wait_until_listening(port).await,
        "listener never bound port {port}"
    );

    let killed = proc_kill::kill_by_pid_tree(pid)
        .await
        .expect("kill_by_pid_tree");
    assert!(killed, "kill_by_pid_tree reported no kill for a live PID");

    let freed = qontinui_supervisor::process::port::wait_for_port_free(port, 5).await;
    assert!(freed, "port {port} still held after kill_by_pid_tree(pid)");

    // Reap the killed child so no zombie lingers.
    let _ = child.wait();
}

/// find_pid_on_port must locate the real listener so the reconcile sweep can
/// identify the orphan to kill.
#[tokio::test]
async fn find_pid_on_port_locates_listener() {
    let port: u16 = 9896;
    if is_port_in_use(port) {
        eprintln!("port {port} already in use; skipping (env not clean)");
        return;
    }

    let mut child = spawn_listener(port);
    let expected_pid = child.id();
    assert!(
        wait_until_listening(port).await,
        "listener never bound port {port}"
    );

    let found = proc_kill::find_pid_on_port(port).await;
    assert_eq!(
        found,
        Some(expected_pid),
        "find_pid_on_port did not return the listener PID"
    );

    // Cleanup.
    let _ = proc_kill::kill_by_pid_tree(expected_pid).await;
    let _ = child.wait();
}

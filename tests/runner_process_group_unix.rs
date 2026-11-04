//! A runner spawned the way the supervisor's spawn site now spawns it —
//! `process_group(0)`, so the runner leads its own group — is ended WITH its
//! descendants by `proc_kill::kill_by_pid_tree` (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1, test (e)).
//!
//! Before that phase the Unix spawn site set no process group, so
//! `kill_by_pid_tree`'s `kill(-pid, …)` addressed a group that did not exist
//! and a temp runner's children (panes, helpers) outlived the sweep. The
//! dummy here is a `sh` that starts a background grandchild `sleep` and then
//! waits on it: the grandchild survives a kill of the bare pid, and dies only
//! when the group is signalled.
//!
//! Spawns only its own dummy processes, in their own group; touches no port.
#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use qontinui_supervisor::process::proc_kill;
use qontinui_supervisor::process::unix_kill::lead_own_process_group;

fn alive(pid: u32) -> bool {
    // A zombie still answers kill(0); read its state so a reaped-pending
    // process does not count as alive.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            // The state is the field after the `)` closing the comm.
            let state = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .unwrap_or("");
            state != "Z" && state != "X"
        }
        // No procfs entry: gone (or not Linux — fall back to kill(0)).
        Err(_) => {
            // SAFETY: signal 0 only probes existence.
            unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
        }
    }
}

fn wait_gone(pid: u32, bound: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < bound {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

#[tokio::test]
async fn kill_by_pid_tree_ends_a_process_group_leader_and_its_grandchild() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let grandchild_pid_file = tmp.path().join("grandchild.pid");

    // The runner stand-in, built with the SAME helper the runner spawn site
    // calls (`lead_own_process_group`): it forks a grandchild into its group,
    // records the grandchild's pid, and waits on it.
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!(
            "sleep 300 & echo $! > '{}'; wait",
            grandchild_pid_file.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    lead_own_process_group(&mut cmd);
    let mut child = cmd.spawn().expect("spawn the dummy runner");
    let leader = child.id().expect("the dummy has a pid");

    // Wait for the grandchild's pid to be written.
    let start = Instant::now();
    let grandchild: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&grandchild_pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the dummy never recorded its grandchild"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        alive(grandchild),
        "the grandchild must be running before the kill"
    );

    // Precondition the test depends on: the leader really leads a group of its
    // own (the property `process_group(0)` buys at the spawn site).
    // SAFETY: getpgid takes a scalar pid.
    let pgid = unsafe { libc::getpgid(leader as libc::pid_t) };
    if pgid as u32 != leader {
        // Never leak the dummy, even on a red.
        // SAFETY: scalar arguments; the pids are our own dummies.
        unsafe {
            libc::kill(grandchild as libc::pid_t, libc::SIGKILL);
            libc::kill(leader as libc::pid_t, libc::SIGKILL);
        }
        panic!(
            "lead_own_process_group must make the spawned runner its own group leader \
             (pgid {pgid}, pid {leader}) — without it `kill_by_pid_tree`'s kill(-pid) reaches \
             no group"
        );
    }

    let delivered = proc_kill::kill_by_pid_tree(leader)
        .await
        .expect("kill_by_pid_tree must not error");
    assert!(
        delivered,
        "kill_by_pid_tree must report it signalled a live process"
    );

    // Reap the leader (our direct child) so it does not linger as a zombie.
    let reaped = {
        let start = Instant::now();
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break true;
            }
            if start.elapsed() > Duration::from_secs(5) {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let grandchild_gone = wait_gone(grandchild, Duration::from_secs(5));
    if !grandchild_gone {
        // Never leak the dummy, even on a red.
        // SAFETY: scalar arguments; the pid was recorded by our own dummy.
        unsafe {
            libc::kill(grandchild as libc::pid_t, libc::SIGKILL);
        }
    }
    if !reaped {
        let _ = child.kill().await;
    }
    assert!(reaped, "the group leader (the runner) must be ended");
    assert!(
        grandchild_gone,
        "the grandchild must be ended with its group — a kill of the bare pid would leave it \
         running, which is the pre-fix behaviour"
    );
}

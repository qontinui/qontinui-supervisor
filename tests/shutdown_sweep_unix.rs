//! The off-Windows shutdown sweep's KILL phase against real process groups
//! (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1): `kill_victims` starts its own deadline, so even a victim that
//! ignores SIGTERM gets the SIGKILL that follows the grace inside the bound;
//! and a victim whose leader is gone still has its group signalled.
//!
//! In its own test binary on purpose: these tests fork, and in the library's
//! unit-test binary a fork can make a sibling test's exec fail with ETXTBSY.
//! Spawns only its own `sleep`/`sh` dummies and touches no port or unit.
//! Linux only: it reads `/proc/<pid>/{comm,stat}` to observe the dummies.
#![cfg(target_os = "linux")]

use std::time::Duration;

use qontinui_supervisor::process::shutdown_sweep::{
    kill_victims, KillOutcome, SweepVictim, SHUTDOWN_SWEEP_BOUND,
};
use qontinui_supervisor::process::unix_kill::lead_own_process_group;

/// A dummy runner leading its own process group, reaped by a task so its exit
/// is observable the way the supervisor's exit monitor observes a runner's.
fn dummy(script: &str) -> (u32, tokio::task::JoinHandle<std::process::ExitStatus>) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    let mut child = cmd.spawn().expect("spawn dummy");
    let pid = child.id().expect("pid");
    let waiter = tokio::spawn(async move { child.wait().await.expect("wait") });
    (pid, waiter)
}

/// Wait until `pid` has exec'd `sleep` (its `comm`), bounded.
async fn until_sleeping(pid: u32) {
    let started = std::time::Instant::now();
    while std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim() != "sleep")
        .unwrap_or(true)
    {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "dummy {pid} never exec'd sleep"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether `pid` is gone or a zombie — dead, whoever reaps it.
fn dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
    }
}

fn victim(id: &str, pid: u32) -> SweepVictim {
    SweepVictim {
        id: id.into(),
        port: 9899,
        pid,
    }
}

#[tokio::test]
async fn the_kill_phase_ends_an_ordinary_and_a_sigterm_ignoring_runner_inside_its_bound() {
    let (plain, plain_exit) = dummy("exec sleep 30");
    // Ignores SIGTERM, and so does the `sleep` it execs (an ignored disposition
    // survives exec): only the SIGKILL after the grace can end it.
    let (stubborn, stubborn_exit) = dummy("trap '' TERM; exec sleep 30");
    until_sleeping(plain).await;
    until_sleeping(stubborn).await;

    let started = std::time::Instant::now();
    let outcomes = kill_victims(
        &[
            victim("test-plain", plain),
            victim("test-stubborn", stubborn),
        ],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert!(
        started.elapsed() <= SHUTDOWN_SWEEP_BOUND + Duration::from_millis(500),
        "the kill phase must finish inside its bound: {:?}",
        started.elapsed()
    );
    // The plain one exits inside the SIGTERM grace and is reaped by its waiter
    // before `kill_by_pid_tree` returns, so it is observed stopped.
    assert_eq!(outcomes[0].1, KillOutcome::Stopped, "{outcomes:?}");
    assert!(
        matches!(
            outcomes[1].1,
            KillOutcome::Stopped | KillOutcome::SignalledNotObserved
        ),
        "the stubborn one is SIGKILLed after the grace, never cut off: {outcomes:?}"
    );

    for (name, exit) in [("plain", plain_exit), ("stubborn", stubborn_exit)] {
        let status = tokio::time::timeout(Duration::from_secs(2), exit)
            .await
            .unwrap_or_else(|_| panic!("the {name} runner must be dead after the kill phase"))
            .expect("waiter");
        assert!(!status.success(), "{name}: {status}");
    }
}

/// A bound shorter than the SIGTERM grace cuts a SIGTERM-ignoring victim off,
/// and says so — never "stopped".
#[tokio::test]
async fn a_victim_the_bound_cuts_off_is_reported_cut_off() {
    let (stubborn, _exit) = dummy("trap '' TERM; exec sleep 30");
    until_sleeping(stubborn).await;
    let outcomes = kill_victims(
        &[victim("test-cut-off", stubborn)],
        Duration::from_millis(200),
    )
    .await;
    assert_eq!(outcomes[0].1, KillOutcome::CutOff);
    // Its waiter task owns the `Child`; the test runtime's shutdown drops it,
    // and `kill_on_drop` ends the dummy then.
}

/// A leader gone and nothing left in its group: reported already gone.
#[tokio::test]
async fn a_victim_already_gone_is_reported_as_already_gone() {
    let (pid, exit) = dummy("exit 0");
    exit.await.expect("the dummy exits and is reaped");
    let outcomes = kill_victims(&[victim("test-gone", pid)], SHUTDOWN_SWEEP_BOUND).await;
    assert_eq!(outcomes[0].1, KillOutcome::AlreadyGone);
}

/// A leader gone but a child left in its group: the group is still signalled,
/// so the child dies too.
#[tokio::test]
async fn a_victim_whose_leader_is_gone_still_has_its_group_signalled() {
    let mut cmd = tokio::process::Command::new("sh");
    // The background `sleep` must not hold the leader's stdout, or reading it to
    // EOF would wait out the sleep.
    cmd.args([
        "-c",
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $!; exit 0",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    let child = cmd.spawn().expect("spawn the leader");
    let leader = child.id().expect("pid");
    let out = child.wait_with_output().await.expect("the leader exits");
    let orphan: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the leader printed its child's pid");
    until_sleeping(orphan).await;
    assert!(!dead(orphan), "the group's child outlives its leader");

    let outcomes = kill_victims(
        &[victim("test-orphaned-group", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert_ne!(
        outcomes[0].1,
        KillOutcome::AlreadyGone,
        "a group with a live member is not 'already gone'"
    );
    let started = std::time::Instant::now();
    while !dead(orphan) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the group's surviving child {orphan} must be signalled and die"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A pid still live after the kill — here a zombie nobody reaps yet — is
/// reported as signalled but not observed gone, never "stopped".
#[tokio::test]
async fn a_victim_still_live_after_the_kill_is_not_reported_stopped() {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", "exec sleep 30"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    // Held but never awaited: once it dies it stays a zombie, which
    // `kill(pid, 0)` still answers for.
    let child = cmd.spawn().expect("spawn dummy");
    let pid = child.id().expect("pid");
    until_sleeping(pid).await;
    let outcomes = kill_victims(&[victim("test-unreaped", pid)], SHUTDOWN_SWEEP_BOUND).await;
    assert_eq!(outcomes[0].1, KillOutcome::SignalledNotObserved);
    assert!(dead(pid), "it was killed all the same");
    drop(child);
}

/// A group member that ignores SIGTERM, whose leader had already exited, is
/// SIGKILLed after the grace — and only then reported stopped.
#[tokio::test]
async fn a_sigterm_ignoring_orphan_in_the_group_is_sigkilled_too() {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args([
        "-c",
        "(trap '' TERM; exec sleep 30) </dev/null >/dev/null 2>&1 & echo $!; exit 0",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    let child = cmd.spawn().expect("spawn the leader");
    let leader = child.id().expect("pid");
    let out = child.wait_with_output().await.expect("the leader exits");
    let orphan: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the leader printed its child's pid");
    until_sleeping(orphan).await;

    let started = std::time::Instant::now();
    let outcomes = kill_victims(
        &[victim("test-stubborn-orphan", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    while !dead(orphan) {
        assert!(
            started.elapsed() < SHUTDOWN_SWEEP_BOUND + Duration::from_millis(500),
            "the SIGTERM-ignoring member {orphan} must be SIGKILLed inside the bound"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        matches!(
            outcomes[0].1,
            KillOutcome::Stopped | KillOutcome::SignalledNotObserved
        ),
        "{outcomes:?}"
    );
}

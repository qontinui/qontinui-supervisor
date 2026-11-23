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

/// Wait until `path` exists, bounded.
async fn until_exists(path: &std::path::Path) {
    let started = std::time::Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A sleeping member that joins `leader`'s process group, held but never
/// awaited: once killed it stays a zombie IN the group.
async fn zombie_to_be_in_group(leader: u32) -> (tokio::process::Child, u32) {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .process_group(leader as i32);
    let member = cmd
        .spawn()
        .expect("spawn the member into the leader's group");
    let pid = member.id().expect("pid");
    until_sleeping(pid).await;
    (member, pid)
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
    // The stubborn one is SIGKILLed after the grace and reaped by its waiter
    // inside the rest of the bound, so it too is observed stopped.
    assert_eq!(outcomes[1].1, KillOutcome::Stopped, "{outcomes:?}");

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
    assert!(
        matches!(
            outcomes[0].1,
            KillOutcome::GroupStopped | KillOutcome::SignalledNotObserved
        ),
        "a group with a live member is neither 'already gone' nor a live leader's stop: {outcomes:?}"
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
            KillOutcome::GroupStopped | KillOutcome::SignalledNotObserved
        ),
        "{outcomes:?}"
    );
}

/// A group member whose leader had already exited still gets the SIGTERM
/// GRACE before any SIGKILL: here it takes 0.8 s to shut down, then leaves a
/// marker — which only exists if nothing killed it first.
#[tokio::test]
async fn a_leaderless_group_member_gets_its_sigterm_grace_before_sigkill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("shut-down-cleanly");
    let ready = dir.path().join("trap-installed");
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args([
        "-c",
        "(trap 'sleep 0.8; touch \"$MARK\"; exit 0' TERM; touch \"$READY\"; i=0; while [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done) \
         </dev/null >/dev/null 2>&1 & echo $!; exit 0",
    ])
    .env("MARK", &mark)
    .env("READY", &ready)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    let child = cmd.spawn().expect("spawn the leader");
    let leader = child.id().expect("pid");
    let out = child.wait_with_output().await.expect("the leader exits");
    let member: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the leader printed its child's pid");
    // The subshell says when its trap is in place; its loop is bounded
    // (~30 s), so a failed run leaves nothing behind for long.
    until_exists(&ready).await;
    assert!(!dead(member), "the member outlives its leader");

    let outcomes = kill_victims(
        &[victim("test-graceful-member", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert!(
        mark.exists(),
        "the member was SIGKILLed inside its SIGTERM grace (no marker): {outcomes:?}"
    );
}

/// Something of the group still live when the kill phase returns — here a
/// member left an unreaped zombie — is SignalledNotObserved, never Stopped.
#[tokio::test]
async fn a_group_member_still_live_after_the_kill_is_not_reported_stopped() {
    let (leader, _leader_exit) = dummy("exec sleep 30");
    until_sleeping(leader).await;
    let mut member_cmd = tokio::process::Command::new("sleep");
    member_cmd
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .process_group(leader as i32);
    // Held but never awaited: once killed it stays a zombie IN the group.
    let member = member_cmd
        .spawn()
        .expect("spawn the member into the leader's group");
    let member_pid = member.id().expect("pid");
    until_sleeping(member_pid).await;

    let outcomes = kill_victims(
        &[victim("test-zombie-member", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert_eq!(
        outcomes[0].1,
        KillOutcome::SignalledNotObserved,
        "{outcomes:?}"
    );
    assert!(dead(member_pid), "the member was killed all the same");
    drop(member);
}

/// The ordinary case: a LIVE leader exits on SIGTERM at once, and a member of
/// its group that takes 0.8 s to shut down still gets the grace — nothing
/// SIGKILLs it the moment the leader is gone.
#[tokio::test]
async fn a_live_leaders_group_member_gets_its_sigterm_grace_before_sigkill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("shut-down-cleanly");
    let ready = dir.path().join("trap-installed");
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args([
        "-c",
        "(trap 'sleep 0.8; touch \"$MARK\"; exit 0' TERM; touch \"$READY\"; i=0; while [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done) \
         </dev/null >/dev/null 2>&1 & exec sleep 30",
    ])
    .env("MARK", &mark)
    .env("READY", &ready)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    let mut child = cmd.spawn().expect("spawn the leader");
    let leader = child.id().expect("pid");
    let _leader_exit = tokio::spawn(async move { child.wait().await.expect("wait") });
    until_sleeping(leader).await;
    until_exists(&ready).await;

    let outcomes = kill_victims(
        &[victim("test-live-leader-graceful-member", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert!(
        mark.exists(),
        "the member was SIGKILLed inside its SIGTERM grace once the leader was gone (no \
         marker): {outcomes:?}"
    );
}

/// A leaderless group that is still live when the kill phase returns — its
/// member left an unreaped zombie — is SignalledNotObserved, never
/// GroupStopped.
#[tokio::test]
async fn a_leaderless_group_still_live_after_the_kill_is_not_reported_group_stopped() {
    let (leader, leader_exit) = dummy("exec sleep 30");
    until_sleeping(leader).await;
    let (member, member_pid) = zombie_to_be_in_group(leader).await;
    // Only the member is left in the group once the leader is reaped.
    // SAFETY: scalar arguments; `leader` is our own live child.
    assert_eq!(
        unsafe { libc::kill(leader as libc::pid_t, libc::SIGKILL) },
        0
    );
    leader_exit.await.expect("the leader is reaped");

    let outcomes = kill_victims(
        &[victim("test-leaderless-zombie", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert_eq!(
        outcomes[0].1,
        KillOutcome::SignalledNotObserved,
        "{outcomes:?}"
    );
    assert!(dead(member_pid), "the member was killed all the same");
    drop(member);
}

/// The grace is counted from the FIRST signal: a SIGTERM-ignoring leader
/// whose group never empties (a zombie member) is SIGKILLed after 1.5 s and
/// the phase still returns inside its 2 s bound — SignalledNotObserved, not
/// CutOff.
#[tokio::test]
async fn the_group_grace_runs_from_the_first_signal_not_after_the_leaders_grace() {
    let (leader, _leader_exit) = dummy("trap '' TERM; exec sleep 30");
    until_sleeping(leader).await;
    let (member, member_pid) = zombie_to_be_in_group(leader).await;

    let outcomes = kill_victims(
        &[victim("test-stubborn-leader-zombie", leader)],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert_eq!(
        outcomes[0].1,
        KillOutcome::SignalledNotObserved,
        "{outcomes:?}"
    );
    assert!(
        dead(leader) && dead(member_pid),
        "both were killed all the same"
    );
    drop(member);
}

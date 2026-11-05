//! The off-Windows shutdown sweep's KILL phase against real process groups
//! (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1): `kill_victims` starts its own deadline, so even a victim that
//! ignores SIGTERM gets the SIGKILL that follows the grace inside the bound.
//!
//! In its own test binary on purpose: these tests fork, and in the library's
//! unit-test binary a fork can make a sibling test's exec fail with ETXTBSY.
//! Spawns only its own `sleep`/`sh` dummies and touches no port or unit.
//! Linux only: it reads `/proc/<pid>/exe` to know a dummy has exec'd.
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

#[tokio::test]
async fn the_kill_phase_ends_an_ordinary_and_a_sigterm_ignoring_runner_inside_its_bound() {
    let (plain, plain_exit) = dummy("exec sleep 30");
    // Ignores SIGTERM, and so does the `sleep` it execs (an ignored disposition
    // survives exec): only the SIGKILL after the grace can end it.
    let (stubborn, stubborn_exit) = dummy("trap '' TERM; exec sleep 30");
    // Both must have exec'd their `sleep` — the stubborn one only then
    // carries the ignored SIGTERM — before they are signalled.
    for pid in [plain, stubborn] {
        let started = std::time::Instant::now();
        while !std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|exe| exe.to_string_lossy().ends_with("sleep"))
            .unwrap_or(false)
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "dummy {pid} never exec'd sleep"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let victims = [
        SweepVictim {
            id: "test-plain".into(),
            port: 9898,
            pid: plain,
        },
        SweepVictim {
            id: "test-stubborn".into(),
            port: 9899,
            pid: stubborn,
        },
    ];

    let started = std::time::Instant::now();
    let outcomes = kill_victims(&victims, SHUTDOWN_SWEEP_BOUND).await;
    assert!(
        started.elapsed() <= SHUTDOWN_SWEEP_BOUND + Duration::from_millis(500),
        "the kill phase must finish inside its bound: {:?}",
        started.elapsed()
    );
    assert_eq!(outcomes.len(), 2);
    for (victim, outcome) in &outcomes {
        assert_ne!(
            *outcome,
            KillOutcome::CutOff,
            "'{}' must not be cut off by the bound",
            victim.id
        );
    }

    for (name, exit) in [("plain", plain_exit), ("stubborn", stubborn_exit)] {
        let status = tokio::time::timeout(Duration::from_secs(2), exit)
            .await
            .unwrap_or_else(|_| panic!("the {name} runner must be dead after the kill phase"))
            .expect("waiter");
        assert!(!status.success(), "{name}: {status}");
    }
}

#[tokio::test]
async fn a_victim_already_gone_is_reported_as_such_and_not_signalled() {
    let (pid, exit) = dummy("exit 0");
    exit.await.expect("the dummy exits and is reaped");
    let outcomes = kill_victims(
        &[SweepVictim {
            id: "test-gone".into(),
            port: 9897,
            pid,
        }],
        SHUTDOWN_SWEEP_BOUND,
    )
    .await;
    assert_eq!(outcomes[0].1, KillOutcome::AlreadyGone);
}

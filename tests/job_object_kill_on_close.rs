//! Kernel-level encoding of the 2026-07-27 incident.
//!
//! A process assigned to the supervisor's `RunnerJob` dies when the job
//! handle drops (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`); a process left
//! unassigned survives. That split IS the fix — `process::job::
//! should_assign_to_ephemeral_job` assigns temp runners only, so supervisor
//! exit reaps ephemerals and leaves the operator's primary and named runners
//! running.
//!
//! **Why this lives in its own integration-test binary rather than a
//! `mod tests` inside `src/process/job.rs`:** libtest runs unit tests of one
//! binary in parallel threads, and `src/process/guarded_command.rs` has
//! wall-clock tree-kill assertions in the same binary. Running this test
//! beside them made `AssignProcessToJobObject` fail with
//! `ERROR_ACCESS_DENIED` (5) — concurrent Win32 job manipulation in one
//! process — and inflated their timings past their budgets. Cargo runs test
//! *targets* one at a time, so a separate binary removes the interference
//! without weakening any assertion.
//!
//! The pure routing-predicate tests stay in `src/process/job.rs`; only this
//! process-spawning one moved.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use qontinui_supervisor::process::job::RunnerJob;

/// `try_wait()` polled until the child is reaped or the deadline passes.
/// Returns true if the child exited.
fn exited_within(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A leaf process that stays alive ~15s and spawns nothing. Invoked directly
/// rather than through `cmd.exe /c` so killing it leaves no orphaned
/// grandchild behind.
fn spawn_sleeper() -> Child {
    Command::new("ping.exe")
        .args(["-n", "15", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dummy child")
}

#[test]
fn dropping_the_job_kills_only_assigned_children() {
    let job = RunnerJob::create().expect("create job");
    let mut assigned = spawn_sleeper();
    let mut unassigned = spawn_sleeper();

    // Record the failure instead of asserting inline, so both children are
    // always cleaned up before the test unwinds.
    let assign_result = job.assign(assigned.id());
    let mut failure: Option<String> = None;
    match assign_result {
        Err(e) => failure = Some(format!("assign failed: {e}")),
        Ok(()) => {
            drop(job);
            if !exited_within(&mut assigned, Duration::from_secs(10)) {
                failure = Some("assigned child survived the job handle drop".to_string());
            } else if unassigned.try_wait().ok().flatten().is_some() {
                failure = Some("unassigned child was killed with the job".to_string());
            }
        }
    }

    let _ = assigned.kill();
    let _ = assigned.wait();
    let _ = unassigned.kill();
    let _ = unassigned.wait();

    if let Some(msg) = failure {
        panic!("{msg}");
    }
}

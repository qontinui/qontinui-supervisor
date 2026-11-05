//! The Linux scope-landing check against REAL child processes (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1b): a fake proc root decides what
//! `systemd_scope::await_scope_landing` SEES for a live dummy child, so each
//! failure shape — landed elsewhere, still pending at the bound, exited before
//! landing — is driven end to end, including the kill.
//!
//! In its own test binary on purpose: these tests fork. In the library's unit
//! test binary a fork can inherit another test's write descriptor on a stub
//! runner copy and make that test's exec fail with ETXTBSY (the
//! `past_text_file_busy` class in `process::manager::tests`); a separate binary
//! is a separate process and cannot.
//!
//! Spawns only its own `sleep`/`false` dummies and touches no port or unit.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use qontinui_supervisor::process::systemd_scope::{
    await_scope_landing, proc_state, reap_with_stderr_tail, LandingOutcome, STDERR_TAIL_BYTES,
};
use qontinui_supervisor::process::unix_kill::lead_own_process_group;

const UNIT: &str = "qontinui-runner-x-20261104T230000000Z.scope";
const SUPERVISOR_CGROUP: &str =
    "/user.slice/user-1000.slice/user@1000.service/app.slice/qontinui-supervisor-x.service";

/// Write a fake `<root>/<pid>/{exe,cgroup}`.
fn fake_proc(root: &Path, pid: u32, exe: &str, cgroup: &str) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).expect("fake pid dir");
    let link = dir.join("exe");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(exe, &link).expect("fake exe link");
    std::fs::write(dir.join("cgroup"), format!("0::{cgroup}\n")).expect("fake cgroup");
}

/// A real, live dummy child (its own process group, as at the spawn site)
/// standing in for the spawned pid; the fake proc root decides what the
/// landing check SEES for it.
fn dummy_child() -> tokio::process::Child {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    lead_own_process_group(&mut cmd);
    cmd.spawn().expect("spawn dummy")
}

#[tokio::test]
async fn landing_in_the_unit_is_scoped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = dummy_child();
    let pid = child.id().expect("pid");
    fake_proc(
        tmp.path(),
        pid,
        "/ws/target/debug/qontinui-runner-x",
        &format!("/app.slice/{UNIT}"),
    );
    let outcome = await_scope_landing(
        &mut child,
        tmp.path(),
        UNIT,
        Duration::from_secs(5),
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(outcome, LandingOutcome::Landed);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a landed runner must be left running"
    );
    let _ = child.kill().await;
}

#[tokio::test]
async fn landing_in_the_parents_cgroup_is_a_failed_start_and_kills_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = dummy_child();
    let pid = child.id().expect("pid");
    fake_proc(
        tmp.path(),
        pid,
        "/ws/target/debug/qontinui-runner-x",
        SUPERVISOR_CGROUP,
    );
    let outcome = await_scope_landing(
        &mut child,
        tmp.path(),
        UNIT,
        Duration::from_secs(5),
        Duration::from_millis(10),
    )
    .await;
    match outcome {
        LandingOutcome::Failed(reason) => {
            assert!(reason.contains(SUPERVISOR_CGROUP), "{reason}");
        }
        other => {
            panic!("a runner in the supervisor's cgroup must be a failed start: {other:?}")
        }
    }
    assert!(
        child.try_wait().expect("try_wait").is_some(),
        "a runner that landed outside its scope must be killed, not left running"
    );
}

/// `systemd-run` still pending at the bound: identity re-read, killed,
/// THEN reported failed — and nothing is retried.
#[tokio::test]
async fn still_pending_at_the_bound_is_killed_then_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = dummy_child();
    let pid = child.id().expect("pid");
    fake_proc(tmp.path(), pid, "/usr/bin/systemd-run", SUPERVISOR_CGROUP);
    let started = std::time::Instant::now();
    let outcome = await_scope_landing(
        &mut child,
        tmp.path(),
        UNIT,
        Duration::from_millis(300),
        Duration::from_millis(10),
    )
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "the bound must be waited out before giving up"
    );
    match outcome {
        LandingOutcome::Failed(reason) => {
            assert!(reason.contains("still pending"), "{reason}");
            assert!(reason.contains(&format!("killed PID {pid}")), "{reason}");
        }
        other => panic!("a pending scope start at the bound must fail: {other:?}"),
    }
    assert!(
        child.try_wait().expect("try_wait").is_some(),
        "the pending systemd-run must be killed BEFORE the failure is reported"
    );
}

/// The spawned process exits before it lands (`systemd-run` could not
/// start the scope): reported as exited-before-landing, naming both shapes,
/// and the child is left UNREAPED for the caller's exit handling. Read
/// through the REAL `/proc`, where an exited, unreaped child is a zombie.
#[tokio::test]
async fn exiting_before_landing_is_reported_and_left_unreaped() {
    let mut child = tokio::process::Command::new("false")
        .spawn()
        .expect("spawn false");
    let pid = child.id().expect("pid");
    // Let it exit first (a zombie until reaped): a check that catches it
    // still running reads `/usr/bin/false` outside the unit, which is the
    // Elsewhere arm, not this one.
    let started = std::time::Instant::now();
    while proc_state(Path::new("/proc"), pid) != Some('Z') {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`false` never exited"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let outcome = await_scope_landing(
        &mut child,
        Path::new("/proc"),
        UNIT,
        Duration::from_secs(5),
        Duration::from_millis(10),
    )
    .await;
    match &outcome {
        LandingOutcome::ExitedBeforeLanding(reason) => {
            assert!(reason.contains("could not start the scope"), "{reason}");
            assert!(reason.contains(&format!("PID {pid}")), "{reason}");
        }
        other => panic!("an exit before landing must be reported as such: {other:?}"),
    }
    assert_eq!(
        child.id(),
        Some(pid),
        "the landing check must not reap the child — the caller's exit handling owns it"
    );
    let status = child.wait().await.expect("wait");
    assert!(!status.success());
}

/// A spawned process that exited before it landed: the spawn site reaps it
/// and reports its exit status and the END of its stderr (where
/// `systemd-run`'s own error is), bounded to `STDERR_TAIL_BYTES`.
#[tokio::test]
async fn an_early_exit_is_reaped_with_its_status_and_the_tail_of_its_stderr() {
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", "echo boom >&2; exit 3"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let (status, tail) = reap_with_stderr_tail(&mut child).await;
    assert!(
        status.contains('3'),
        "the exit status names code 3: {status}"
    );
    assert_eq!(tail, "boom");

    // More than the tail bound: only the END is kept, so the last line — where
    // an error usually is — survives.
    let mut child = tokio::process::Command::new("sh")
        .args([
            "-c",
            "i=0; while [ $i -lt 400 ]; do echo xxxxxxxxxxxxxxxxxxxx >&2; i=$((i+1)); done; \
             echo LAST-LINE >&2; exit 1",
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let (_, tail) = reap_with_stderr_tail(&mut child).await;
    assert!(
        tail.len() <= STDERR_TAIL_BYTES,
        "the tail is bounded: {} bytes",
        tail.len()
    );
    assert!(
        tail.ends_with("LAST-LINE"),
        "the END is kept: …{}",
        &tail[tail.len().saturating_sub(40)..]
    );
}

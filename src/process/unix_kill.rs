//! Cross-platform (Unix) process-kill primitives mirroring the Windows
//! helpers in [`crate::process::windows`].
//!
//! The supervisor's kill machinery (`kill_by_pid`, `kill_by_port`,
//! `kill_by_pid_tree`, `find_pid_on_port`) was historically Windows-only —
//! every call site was gated behind `#[cfg(target_os = "windows")]` and the
//! non-Windows branch was a no-op `let _ = pid;`. On macOS/Linux that meant
//! the **reaper removed a temp runner's registry record while its OS process
//! stayed alive** (orphan leak D7): `POST /runners/<id>/stop` then 404'd
//! ("Runner not found") while the runner kept serving its port.
//!
//! This module provides the missing Unix primitives so the reaper, stop
//! path, and first-healthy watchdog terminate the owned process *as* (or
//! before) the record is dropped, and so a reconcile sweep can kill an
//! already-orphaned `test-*` process whose record is already gone.
//!
//! Killing strategy:
//! - PID lookup on a port uses `lsof -t -i :<port> -sTCP:LISTEN` (present on
//!   stock macOS and most Linux distros). If `lsof` is missing the helper
//!   returns "nothing found" and the caller falls back to the port-free
//!   confirmation loop — never a panic.
//! - The kill itself uses `libc::kill` with `SIGTERM` first, then escalates
//!   to `SIGKILL` if the PID is still alive after a short grace window.
//! - Tree-kill targets the process *group* (`kill(-pgid, ...)`); runners are
//!   spawned in their own process group (process-wrap sets `SETSID`/group on
//!   Unix), so killing the group reaps child panes/helpers that would
//!   otherwise keep the port held.

#![cfg(not(target_os = "windows"))]

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info};

/// Poll interval while waiting for a SIGTERM'd process to exit before we
/// escalate to SIGKILL.
const TERM_POLL_MS: u64 = 100;
/// Total grace given to SIGTERM before escalating to SIGKILL.
const TERM_GRACE_MS: u64 = 1500;

/// Return true if `pid` refers to a live process. Uses `kill(pid, 0)` which
/// performs the permission/existence check without sending a signal.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 only probes for existence/permission and
    // never delivers a signal. `pid` is a plain integer; no memory is touched.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // errno == EPERM means the process exists but we can't signal it — still
    // "alive" for our purposes. ESRCH means no such process.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Send `signal` to `pid`. Returns true if the signal was delivered (or the
/// process exists but we lacked permission), false if there is no such
/// process.
fn raw_kill(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: `kill` is async-signal-safe and takes only scalar arguments.
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Send `signal` to the entire process group led by `pgid` (negative-pid
/// form of `kill`).
fn raw_kill_group(pgid: u32, signal: libc::c_int) -> bool {
    // SAFETY: negative pid targets the process group; scalar args only.
    let rc = unsafe { libc::kill(-(pgid as libc::pid_t), signal) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Kill a single process by PID: SIGTERM, wait up to [`TERM_GRACE_MS`], then
/// SIGKILL if it is still alive. Returns true if we delivered a signal to a
/// process that existed; false if the PID was already gone.
pub async fn kill_by_pid(pid: u32) -> anyhow::Result<bool> {
    if !pid_alive(pid) {
        debug!("kill_by_pid: PID {} already gone", pid);
        return Ok(false);
    }

    raw_kill(pid, libc::SIGTERM);

    let deadline = std::time::Instant::now() + Duration::from_millis(TERM_GRACE_MS);
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            info!("kill_by_pid: PID {} exited after SIGTERM", pid);
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(TERM_POLL_MS)).await;
    }

    // Still alive — escalate.
    raw_kill(pid, libc::SIGKILL);
    info!("kill_by_pid: SIGKILL'd PID {} after grace window", pid);
    Ok(true)
}

/// Kill a PID **and its entire process group** (mirrors Windows
/// `kill_by_pid_tree`'s `/F /T`). Runners are launched in their own process
/// group, so the group id equals the runner's PID; signalling the group
/// reaps child panes/helpers that keep the port held. Falls back to a plain
/// per-PID kill if the group signal finds nothing.
pub async fn kill_by_pid_tree(pid: u32) -> anyhow::Result<bool> {
    let group_hit = raw_kill_group(pid, libc::SIGTERM);

    let deadline = std::time::Instant::now() + Duration::from_millis(TERM_GRACE_MS);
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            info!(
                "kill_by_pid_tree: process group {} exited after SIGTERM",
                pid
            );
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(TERM_POLL_MS)).await;
    }

    // Escalate to SIGKILL on the group, then the bare PID as a backstop.
    raw_kill_group(pid, libc::SIGKILL);
    let pid_hit = raw_kill(pid, libc::SIGKILL);
    info!(
        "kill_by_pid_tree: SIGKILL'd process group {} after grace window",
        pid
    );
    Ok(group_hit || pid_hit)
}

/// Return every distinct PID found LISTENING on `port` via `lsof`. Empty vec
/// when the port is idle, `lsof` is unavailable, or it fails for any reason
/// — callers treat an empty result as "nothing to do here".
async fn find_pids_on_port(port: u16) -> Vec<u32> {
    let Ok(output) = Command::new("lsof")
        .args([
            "-t",                       // terse: PIDs only, one per line
            "-nP",                      // no DNS / port-name resolution (faster)
            &format!("-iTCP:{}", port), // TCP on this port
            "-sTCP:LISTEN",             // only listeners
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    else {
        debug!("find_pids_on_port: lsof unavailable for port {}", port);
        return Vec::new();
    };

    let our_pid = std::process::id();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in stdout.lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            if pid > 0 && pid != our_pid && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Return the PID of the first process LISTENING on `port`, or `None`.
pub async fn find_pid_on_port(port: u16) -> Option<u32> {
    find_pids_on_port(port).await.into_iter().next()
}

/// Kill every process listening on a specific port. Returns true if at least
/// one kill landed. Never errors on `lsof` failure — it just finds nothing
/// and returns Ok(false).
pub async fn kill_by_port(port: u16) -> anyhow::Result<bool> {
    let mut killed_any = false;
    for pid in find_pids_on_port(port).await {
        info!("kill_by_port: killing PID {} on port {}", pid, port);
        // Tree-kill so the runner's children release the port too.
        if kill_by_pid_tree(pid).await.unwrap_or(false) {
            killed_any = true;
        }
    }
    Ok(killed_any)
}

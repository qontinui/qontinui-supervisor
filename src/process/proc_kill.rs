//! Cross-platform process-kill facade.
//!
//! Dispatches to [`crate::process::windows`] on Windows and
//! [`crate::process::unix_kill`] elsewhere so call sites (the reaper, the
//! stop path, the first-healthy watchdog, the reconcile sweep) can terminate
//! a runner's OS process without per-call `#[cfg(target_os = "windows")]`
//! branching.
//!
//! Historically the kill primitives were Windows-only; on macOS/Linux the
//! non-Windows arm was a no-op, which let the reaper drop a temp runner's
//! record while its process kept running (orphan leak D7). Routing every
//! caller through this facade closes that gap: every platform now has a real
//! kill.

#[cfg(target_os = "windows")]
use crate::process::windows as imp;

#[cfg(not(target_os = "windows"))]
use crate::process::unix_kill as imp;

/// Kill a single process by PID (graceful-then-forceful). Returns true if a
/// signal/kill landed on a process that existed.
pub async fn kill_by_pid(pid: u32) -> anyhow::Result<bool> {
    imp::kill_by_pid(pid).await
}

/// Kill a PID and its child process tree / group. Used by the stop
/// escalation path so child panes/helpers release the port.
pub async fn kill_by_pid_tree(pid: u32) -> anyhow::Result<bool> {
    imp::kill_by_pid_tree(pid).await
}

/// Kill every process LISTENING on `port`. Returns true if at least one kill
/// landed. Never errors when the port-lookup tool is unavailable.
pub async fn kill_by_port(port: u16) -> anyhow::Result<bool> {
    imp::kill_by_port(port).await
}

/// Return the PID of the first process LISTENING on `port`, or `None`.
pub async fn find_pid_on_port(port: u16) -> Option<u32> {
    imp::find_pid_on_port(port).await
}

/// True if `pid` is a live process whose executable / image name looks like a
/// `qontinui-runner` binary (bare `qontinui-runner[.exe]` or a per-runner copy
/// `qontinui-runner-<id>[.exe]`). Backed by sysinfo so it works on every
/// platform. Used by the reconcile sweep to confirm a process squatting on a
/// temp-runner port is one of *ours* before killing it — never kills an
/// unrelated process that happens to grab a port in the temp range.
pub fn is_qontinui_runner_pid(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System, UpdateKind};
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::Always)),
    );
    let Some(process) = system.process(Pid::from_u32(pid)) else {
        return false;
    };
    // Prefer the resolved exe file name; fall back to the process name.
    let candidate = process
        .exe()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| process.name().to_string_lossy().to_ascii_lowercase());

    image_name_is_runner(&candidate)
}

/// Pure name-matching half of [`is_qontinui_runner_pid`]: does an executable
/// image file name belong to the qontinui-runner family? Accepts the bare
/// `qontinui-runner[.exe]` and the per-runner copy `qontinui-runner-<id>[.exe]`
/// (the form `start_managed_runner` produces), and rejects near-misses like
/// `qontinui-runners` or `qontinui-supervisor`.
fn image_name_is_runner(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "qontinui-runner" || stem.starts_with("qontinui-runner-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_image_names_match() {
        assert!(image_name_is_runner("qontinui-runner"));
        assert!(image_name_is_runner("qontinui-runner.exe"));
        assert!(image_name_is_runner("qontinui-runner-primary"));
        assert!(image_name_is_runner("qontinui-runner-test-19ed69c80f6-7"));
        assert!(image_name_is_runner("qontinui-runner-named-foo.exe"));
        // Case-insensitive (Windows image names).
        assert!(image_name_is_runner("Qontinui-Runner.exe"));
    }

    #[test]
    fn non_runner_image_names_rejected() {
        assert!(!image_name_is_runner("qontinui-supervisor"));
        assert!(!image_name_is_runner("qontinui-runners")); // trailing 's', no hyphen
        assert!(!image_name_is_runner("node"));
        assert!(!image_name_is_runner("cargo"));
        assert!(!image_name_is_runner(""));
    }
}

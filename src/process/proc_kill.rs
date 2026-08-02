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

/// Kill every process LISTENING on `port`. `Ok(true)` when at least one kill
/// landed, `Ok(false)` when the probe ran and there was nothing to kill.
///
/// `Err` means the port-lookup probe could not RUN, so the port's state is
/// UNKNOWN — it is NOT "nothing was listening". Callers that record what they
/// actually did (`stop_ledger`) must log it as a rung that never ran rather
/// than as an attempted kill.
pub async fn kill_by_port(port: u16) -> anyhow::Result<bool> {
    imp::kill_by_port(port).await
}

/// Return the PID of the first process LISTENING on `port`.
///
/// Three-state on purpose, mirroring [`crate::process::windows::find_pid_on_port`]:
/// - `Ok(Some(pid))` — that PID holds the port;
/// - `Ok(None)` — the probe RAN and the port is idle;
/// - `Err(_)` — the probe could not run (no `netstat` / no `lsof`, or it
///   failed); the port's state is UNKNOWN and no caller may treat it as idle.
///
/// The `Err` arm is load-bearing: collapsing it into `None` is how a probe
/// that never ran gets reported as "nothing was listening", which then reads
/// as "nothing to kill".
pub async fn find_pid_on_port(port: u16) -> anyhow::Result<Option<u32>> {
    imp::find_pid_on_port(port).await
}

/// Executable path of a live PID via sysinfo, or `None` when the process is
/// gone or sysinfo could not read its image path.
///
/// Cross-platform twin of [`crate::process::windows::pid_exe_path`], so the
/// stop path's pre-kill identity re-check is not Windows-only. sysinfo reads
/// the image path on every supported platform; the Windows module keeps its
/// own copy because `build_monitor` calls it from `#[cfg(windows)]` code.
pub async fn pid_exe_path(pid: u32) -> Option<std::path::PathBuf> {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System, UpdateKind};
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::Always)),
    );
    let process = system.process(Pid::from_u32(pid))?;
    process.exe().map(|p| p.to_path_buf())
}

/// Every live PID whose `exe()` is `exe_path`. Cross-platform twin of
/// [`crate::process::windows::find_pids_holding_exe`], used by the stop path's
/// deterministic-identity rung: a runner always runs from its own
/// `runner_exe_copy_path`, so this names the process with no subprocess, no
/// locale dependence and no listening socket.
///
/// Comparison is case-insensitive, matching the Windows helper. On Unix that
/// is marginally looser than the filesystem, but the path being matched is a
/// supervisor-generated `qontinui-runner-<id>` under the runner's own
/// `target/debug` — there is no realistic case-variant collision, and being
/// loose here can only ever find OUR runner.
pub async fn find_pids_holding_exe(exe_path: &std::path::Path) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
    let wanted = exe_path.to_string_lossy().to_ascii_lowercase();
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::Always)),
    );
    let mut pids: Vec<u32> = Vec::new();
    for process in system.processes().values() {
        let Some(proc_exe) = process.exe() else {
            continue;
        };
        if proc_exe.to_string_lossy().to_ascii_lowercase() == wanted {
            let pid = process.pid().as_u32();
            if pid > 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
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

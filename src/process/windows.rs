use std::path::{Path, PathBuf};
use std::process::Stdio;
use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
use tokio::process::Command;
use tracing::{debug, info, warn};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Build a sysinfo `System` snapshot pre-populated with process info plus
/// the executable path field. We deliberately exclude memory/cpu/network
/// since we never read them — keeps enumeration in the ~10-50ms range on
/// a busy box.
///
/// `UpdateKind::Always` forces sysinfo to refresh `exe()` for every
/// process even if it had a cached value from a previous refresh; we want
/// the live image path so a stale cache cannot make us miss a holder.
fn snapshot_processes() -> System {
    let refresh =
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::Always));
    System::new_with_specifics(refresh)
}

/// Case-insensitive path-string compare. Windows file paths are
/// case-insensitive, but `Path::eq_ignore_ascii_case` doesn't exist —
/// `OsStr` comparisons are case-sensitive. Stringifying both sides via
/// `to_string_lossy()` and lowercasing is a pragmatic match for how the
/// OS actually treats these paths and avoids the canonicalize trap
/// (canonicalize fails on locked exes and resolves symlinks we don't
/// want resolved).
fn paths_equal_ignore_case(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_ascii_lowercase() == b.to_string_lossy().to_ascii_lowercase()
}

/// Return every distinct PID found LISTENING on `port`. Empty vec when the
/// port is idle or netstat fails for any reason — callers treat an empty
/// result as "nothing to do here". Shared by `find_pid_on_port` (first
/// result) and `kill_by_port` (kill all results).
async fn find_pids_on_port(port: u16) -> Vec<u32> {
    let Ok(output) = Command::new("cmd")
        .args([
            "/C",
            &format!("netstat -ano | findstr :{} | findstr LISTENING", port),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    else {
        return Vec::new();
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in stdout.lines() {
        // netstat -ano row: TCP    0.0.0.0:9876    0.0.0.0:0    LISTENING    12345
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(pid_str) = parts.last() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid > 0 && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

/// Kill every process listening on a specific port. Returns true if at
/// least one kill succeeded. Never errors on netstat failure — it just
/// finds nothing to kill and returns Ok(false).
pub async fn kill_by_port(port: u16) -> anyhow::Result<bool> {
    let mut killed_any = false;
    for pid in find_pids_on_port(port).await {
        info!("Killing PID {} on port {}", pid, port);
        if kill_by_pid(pid).await.unwrap_or(false) {
            killed_any = true;
        }
    }
    Ok(killed_any)
}

/// Clean up orphaned cargo/rustc processes that may be left from a previous build.
///
/// Enumerates processes via sysinfo (Win11 26200 removed wmic entirely, so
/// the previous wmic-based implementation was a silent no-op on this OS).
/// We attempt sysinfo's native `Process::kill()` first; if that returns
/// false we fall back to taskkill via `kill_by_pid` to cover edge cases
/// where sysinfo lacks the access right but taskkill (with /F) doesn't.
pub async fn cleanup_orphaned_build_processes() {
    let our_pid = std::process::id();
    let system = snapshot_processes();

    let mut killed_total: u32 = 0;
    for proc_name in &["cargo.exe", "rustc.exe"] {
        let mut killed_for_name: u32 = 0;

        // Collect (pid, sysinfo_kill_succeeded) pairs first so we don't hold
        // a borrow across the async fallback.
        let mut to_fallback: Vec<u32> = Vec::new();
        for process in system.processes().values() {
            if process
                .name()
                .to_string_lossy()
                .eq_ignore_ascii_case(proc_name)
            {
                let pid = process.pid().as_u32();
                if pid == 0 || pid == our_pid {
                    continue;
                }
                debug!(
                    "Cleaning up orphaned {} (PID {}) via sysinfo",
                    proc_name, pid
                );
                if process.kill() {
                    killed_for_name += 1;
                } else {
                    to_fallback.push(pid);
                }
            }
        }

        for pid in to_fallback {
            debug!(
                "sysinfo kill failed for {} PID {}; falling back to taskkill",
                proc_name, pid
            );
            if kill_by_pid(pid).await.unwrap_or(false) {
                killed_for_name += 1;
            }
        }

        if killed_for_name > 0 {
            debug!(
                "Killed {} orphaned {} process(es)",
                killed_for_name, proc_name
            );
            killed_total += killed_for_name;
        }
    }

    if killed_total > 0 {
        info!(
            "cleanup_orphaned_build_processes: killed {} orphan cargo/rustc process(es)",
            killed_total
        );
    }
}

/// Return the PID of the first process found LISTENING on `port`, or `None`
/// if the port is idle. Used by the health cache to recover a runner's PID
/// after a supervisor restart re-discovers it (the supervisor lost the PID
/// when it shut down; the process is still the same one bound to the
/// port).
pub async fn find_pid_on_port(port: u16) -> Option<u32> {
    find_pids_on_port(port).await.into_iter().next()
}

/// Return every PID whose `exe()` matches `exe_path` (case-insensitive
/// on Windows). Used by the build pool to identify processes holding a slot
/// binary open so the build can free the lock instead of failing with
/// "Access is denied" on the cargo replace step.
///
/// Backed by sysinfo. Returns an empty vec when no live process matches
/// — callers treat that as "no holder we can identify" and surface
/// the original lock error.
pub async fn find_pids_holding_exe(exe_path: &Path) -> Vec<u32> {
    // sysinfo enumeration is sync and runs ~10-50ms on a busy box. We
    // accept that as cheap enough to inline in the async caller rather
    // than wrap in spawn_blocking — these call sites are already
    // sleep-polling on file locks at 500ms cadence so the enumeration
    // cost is well within the budget.
    let exe_path = exe_path.to_path_buf();
    let system = snapshot_processes();
    let mut pids: Vec<u32> = Vec::new();
    for process in system.processes().values() {
        if let Some(proc_exe) = process.exe() {
            if paths_equal_ignore_case(proc_exe, &exe_path) {
                let pid = process.pid().as_u32();
                if pid > 0 && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

/// Return the executable path of a live PID via sysinfo, or `None` when
/// the process is gone or sysinfo could not read its image path.
///
/// Centralized here so `build_monitor.rs` does not have to duplicate the
/// sysinfo plumbing. Sync work runs inline in the async caller — see the
/// note on `find_pids_holding_exe` for why we don't bother with
/// spawn_blocking.
pub async fn pid_exe_path(pid: u32) -> Option<PathBuf> {
    let system = snapshot_processes();
    let process = system.process(sysinfo::Pid::from_u32(pid))?;
    process.exe().map(|p| p.to_path_buf())
}

/// Enumerate every live runner-family process (`qontinui-runner.exe`,
/// `qontinui-runner-<id>.exe`) and return `(pid, exe_path)` pairs. Used by
/// the startup orphan scan to find runner processes left over from a
/// prior supervisor instance.
///
/// **Why prefix-match, not exact:** `start_managed_runner` copies the
/// freshly-built slot exe to `target/debug/qontinui-runner-<id>.exe`
/// before launching, so the *running* process image is `qontinui-runner-
/// primary.exe`, `qontinui-runner-named-…exe`, etc. — not the bare
/// `qontinui-runner.exe`. An exact-name match misses every per-runner
/// copy, which is exactly the orphan we most often need to clean up. The
/// bare name is still matched (for the failure-mode case where Fix B
/// regressed and a runner is running from the slot dir directly).
///
/// Backed by sysinfo. Match is case-insensitive (Windows filesystem
/// semantics) on the prefix `qontinui-runner` followed by either `.exe`
/// or `-` then `<id>.exe`. The returned `exe_path` is whatever sysinfo
/// resolved for the live image. Processes whose image path can't be read
/// are skipped — without a path the orphan scan can't decide whether the
/// PID is one of ours, and acting on uncategorized PIDs would be unsafe.
///
/// Sync work runs inline in the async caller (single startup call) — see
/// the note on `find_pids_holding_exe`.
pub async fn find_runner_processes() -> Vec<(u32, PathBuf)> {
    let system = snapshot_processes();
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    for process in system.processes().values() {
        let name_str = process.name().to_string_lossy().to_ascii_lowercase();
        // Accept "qontinui-runner.exe" exactly, or "qontinui-runner-<id>.exe"
        // (where <id> is non-empty). Reject "qontinui-runner-foo" without
        // the .exe suffix and "qontinui-runners.exe" or other near-matches.
        let is_runner = name_str == "qontinui-runner.exe"
            || (name_str.starts_with("qontinui-runner-") && name_str.ends_with(".exe"));
        if !is_runner {
            continue;
        }
        let pid = process.pid().as_u32();
        if pid == 0 {
            continue;
        }
        let Some(exe) = process.exe() else {
            continue;
        };
        out.push((pid, exe.to_path_buf()));
    }
    out
}

/// Kill a process by its PID using taskkill.
pub async fn kill_by_pid(pid: u32) -> anyhow::Result<bool> {
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let success = output.status.success();
    if success {
        info!("taskkill: killed PID {}", pid);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("taskkill PID {}: {}", pid, stderr.trim());
    }

    Ok(success)
}

/// Return the WebView2 user data folder for a given runner id.
///
/// - The primary runner uses the default path `com.qontinui.runner\EBWebView`
///   (unchanged) so existing auth, terminal layouts, and other local state
///   are preserved.
/// - All other runners (named secondaries, temp `test-*` runners) get a
///   per-runner subdirectory `com.qontinui.runner\EBWebView-{id}` so their
///   localStorage, IndexedDB, cookies, and WebView2 caches are isolated from
///   the primary and from each other. This is what the runner process sees
///   via the `WEBVIEW2_USER_DATA_FOLDER` env var.
///
/// Returns `None` if `LOCALAPPDATA` is not set (non-Windows or broken env).
///
/// Thin compatibility wrapper around
/// [`qontinui_types::wire::webview2::webview2_data_dir`]; the layout logic
/// is owned by `qontinui-types` so the runner can fall back to the same
/// path scheme when launched standalone (no supervisor setting the env
/// var). `is_primary` is the existing call-site idiom on this crate;
/// internally it discriminates only the `Primary` branch — the non-primary
/// branch retains the historical `EBWebView-<id>` naming regardless of
/// whether the runner is `Temp`, `Named`, or `External`.
pub fn webview2_user_data_folder(runner_id: &str, is_primary: bool) -> Option<std::path::PathBuf> {
    use qontinui_types::wire::RunnerKind;
    // The shared helper only inspects `kind` to decide Primary-vs-other; the
    // non-Primary branch ignores the specific variant and uses `runner_id`
    // for the folder suffix. Reflect that: `External` is a safe placeholder
    // for "any non-primary" — we deliberately do NOT call
    // `RunnerKind::from_id(runner_id)` here so a non-primary call with id
    // `"primary"` (theoretical edge case) keeps producing
    // `EBWebView-primary` instead of collapsing onto the primary folder.
    let kind = if is_primary {
        RunnerKind::Primary
    } else {
        RunnerKind::External
    };
    qontinui_types::wire::webview2_data_dir(&kind, runner_id)
}

/// Remove a non-primary runner's WebView2 data folder. Used when a temp
/// runner is deleted so its state doesn't accumulate on disk.
///
/// Refuses to touch the primary runner's folder as a safety check — always
/// pass `is_primary: false`. Returns `Ok(false)` if the folder didn't exist
/// (nothing to clean up).
pub async fn remove_webview2_user_data_folder(
    runner_id: &str,
    is_primary: bool,
) -> anyhow::Result<bool> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's WebView2 data folder");
    }
    let Some(folder) = webview2_user_data_folder(runner_id, false) else {
        return Ok(false);
    };
    if !folder.exists() {
        return Ok(false);
    }
    match tokio::fs::remove_dir_all(&folder).await {
        Ok(()) => {
            info!(
                "Removed WebView2 data folder for runner '{}' at {:?}",
                runner_id, folder
            );
            Ok(true)
        }
        Err(e) => {
            warn!(
                "Failed to remove WebView2 data folder for runner '{}' at {:?}: {}",
                runner_id, folder, e
            );
            Err(e.into())
        }
    }
}

/// Remove per-instance app-data directories for a non-primary runner.
///
/// The runner's `crate::instance::scope_path()` helper writes per-runner
/// dev logs, macros, prompts, playwright tests, contexts, and Restate journals
/// under an `instance-<sanitized_name>` subdirectory of several base locations.
/// When a temp runner is deleted we clean these up so disk usage doesn't grow
/// unbounded.
///
/// `runner_name` must be the `managed.config.name` value that the supervisor
/// passed to the runner as `QONTINUI_INSTANCE_NAME` — not the runner's id.
///
/// Refuses to touch anything for primary runners as a safety check.
pub async fn remove_runner_app_data_dirs(
    runner_name: &str,
    is_primary: bool,
) -> anyhow::Result<u32> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's app data dirs");
    }

    // Mirror runner's sanitize() in src-tauri/src/instance.rs.
    let safe_name: String = runner_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let subdir = format!("instance-{}", safe_name);

    let local_app_data = std::env::var("LOCALAPPDATA")
        .ok()
        .map(std::path::PathBuf::from);
    let roaming_app_data = std::env::var("APPDATA").ok().map(std::path::PathBuf::from);

    // Each runner path helper applies scope_path at a slightly different
    // nesting level. Enumerate every known landing spot so all per-instance
    // state gets cleaned up.
    let candidates: Vec<std::path::PathBuf> = [
        // dev-logs: base is `.../qontinui-runner/dev-logs`, so the instance
        // dir lands inside dev-logs (see src/paths.rs::get_dev_logs_dir).
        local_app_data
            .as_ref()
            .map(|p| p.join("qontinui-runner").join("dev-logs").join(&subdir)),
        // macros, ai_workflows: base is `.../qontinui-runner`.
        local_app_data
            .as_ref()
            .map(|p| p.join("qontinui-runner").join(&subdir)),
        // prompts, playwright, contexts: base is `.../com.qontinui.runner`.
        local_app_data
            .as_ref()
            .map(|p| p.join("com.qontinui.runner").join(&subdir)),
        roaming_app_data
            .as_ref()
            .map(|p| p.join("com.qontinui.runner").join(&subdir)),
        // Restate journal: base is dirs::data_dir()/qontinui-runner/restate/data.
        roaming_app_data.as_ref().map(|p| {
            p.join("qontinui-runner")
                .join("restate")
                .join("data")
                .join(&subdir)
        }),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut removed = 0u32;
    for path in candidates {
        if !path.exists() {
            continue;
        }
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                info!(
                    "Removed per-instance app data for runner '{}' at {:?}",
                    runner_name, path
                );
                removed += 1;
            }
            Err(e) => {
                warn!(
                    "Failed to remove per-instance app data at {:?}: {}",
                    path, e
                );
            }
        }
    }
    Ok(removed)
}

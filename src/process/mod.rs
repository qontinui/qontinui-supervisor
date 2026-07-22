pub mod early_log;
pub mod env_forwarders;
pub mod guarded_command;
pub mod health_probe;
pub mod job;
pub mod manager;
pub mod orphan_scan;
pub mod panic_log;
pub mod port;
pub mod restate_port;
pub mod stopped_cache;
#[cfg(target_os = "windows")]
pub mod windows;

use std::path::PathBuf;

/// The per-instance config + secure-storage directory of a supervisor-spawned
/// runner: `<config_dir>/com.qontinui.runner/instances/<runner_id>`.
///
/// **Single source of truth for that path.** Three call sites must agree on it
/// byte-for-byte:
///
/// 1. **Spawn side** — [`manager::start_exe_mode_for_runner`] exports it to the
///    child as BOTH `QONTINUI_CONFIG_DIR` and `QONTINUI_SECURE_STORAGE_DIR`.
///    The runner prefers those env vars over its
///    `dirs::data_local_dir()/com.qontinui.runner` fallback (`auth.rs`,
///    `secure_storage.rs`, `pair.rs` all read the env var first), so for a
///    supervisor-spawned runner this is the *only* directory its pairing and
///    token cache are ever loaded from.
/// 2. **Profile-write side** — `routes::runners::apply_paired_profile_for_spawn`
///    copies the requested `paired_profile_id` snapshot INTO it before the
///    child process starts.
/// 3. **Removal side** — [`windows::remove_instance_config_dir`] reaps it when
///    the runner is deleted.
///
/// None of the three may compute this path independently. They used to: the
/// profile-write side copied into the shared `data_local_dir()` fallback while
/// the spawn side pointed the child at the per-instance dir. Every
/// `POST /runners/spawn-test {"paired_profile_id": …}` therefore reported
/// success and produced an UNPAIRED runner that logged `provisioning gate
/// (advisory): runner has NO live coord device JWT` every 15s. Funnelling all
/// three through one function is what makes that divergence impossible.
///
/// Note on placement: this lives here rather than beside the remover in
/// [`windows`] because that module is `#[cfg(target_os = "windows")]` while the
/// spawn and profile-write sides are cross-platform (CI builds this crate on
/// Linux).
///
/// Returns `None` when the platform has no resolvable config dir, or when
/// `runner_id` is degenerate (empty, or containing a path separator or `..`).
/// Callers must treat that as a hard failure — silently falling back to a
/// shared directory is precisely the bug described above.
///
/// The id guard mirrors the traversal rejection in
/// `routes::runners::apply_paired_profile`. It matters because
/// `PathBuf::join("")` does NOT descend: `instance_config_dir("")` would
/// otherwise return the `instances/` PARENT, and
/// [`windows::remove_instance_config_dir`] would `remove_dir_all` every
/// runner's instance dir (its `is_primary` flag does not guard that). Ids are
/// server-generated today, so this is unreachable — but this function is `pub`
/// and is the documented single source of truth for three call sites, so the
/// guard is worth its two comparisons.
pub fn instance_config_dir(runner_id: &str) -> Option<PathBuf> {
    if runner_id.is_empty() || runner_id.contains(['/', '\\']) || runner_id.contains("..") {
        return None;
    }
    dirs::config_dir().map(|d| {
        d.join("com.qontinui.runner")
            .join("instances")
            .join(runner_id)
    })
}

#[cfg(test)]
mod tests {
    use super::instance_config_dir;

    /// A degenerate id must resolve to `None`, never to a path.
    ///
    /// `""` is the dangerous one: `PathBuf::join("")` does not descend, so
    /// without the guard the helper hands back the shared `instances/` parent
    /// and the reaper wipes every instance dir on the box. The separator and
    /// `..` cases are the same traversal class `apply_paired_profile` rejects.
    #[test]
    fn instance_config_dir_rejects_degenerate_ids() {
        for bad in ["", "a/b", "a\\b", ".."] {
            assert_eq!(
                instance_config_dir(bad),
                None,
                "instance_config_dir({bad:?}) must be None — it escapes or collapses to the \
                 shared instances/ parent"
            );
        }
    }

    /// The happy path still resolves, so the guard cannot be satisfied by
    /// simply returning `None` everywhere.
    #[test]
    fn instance_config_dir_accepts_a_normal_runner_id() {
        if dirs::config_dir().is_none() {
            return;
        }
        let dir = instance_config_dir("test-9877").expect("normal id must resolve");
        assert!(dir.ends_with(std::path::Path::new(
            "com.qontinui.runner/instances/test-9877"
        )));
    }
}

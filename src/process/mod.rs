/// Cross-platform on purpose — the strip rule applies to every spawn on every
/// platform, and the grep-level regression guard inside it must run on the
/// Linux-only CI gate that blocks merges.
pub mod claude_env;
pub mod early_log;
pub mod env_forwarders;
pub mod guarded_command;
pub mod health_probe;
pub mod job;
pub mod manager;
/// Cross-platform on purpose — see the module docs. CI is Linux-only, and the
/// "which PID is LISTENING on this port" predicate must be covered by the gate
/// that blocks merges; it silently returned the wrong answer on every
/// non-English Windows for as long as it lived inside the `windows` module.
///
/// The `allow` is the price of that placement: every non-test CONSUMER is
/// `#[cfg(target_os = "windows")]`, so from the binary's private module tree
/// these items are unreachable on Linux and `-D warnings` rejects them as dead
/// code. Scoped to non-Windows deliberately — on Windows, where the code does
/// run, genuine dead code is still reported.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod netstat_parse;
pub mod orphan_scan;
pub mod panic_log;
pub mod port;
pub mod proc_kill;
pub mod restate_port;
/// Cross-platform on purpose — see the module docs. CI is Linux-only, and the
/// slot-kill predicate must be covered by the gate that blocks merges.
pub mod slot_territory;
/// Cross-platform on purpose — see the module docs. Decides what a failed stop
/// TELLS the operator.
///
/// The stop ladder itself is no longer Windows-only: every rung now goes
/// through [`proc_kill`], so the tree-kill / kill-by-port rungs and the
/// pre-kill identity check are constructed on every platform and the module
/// needs no dead-code exemption.
pub mod stop_ledger;
pub mod stopped_cache;
/// Cross-platform on purpose — see the module docs. The census is the
/// readiness source that survives the runner's wedged HTTP door, and its walk
/// and name predicate must be covered by the Linux-only gate that blocks
/// merges.
///
/// The `allow` is temporary: Phase 1 of plan
/// `2026-09-03-runner-zombie-serving-watchdog` lands the module with no
/// in-tree consumer, and this is a bin crate, so `-D warnings` would reject
/// every item as dead code. Phase 2 (`restart_readiness`'s census fallback)
/// consumes it and removes this attribute.
#[allow(dead_code)]
pub mod subtree_census;
#[cfg(not(target_os = "windows"))]
pub mod unix_kill;
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
/// 4. **Pair side** — `routes::runners_pair::pair_with_token` (with
///    `target_runner_id`) exports it to the `qontinui_profile` child as
///    `QONTINUI_SECURE_STORAGE_DIR` and reads `paired_user.json` back from it,
///    so an existing runner can be paired in place.
///
/// None of the four may compute this path independently. They used to: the
/// profile-write side copied into the shared `data_local_dir()` fallback while
/// the spawn side pointed the child at the per-instance dir. Every
/// `POST /runners/spawn-test {"paired_profile_id": …}` therefore reported
/// success and produced an UNPAIRED runner that logged `provisioning gate
/// (advisory): runner has NO live coord device JWT` every 15s. Funnelling all
/// call sites through one function is what makes that divergence impossible.
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
/// and is the documented single source of truth for four call sites, so the
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

/// The value a temp runner is given as its `QONTINUI_INSTANCE_NAME` — i.e.
/// **the runner's own unique id**, never anything derived from its port.
///
/// The runner keys its entire `instance-<sanitized_name>` app-data tree off
/// this string (`qontinui-runner` `src-tauri/src/instance.rs:scope_path`),
/// including `terminal-sessions.json`. It used to be `format!("test-{port}")`,
/// and temp ports are recycled inside a 23-slot range (9877-9899) — so two
/// sequential temp runners on the same port resolved to the SAME instance dir
/// and the second inherited the first's live terminal-session registry. Plan
/// `2026-07-20-runner-port-keyed-state-inheritance` moved that store off a
/// `-<port>` *filename* onto an instance key that was itself port-derived: the
/// inheritance was renamed, not removed.
///
/// The id (`test-<hex-millis>-<hex-seq>`, minted by `routes::runners::uuid_simple`) is already
/// unique per spawn and already keys every other per-instance resource — the
/// config/secure-storage dir ([`instance_config_dir`]), the
/// WebView2 profile, and `QONTINUI_RUNNER_ID` itself. Reusing it here is what
/// keeps the name and the id from drifting apart again; no second uuid is
/// minted.
///
/// **Teardown follows automatically** because every removal site reads
/// `managed.config.name`. There are **four**, not three:
///
/// 1. `routes::runners::remove_runner` (the `DELETE` handler)
/// 2. `routes::runners::purge_stale_test_runners_core`
/// 3. `process::manager::stop_runner_by_id` (auto-remove arm)
/// 4. `process::manager::reap_stale_test_runners` (the periodic sweep — and the
///    one that can now kill a *live* runner for age)
///
/// All four hand that value to `process::windows::remove_runner_app_data_dirs`,
/// whose sanitizer ([`sanitize_instance_name`]) mirrors the
/// runner's. The id's alphabet is `[0-9a-f-]`, so it survives both sanitizers
/// unchanged (identity mapping) and the dir removed is exactly the dir created
/// — pinned by
/// `process::tests::temp_runner_instance_name_survives_the_app_data_sanitizer`.
///
/// **Legacy trees are permanently orphaned by this change.** Up to 23
/// `instance-test-9877` … `instance-test-9899` trees (and their stale
/// `terminal-sessions.json`) exist on machines that ran the old scheme. Nothing
/// will ever key onto those names again, so nothing reuses them — which is the
/// point — and nothing removes them either. Bounded (≤23 dirs, one per port
/// slot) and harmless, but it makes permanent the orphaned-file janitor that
/// `2026-07-20-runner-port-keyed-state-inheritance` §7 deferred. That janitor is
/// now the only thing that will ever clean them up.
///
/// Note this deliberately does NOT change
/// [`crate::config::SupervisorConfig::runner_exe_copy_path`], which stays
/// port-keyed on purpose (a per-spawn exe path re-triggers a Windows Firewall
/// prompt on every cold spawn).
pub(crate) fn temp_runner_instance_name(id: &str) -> String {
    id.to_string()
}

/// Map a runner's `QONTINUI_INSTANCE_NAME` to the directory-safe form the
/// runner itself uses: keep `[A-Za-z0-9-_]`, replace everything else with `_`.
///
/// **Byte-for-byte mirror of `qontinui-runner`
/// `src-tauri/src/instance.rs:sanitize()`**, which is what `instance::scope_path`
/// applies when it creates the `instance-<sanitized>` app-data tree. The
/// supervisor's `windows::remove_runner_app_data_dirs` is the only consumer:
/// it must reconstruct exactly the directory the runner created, or teardown
/// deletes nothing while the real tree leaks — silently, and once per spawn.
///
/// **Cross-platform on purpose**, like [`netstat_parse`] and [`slot_territory`]:
/// this is a cross-repo contract, and the test pinning it must run on the
/// Linux-only CI gate that blocks merges rather than only on a Windows box.
/// The `allow` is the price of that placement (the sole non-test consumer is
/// `#[cfg(target_os = "windows")]`).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn sanitize_instance_name(runner_name: &str) -> String {
    runner_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{instance_config_dir, sanitize_instance_name};

    /// The sanitizer still agrees with the runner's `sanitize()` on the exact
    /// cases the runner pins (`instance.rs::sanitize_keeps_safe_chars`).
    #[test]
    fn sanitize_instance_name_mirrors_the_runner_side_sanitizer() {
        assert_eq!(sanitize_instance_name("test-runner_1"), "test-runner_1");
        assert_eq!(sanitize_instance_name("abc/def"), "abc_def");
        assert_eq!(sanitize_instance_name("weird name!"), "weird_name_");
    }

    /// The instance name a temp spawn mints must survive the app-data
    /// sanitizer **unchanged**, so the tree teardown removes is exactly the
    /// tree the runner created.
    ///
    /// [`temp_runner_instance_name`] returns the runner id
    /// (`test-<hex-millis>-<hex-seq>`), whose alphabet is `[0-9a-f-]` — every
    /// character is already in the keep-set of BOTH [`sanitize_instance_name`]
    /// and the runner's `instance.rs:sanitize()`, so the mapping is the
    /// identity and the two repos cannot disagree about the resulting
    /// `instance-<name>` dir.
    ///
    /// This is the "worse than the bug" risk from plan
    /// `2026-08-10-temp-runner-session-restore-isolation`: re-keying
    /// `QONTINUI_INSTANCE_NAME` while teardown still resolves the old key turns
    /// one reused directory into unbounded per-spawn clutter.
    #[test]
    fn temp_runner_instance_name_survives_the_app_data_sanitizer() {
        for id in [
            "test-19fe1161aa3-0".to_string(),
            "test-1a02b3c4d5e-ff".to_string(),
            format!("test-{:x}-{:x}", u64::MAX, u32::MAX),
        ] {
            let name = super::temp_runner_instance_name(&id);
            assert_eq!(
                sanitize_instance_name(&name),
                name,
                "the minted instance name {name:?} must be its own sanitized form — \
                 otherwise the runner creates instance-<sanitized> while teardown \
                 targets a different string"
            );
        }
    }

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

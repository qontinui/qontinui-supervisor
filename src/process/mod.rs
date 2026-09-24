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
/// Cross-platform on purpose — the reconcile sweep's ownership decision must be
/// covered by the Linux-only CI gate that blocks merges. Killing a process the
/// supervisor never spawned is the failure it exists to prevent, and a
/// Windows-only test would not gate it.
pub mod temp_runner_ledger;
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
/// 3. **Removal side** — [`remove_instance_config_dir`] reaps it when
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
/// Note on placement: this and its remover live here, not in [`windows`],
/// because that module is `#[cfg(target_os = "windows")]` while all four sides
/// are cross-platform (CI builds this crate on Linux).
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
/// [`remove_instance_config_dir`] would `remove_dir_all` every
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

/// Reap a non-primary runner's [`instance_config_dir`] — its per-instance
/// config + secure-storage dir, which holds a COPY of a pairing
/// (`paired_user.json`) and the encrypted token cache (`auth_tokens.enc`) once
/// spawn-test has applied a paired profile or the primary snapshot.
///
/// Cross-platform on purpose. It used to live in the Windows-only module, so on
/// Linux nothing ever deleted these dirs; once a default spawn-test began
/// copying the primary's credential store into one (plan
/// `2026-09-23-conductor-e2e-phase1-defects`, S-4), every temp spawn on Linux
/// would have left a copy behind for good.
///
/// Returns `Ok(true)` when a dir was removed, `Ok(false)` when there was none.
/// Refuses the primary outright.
pub async fn remove_instance_config_dir(runner_id: &str, is_primary: bool) -> anyhow::Result<bool> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's instance config dir");
    }
    // Resolve through the shared helper, never inline: the reaper must delete
    // exactly the directory the spawn side exported as
    // `QONTINUI_SECURE_STORAGE_DIR` and the paired-profile writer copied into.
    let Some(dir) = crate::process::instance_config_dir(runner_id) else {
        return Ok(false);
    };
    if !dir.exists() {
        return Ok(false);
    }
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {
            tracing::info!(
                "Removed instance config dir for runner '{}' at {:?}",
                runner_id,
                dir
            );
            Ok(true)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to remove instance config dir for runner '{}' at {:?}: {}",
                runner_id,
                dir,
                e
            );
            Err(e.into())
        }
    }
}

/// The PRIMARY runner's paired-state directory —
/// `dirs::data_local_dir()/com.qontinui.runner`, where it keeps
/// `paired_user.json` and `auth_tokens.enc`.
///
/// The primary runs with no `QONTINUI_SECURE_STORAGE_DIR` override (only
/// non-primary spawns get [`instance_config_dir`]), so both files sit at the
/// runner's own fallback: `SecureStorage::new()` in qontinui-runner
/// `src-tauri/src/secure_storage.rs` and `paired_user_path` in `pair.rs`.
/// `spawn-test` without a `paired_profile_id` snapshots this dir into the temp
/// runner's instance dir (`routes::runners::apply_primary_paired_snapshot`).
///
/// Returns `None` only when the platform data-local dir can't be resolved.
pub fn primary_paired_state_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("com.qontinui.runner"))
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
/// All four hand that value to [`remove_runner_app_data_dirs`],
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
/// applies when it creates the `instance-<sanitized>` app-data tree. This
/// module's own [`remove_runner_app_data_dirs`] is the consumer: it must
/// reconstruct exactly the directory the runner created, or teardown deletes
/// nothing while the real tree leaks — silently, and once per spawn.
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

/// Every base location under which `qontinui-runner`'s `instance::scope_path()`
/// helper may have written an `instance-<sanitized_name>` per-instance tree —
/// dev logs, macros/ai-workflows, prompts/playwright/contexts, and the Restate
/// journal — mirroring exactly the bases the runner itself resolves:
///
/// - dev-logs (`paths.rs::get_default_dev_logs_dir`):
///   `dirs::data_local_dir()/qontinui-runner/dev-logs`
/// - macros / ai_workflows (e.g. `macros.rs::get_storage_dir`):
///   `dirs::data_local_dir()/qontinui-runner`
/// - prompts / playwright / contexts (e.g. `prompts.rs::get_prompts_path`):
///   `dirs::config_dir()/com.qontinui.runner`
/// - Restate journal (`restate/config.rs::resolve_data_dir`, whose
///   `app_data_dir` argument is `dirs::data_dir().join("qontinui-runner")` —
///   see `main.rs`'s Restate-injection block):
///   `dirs::data_dir()/qontinui-runner/restate/data`
///
/// `data_local_dir()`, `data_dir()` and `config_dir()` resolve to Windows'
/// local/roaming AppData the same way the runner's own calls do (`config_dir()`
/// and `data_dir()` are the SAME roaming folder on Windows), so this is one
/// cross-platform resolution rather than a Windows-only one plus a guessed
/// non-Windows arm. A base that fails to resolve on this platform contributes
/// no candidate rather than erroring — the caller treats a missing candidate
/// exactly like one that resolved but does not exist on disk.
fn app_data_dir_candidates(subdir: &str) -> Vec<PathBuf> {
    let data_local = dirs::data_local_dir();
    let data = dirs::data_dir();
    let config = dirs::config_dir();
    [
        data_local
            .as_ref()
            .map(|p| p.join("qontinui-runner").join("dev-logs").join(subdir)),
        data_local
            .as_ref()
            .map(|p| p.join("qontinui-runner").join(subdir)),
        config
            .as_ref()
            .map(|p| p.join("com.qontinui.runner").join(subdir)),
        data.as_ref().map(|p| {
            p.join("qontinui-runner")
                .join("restate")
                .join("data")
                .join(subdir)
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Remove per-instance app-data directories for a non-primary runner.
///
/// The runner's `crate::instance::scope_path()` helper writes per-runner dev
/// logs, macros, prompts, playwright tests, contexts, and Restate journals
/// under an `instance-<sanitized_name>` subdirectory of several base
/// locations ([`app_data_dir_candidates`]). When a temp runner is deleted we
/// clean these up so disk usage doesn't grow unbounded.
///
/// `runner_name` must be the `managed.config.name` value that the supervisor
/// passed to the runner as `QONTINUI_INSTANCE_NAME`. For a **temp** runner
/// that value now IS the runner id ([`temp_runner_instance_name`]) — it used
/// to be `test-<port>`, which made two spawns on a recycled port share one
/// `instance-<name>` tree. Keep resolving it from `config.name` rather than
/// substituting `config.id` at a call site: named runners still carry an
/// operator-supplied name, and the two keys must not be assumed equal.
///
/// Cross-platform on purpose, like [`remove_instance_config_dir`] beside it.
/// It used to be `windows::remove_runner_app_data_dirs`, gated
/// `#[cfg(target_os = "windows")]` at every call site — so on Linux, where
/// `qontinui-supervisor` `#197` (plan `2026-09-23-conductor-e2e-phase1-defects`,
/// S-1..S-3) just made temp runners actually survive to populate these trees,
/// nothing ever reaped them. Every candidate is still probed even when several
/// resolve to the same directory on a given platform (Windows: `config_dir()`
/// and `data_dir()` coincide) — `path.exists()` makes the duplicate a no-op,
/// not a double-count.
///
/// Refuses to touch anything for primary runners as a safety check.
pub async fn remove_runner_app_data_dirs(
    runner_name: &str,
    is_primary: bool,
) -> anyhow::Result<u32> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's app data dirs");
    }

    let subdir = format!("instance-{}", sanitize_instance_name(runner_name));
    let mut removed = 0u32;
    for path in app_data_dir_candidates(&subdir) {
        if !path.exists() {
            continue;
        }
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                tracing::info!(
                    "Removed per-instance app data for runner '{}' at {:?}",
                    runner_name,
                    path
                );
                removed += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to remove per-instance app data at {:?}: {}",
                    path,
                    e
                );
            }
        }
    }
    Ok(removed)
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

    /// The write side and the reap side must resolve the SAME directory.
    ///
    /// If they diverge, a spawn writes a runner's pairing into a directory the
    /// reaper never visits — the instance dirs leak, and (worse) whichever side
    /// is wrong is silently wrong, exactly like the `paired_profile_id` copy
    /// that landed in `data_local_dir()` while the child read the per-instance
    /// dir. Asserted behaviourally rather than by re-deriving the path: we
    /// create the dir at `instance_config_dir(id)`, hand only the id to
    /// `remove_instance_config_dir`, and require it to report a real removal
    /// (`Ok(true)` — it returns `Ok(false)` for a dir it does not find).
    #[tokio::test]
    async fn instance_config_dir_and_remover_agree_on_path() {
        let runner_id = format!("test-instance-config-dir-selftest-{}", std::process::id());
        let Some(dir) = instance_config_dir(&runner_id) else {
            // No resolvable config dir on this platform — nothing to assert.
            return;
        };
        std::fs::create_dir_all(&dir).expect("create instance dir");
        // The dir lives under the operator's REAL config dir, so it must not
        // survive a failing assertion below — the divergence case this test
        // exists to catch is exactly the one that would leak it permanently.
        // Same guard the routes-side test uses.
        let _cleanup = scopeguard::guard(dir.clone(), |d| {
            let _ = std::fs::remove_dir_all(d);
        });
        std::fs::write(dir.join("paired_user.json"), b"{}").expect("write marker");

        let removed = super::remove_instance_config_dir(&runner_id, false)
            .await
            .expect("remover must not error");

        assert!(
            removed,
            "remove_instance_config_dir did not find the dir instance_config_dir() \
             created at {dir:?} — the write side and the reap side have diverged"
        );
        assert!(!dir.exists(), "instance dir must be gone after removal");
    }

    /// The reaper refuses primaries outright, so a mis-keyed call can never
    /// delete the operator's own runner config.
    #[tokio::test]
    async fn instance_config_dir_remover_refuses_primary() {
        assert!(super::remove_instance_config_dir("primary", true)
            .await
            .is_err());
    }

    // --- remove_runner_app_data_dirs (app-data instance-tree reaper) ------

    /// Every base that resolves on this platform must produce a candidate
    /// ending in the `instance-<name>` subdir — so a caller that creates
    /// markers at each and calls the remover can expect them all gone.
    #[test]
    fn app_data_dir_candidates_all_end_in_the_subdir() {
        let candidates = super::app_data_dir_candidates("instance-selftest");
        assert!(
            !candidates.is_empty(),
            "at least one of data_local_dir/data_dir/config_dir must resolve on CI"
        );
        for c in &candidates {
            assert!(
                c.ends_with("instance-selftest"),
                "{c:?} does not end with the instance subdir"
            );
        }
    }

    /// The write side (the runner's own `instance::scope_path()`, mirrored
    /// here as [`super::app_data_dir_candidates`]) and the reap side
    /// ([`super::remove_runner_app_data_dirs`]) must resolve the SAME
    /// directories — same shape as `instance_config_dir_and_remover_agree_on_path`
    /// above, for the sibling leak class this reaper closes: it used to be
    /// Windows-only (`windows::remove_runner_app_data_dirs`), so on Linux —
    /// where qontinui-supervisor #197 just made temp runners survive to
    /// populate these trees at all — nothing ever reaped them.
    #[tokio::test]
    async fn remove_runner_app_data_dirs_removes_every_resolved_candidate() {
        let runner_name = format!("app-data-selftest-{}", std::process::id());
        let subdir = format!("instance-{}", sanitize_instance_name(&runner_name));
        let mut candidates = super::app_data_dir_candidates(&subdir);
        candidates.sort();
        candidates.dedup();
        if candidates.is_empty() {
            // No resolvable base dir on this platform — nothing to assert.
            return;
        }
        let mut cleanups = Vec::new();
        for dir in &candidates {
            std::fs::create_dir_all(dir).expect("create candidate dir");
            std::fs::write(dir.join("marker.json"), b"{}").expect("write marker");
            // Same belt-and-braces guard as the instance-config-dir test
            // above: these live under the operator's real data/config dirs.
            cleanups.push(scopeguard::guard(dir.clone(), |d| {
                let _ = std::fs::remove_dir_all(d);
            }));
        }

        let removed = super::remove_runner_app_data_dirs(&runner_name, false)
            .await
            .expect("remover must not error");

        assert_eq!(
            removed as usize,
            candidates.len(),
            "remove_runner_app_data_dirs did not remove every candidate \
             app_data_dir_candidates resolved — the write side and the reap side \
             have diverged"
        );
        for dir in &candidates {
            assert!(!dir.exists(), "{dir:?} must be gone after removal");
        }
        cleanups.clear();
    }

    /// Refuses primaries outright, mirroring `remove_instance_config_dir`'s
    /// guard — a mis-keyed call can never delete the operator's own runner
    /// data.
    #[tokio::test]
    async fn remove_runner_app_data_dirs_refuses_primary() {
        assert!(super::remove_runner_app_data_dirs("primary", true)
            .await
            .is_err());
    }
}

//! Managed detached git worktrees for `POST /runners/spawn-test {git_ref}`.
//!
//! The supervisor normally rebuilds `state.config.project_dir` — the live
//! runner working tree. When that tree is parked on a broken or foreign
//! branch (common in the multi-agent flow), `spawn-test {rebuild:true}`
//! compiles whatever is checked out and there's no way to obtain a temp
//! runner that reflects `origin/main` (or any other ref) for diagnostics.
//!
//! This module materializes a *stable, detached* worktree at a known ref so
//! the build's provenance is the ref the caller asked for — never the live
//! tree. On any git failure it returns a clear [`SupervisorError`]; it never
//! silently falls back to the live tree (that would defeat the whole point).
//!
//! ## Placement invariant + pinned-schemas isolation
//!
//! The runner crate's `src-tauri/Cargo.toml` declares upward-relative cargo
//! path-deps:
//!
//! ```toml
//! qontinui-types      = { path = "../../qontinui-schemas/rust", ... }
//! qontinui-vision-core = { path = "../../qontinui-schemas/rust-vision-core", ... }
//! ```
//!
//! From `<runner_wt>/src-tauri/`, `../../qontinui-schemas/rust` resolves to
//! the directory two levels above the `src-tauri` dir.
//!
//! An earlier layout placed the runner worktree FLAT at
//! `<workspace_root>/.spawn-<ref>/`, so `../../qontinui-schemas/rust`
//! resolved to the SHARED `<workspace_root>/qontinui-schemas/rust` checkout.
//! That shared checkout is routinely parked on a WIP branch by a concurrent
//! session, so a clean `origin/main` spawn build failed through no fault of
//! the requested ref (qontinui-supervisor#... — "shared path-dep drift").
//!
//! To isolate the build from shared-checkout state we now nest BOTH repos one
//! level deeper under a per-ref container dir:
//!
//! ```text
//! <workspace_root>/.spawn-<ref>/
//!   qontinui-runner/             ← detached worktree at the requested ref
//!     src-tauri/Cargo.toml       ← path = "../../qontinui-schemas/rust"
//!   qontinui-schemas/            ← detached worktree pinned to origin/main
//!     rust/ rust-vision-core/
//! ```
//!
//! From `<container>/qontinui-runner/src-tauri/`, `../../qontinui-schemas/rust`
//! now resolves to `<container>/qontinui-schemas/rust` — the pinned worktree,
//! fully isolated from whatever branch the shared schemas checkout is on.
//! The schemas worktree is created from the SHARED `qontinui-schemas` repo
//! (which has the git object history) but checked out detached at
//! `origin/main`, and re-pinned on every reuse.

use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::error::SupervisorError;

/// Sanitize a git ref into a filesystem-safe directory component: every
/// character outside `[A-Za-z0-9._-]` becomes `_`. Keeps the mapping stable
/// (same ref ⇒ same worktree dir ⇒ reused across requests).
pub fn sanitize_ref(git_ref: &str) -> String {
    let s: String = git_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "_".to_string()
    } else {
        s
    }
}

/// Walk up from `project_dir` (the runner `src-tauri`) to the nearest
/// ancestor that contains a `.git` entry (file or dir — `.git` is a *file*
/// inside a worktree). Returns the repo root.
///
/// Handles both layouts: `project_dir` being `<repo>/src-tauri` (the usual
/// case) and `project_dir` itself being the repo root.
pub fn find_repo_root(project_dir: &Path) -> Result<PathBuf, SupervisorError> {
    let mut cur: Option<&Path> = Some(project_dir);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    Err(SupervisorError::Validation(format!(
        "could not locate a git repo root at or above project_dir {:?} (no .git found)",
        project_dir
    )))
}

/// Max number of ancestor levels we'll walk from `project_dir` looking for
/// the workspace root. The expected depth is 2 (`.../qontinui-runner/src-tauri`
/// → `.../qontinui-runner` → `.../<workspace>`); 6 gives generous slack for
/// nested checkouts (e.g. worktree shells under `D:/qontinui-root.wt/<wt>/`)
/// without devolving into "search the whole filesystem".
const WORKSPACE_ROOT_WALK_CAP: usize = 6;

/// Returns the workspace root: the directory that contains BOTH `qontinui-runner`
/// AND `qontinui-schemas` (siblings under the same parent). Walks up from
/// `project_dir`. Required so the per-ref spawn container can sit at the same
/// level as `qontinui-runner` and so the schemas worktree can be created from
/// the shared `<workspace_root>/qontinui-schemas` repo.
///
/// Cap: walks at most [`WORKSPACE_ROOT_WALK_CAP`] ancestor levels before
/// giving up. On failure returns a [`SupervisorError::Validation`] listing
/// every directory it inspected — this is *intentionally noisy* because the
/// previous bug (#40 P1) was a silent fallback to the wrong placement.
pub fn derive_workspace_root(project_dir: &Path) -> Result<PathBuf, SupervisorError> {
    let mut searched: Vec<PathBuf> = Vec::new();
    let mut cur: Option<&Path> = Some(project_dir);
    for _ in 0..=WORKSPACE_ROOT_WALK_CAP {
        let dir = match cur {
            Some(d) => d,
            None => break,
        };
        searched.push(dir.to_path_buf());
        if dir.join("qontinui-runner").is_dir() && dir.join("qontinui-schemas").is_dir() {
            return Ok(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    Err(SupervisorError::Validation(format!(
        "could not derive workspace root from project_dir {:?}: walked {} ancestor(s) \
         looking for a directory containing BOTH `qontinui-runner/` and \
         `qontinui-schemas/` (this invariant is required so spawn-worktree \
         path-deps `../../qontinui-schemas/...` resolve correctly). \
         Searched: {:?}",
        project_dir,
        searched.len(),
        searched
    )))
}

/// Run a git command, returning a clear `SupervisorError::Process` on
/// non-zero exit or spawn failure. `quiet_offline_ok` makes a failed
/// `fetch` non-fatal (offline tolerance) — the caller still validates the
/// ref via the subsequent checkout/rev-parse.
async fn git(args: &[&str], cwd: &Path, quiet_offline_ok: bool) -> Result<String, SupervisorError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| {
            SupervisorError::Process(format!("failed to spawn `git {}`: {}", args.join(" "), e))
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else if quiet_offline_ok {
        // Best-effort fetch: tolerate offline / transient remote errors.
        Ok(String::new())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SupervisorError::Process(format!(
            "`git {}` (in {:?}) failed with {}: {}",
            args.join(" "),
            cwd,
            output.status,
            stderr.trim()
        )))
    }
}

/// Prefix prepended to a build-failure error when [`classify_drift`] detects
/// the cargo error references a file OUTSIDE the spawn container — i.e. the
/// failure is shared path-dep drift, not the requested ref's fault.
pub const DRIFT_PREFIX: &str = "shared path-dep drift — not your ref: ";

/// Inspect a cargo build-failure error string for file paths that live
/// OUTSIDE the spawn container. With pinned-schemas isolation in place this
/// should be rare, but it catches any residual out-of-worktree leakage
/// (registry/cache issues, a future path-dep that escapes the container,
/// etc.). Returns `Some(offending_path)` if such a path is found.
///
/// Heuristic: scan for the SHARED `qontinui-schemas` checkout path (the
/// workspace-root sibling, which is what the old flat layout leaked into)
/// and for any `error[...]` line whose referenced file path is absolute and
/// does not start with the container. We keep it conservative — only the
/// shared-schemas signal and an explicit container-escape are reported, so a
/// genuine in-ref compile error never gets mislabeled as drift.
///
/// `container` is `<workspace_root>/.spawn-<ref>/`. `workspace_root` is its
/// parent; the shared schemas checkout is `<workspace_root>/qontinui-schemas`.
pub fn classify_drift(error: &str, container: &Path) -> Option<String> {
    let workspace_root = container.parent()?;
    let shared_schemas = workspace_root.join("qontinui-schemas");
    let shared_schemas_str = shared_schemas.to_string_lossy().replace('\\', "/");
    let container_str = container.to_string_lossy().replace('\\', "/");

    // Normalize the haystack so Windows backslash paths match the same way.
    let normalized = error.replace('\\', "/");

    // Direct signal: the SHARED schemas checkout appears in the error AND the
    // reference is not inside the container (the pinned worktree is inside the
    // container so a legitimate pinned-schemas error wouldn't trip this).
    if let Some(idx) = normalized.find(&shared_schemas_str) {
        // Make sure the matched occurrence isn't actually the container's own
        // (nested) `qontinui-schemas` worktree — that path starts with the
        // container prefix, so a substring search for the shared path would
        // not match it (different prefix). The find above is therefore the
        // shared checkout. Extract a readable snippet for the message.
        let snippet: String = normalized[idx..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        return Some(snippet);
    }

    // Secondary signal: any token that looks like an absolute path the build
    // touched but that is neither inside the container nor a relative path.
    // Only fires when the workspace root itself is referenced outside the
    // container (e.g. a stray `<workspace_root>/qontinui-runner/...`).
    let ws_str = workspace_root.to_string_lossy().replace('\\', "/");
    for token in normalized.split_whitespace() {
        let tok = token.trim_matches(|c: char| c == '`' || c == '\'' || c == '"' || c == ',');
        if tok.starts_with(&ws_str)
            && !tok.starts_with(&container_str)
            && (tok.contains("/qontinui-schemas/") || tok.contains("/qontinui-runner/"))
        {
            return Some(tok.to_string());
        }
    }

    None
}

/// A prepared, isolated detached worktree at `git_ref`.
///
/// Materializes the nested-container layout described in the module docs:
/// a runner worktree at the requested ref plus a pinned `qontinui-schemas`
/// worktree at `origin/main`, both under `<workspace_root>/.spawn-<ref>/`.
pub struct PreparedWorktree {
    /// Per-ref container dir (`<workspace_root>/.spawn-<ref>/`). Parent of
    /// both the runner and schemas worktrees. This is what cleanup removes
    /// last, after both worktree registrations are pruned.
    pub container_path: PathBuf,
    /// Runner worktree root (`<container>/qontinui-runner`, parent of
    /// `src-tauri`). Frontend prebuild + dist live here.
    pub worktree_path: PathBuf,
    /// `<worktree_path>/src-tauri` — cargo's `current_dir` for the build.
    pub src_tauri: PathBuf,
    /// Pinned schemas worktree root (`<container>/qontinui-schemas`). The
    /// runner crate's `../../qontinui-schemas/...` path-deps resolve here.
    pub schemas_path: PathBuf,
    /// The ref the caller requested (echoed back verbatim).
    pub requested_ref: String,
    /// `git rev-parse HEAD` of the prepared runner worktree.
    pub resolved_sha: String,
}

/// Best-effort removal of a single registered worktree from `repo_root`,
/// then a `prune` so the repo's `worktree` registration doesn't leak. Logs
/// (via `tracing`) on failure but never propagates — cleanup is a side
/// effect, the caller's original error must stay the user-visible one.
async fn remove_worktree_registration(repo_root: &Path, wt_path: &Path) {
    let wt_path_str = wt_path.to_string_lossy().to_string();
    if let Err(e) = git(
        &["worktree", "remove", "--force", &wt_path_str],
        repo_root,
        false,
    )
    .await
    {
        tracing::warn!(
            "spawn-worktree cleanup: `git worktree remove --force {:?}` (in {:?}) failed: {} \
             (continuing — the original error is what the caller sees)",
            wt_path,
            repo_root,
            e
        );
    }
    if let Err(e) = git(&["worktree", "prune"], repo_root, false).await {
        tracing::warn!(
            "spawn-worktree cleanup: `git worktree prune` (in {:?}) failed: {} \
             (continuing — the original error is what the caller sees)",
            repo_root,
            e
        );
    }
}

/// Best-effort cleanup of a freshly-prepared spawn container whose follow-up
/// operations failed. Removes BOTH the runner worktree (registered in the
/// runner `repo_root`) and the pinned schemas worktree (registered in the
/// shared `schemas_repo_root`), prunes both repos, then removes the now-empty
/// container dir. Never propagates — the caller's original error stays the
/// user-visible one.
///
/// `schemas_repo_root` is `None` when the schemas worktree was never created
/// (the failure happened before that step), in which case only the runner
/// side is cleaned. The container dir is removed last via a recursive delete
/// so leftover empty repo metadata dirs (`.git` worktree gitdir files are
/// removed by the prune above) don't block it.
///
/// Public so the `spawn-test` route handler can invoke it via the
/// `cleanup_worktree_on_fail` request flag after a downstream cargo build
/// failure (the in-function call below only covers errors raised inside
/// `prepare_worktree` itself).
pub async fn cleanup_fresh_worktree(
    repo_root: &Path,
    container_path: &Path,
    runner_wt_path: &Path,
    schemas_repo_root: Option<&Path>,
    schemas_wt_path: Option<&Path>,
) {
    remove_worktree_registration(repo_root, runner_wt_path).await;
    if let (Some(srepo), Some(swt)) = (schemas_repo_root, schemas_wt_path) {
        remove_worktree_registration(srepo, swt).await;
    }
    // Drop the container dir itself. `git worktree remove` already deleted the
    // worktree subdirs; this mops up the container shell (and any residue).
    if container_path.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(container_path).await {
            tracing::warn!(
                "spawn-worktree cleanup: failed to remove container {:?}: {} \
                 (continuing — the original error is what the caller sees)",
                container_path,
                e
            );
        }
    }
}

/// Ref the pinned schemas worktree is always checked out at. Centralized so
/// the fresh-add, reuse, and tests agree.
const SCHEMAS_PIN_REF: &str = "origin/main";

/// Detect a stale FLAT-layout spawn dir from before the nested-container
/// migration. The old layout put the runner worktree's `src-tauri` directly
/// at `<container>/src-tauri`; the new layout puts it at
/// `<container>/qontinui-runner/src-tauri`. A dir with `src-tauri/` directly
/// inside but NO `qontinui-runner/` subdir is the old layout and must be
/// rebuilt (delete + recreate) rather than reused.
fn is_old_flat_layout(container: &Path) -> bool {
    container.join("src-tauri").exists() && !container.join("qontinui-runner").exists()
}

/// Ensure a managed, *isolated* detached worktree set for `git_ref` exists.
/// Idempotent: a subsequent call for the same ref reuses the existing
/// container, force-resets the runner worktree to the ref, and re-pins the
/// schemas worktree to `origin/main`.
///
/// Layout (see module docs for the rationale):
///
/// ```text
/// <workspace_root>/.spawn-<sanitized_ref>/   ← container
///   qontinui-runner/                         ← detached worktree at git_ref
///   qontinui-schemas/                        ← detached worktree at origin/main
/// ```
///
/// The runner worktree is created from the runner repo (`repo_root`); the
/// schemas worktree from the SHARED `<workspace_root>/qontinui-schemas` repo,
/// pinned to `origin/main`, so the runner crate's `../../qontinui-schemas/...`
/// path-deps resolve INSIDE the container and never touch the shared
/// checkout's working-tree branch.
///
/// On the *fresh-add* path, if any post-add step fails this function
/// best-effort cleans up BOTH worktrees + the container dir before returning.
/// On the *reuse* path the container is left in place on a later failure (the
/// caller may have ongoing state there) — but an OLD FLAT-layout container is
/// always torn down and rebuilt first.
pub async fn prepare_worktree(
    project_dir: &Path,
    git_ref: &str,
) -> Result<PreparedWorktree, SupervisorError> {
    if git_ref.trim().is_empty() {
        return Err(SupervisorError::Validation(
            "git_ref must be a non-empty git ref".to_string(),
        ));
    }

    let repo_root = find_repo_root(project_dir)?;
    let workspace_root = derive_workspace_root(project_dir)?;
    let schemas_repo_root = workspace_root.join("qontinui-schemas");
    let sanitized = sanitize_ref(git_ref);

    // Per-ref container holding both nested worktrees.
    let container: PathBuf = workspace_root.join(format!(".spawn-{}", sanitized));
    let runner_wt: PathBuf = container.join("qontinui-runner");
    let schemas_wt: PathBuf = container.join("qontinui-schemas");

    // Best-effort fetch on the runner repo so the ref is resolvable even if it
    // was pushed after this supervisor started. Offline ⇒ tolerated.
    git(&["fetch", "--quiet", "origin"], &repo_root, true).await?;

    // Migrate an old flat-layout container (src-tauri directly inside) to the
    // nested layout: tear it down (pruning the leaked runner worktree
    // registration) and fall through to a fresh add. Pre-migration containers
    // never created a schemas worktree, so only the runner side needs pruning.
    if container.exists() && is_old_flat_layout(&container) {
        tracing::info!(
            "spawn-worktree: migrating old flat-layout container {:?} to nested layout",
            container
        );
        // The flat worktree was registered at the container path itself.
        remove_worktree_registration(&repo_root, &container).await;
        if container.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&container).await {
                return Err(SupervisorError::Process(format!(
                    "failed to remove old flat-layout spawn container {:?} during migration: {}",
                    container, e
                )));
            }
        }
    }

    let was_fresh = !container.exists();
    if was_fresh {
        // Fresh add: create the runner worktree at the ref and the schemas
        // worktree pinned to origin/main. `git worktree add --detach <path>
        // <ref>` creates the intermediate `<container>/` dir as needed.
        let runner_wt_str = runner_wt.to_string_lossy().to_string();
        git(
            &["worktree", "add", "--detach", &runner_wt_str, git_ref],
            &repo_root,
            false,
        )
        .await?;
    } else {
        // Reuse: refresh + hard-pin the runner worktree to the ref.
        git(&["fetch", "--quiet", "origin"], &runner_wt, true).await?;
        git(
            &["checkout", "--detach", "--force", git_ref],
            &runner_wt,
            false,
        )
        .await?;
        git(&["reset", "--hard", git_ref], &runner_wt, false).await?;
    }

    // Prepare (or re-pin) the schemas worktree. Done for BOTH fresh and reuse
    // paths: on reuse the shared schemas checkout may have advanced, so we
    // always fetch + force-pin the worktree to origin/main. Wrapped so a
    // failure on the fresh path triggers full cleanup below.
    let prepare_schemas = async {
        // Best-effort fetch on the shared schemas repo so origin/main is current.
        git(
            &["fetch", "--quiet", "origin", "main"],
            &schemas_repo_root,
            true,
        )
        .await?;
        if !schemas_wt.exists() {
            let schemas_wt_str = schemas_wt.to_string_lossy().to_string();
            git(
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &schemas_wt_str,
                    SCHEMAS_PIN_REF,
                ],
                &schemas_repo_root,
                false,
            )
            .await?;
        } else {
            // Re-pin an existing schemas worktree to origin/main.
            git(&["fetch", "--quiet", "origin", "main"], &schemas_wt, true).await?;
            git(
                &["checkout", "--detach", "--force", SCHEMAS_PIN_REF],
                &schemas_wt,
                false,
            )
            .await?;
            git(&["reset", "--hard", SCHEMAS_PIN_REF], &schemas_wt, false).await?;
        }
        Ok::<(), SupervisorError>(())
    };

    // Run schemas prep + post-add validation in an inner async block so a
    // single match arm can wire cleanup-on-error for the fresh-add path.
    let finalize = async {
        prepare_schemas.await?;

        let resolved_sha = git(&["rev-parse", "HEAD"], &runner_wt, false).await?;
        let src_tauri = runner_wt.join("src-tauri");
        if !src_tauri.exists() {
            return Err(SupervisorError::Process(format!(
                "prepared runner worktree {:?} has no src-tauri/ — is {:?} a runner repo?",
                runner_wt, git_ref
            )));
        }
        // The runner crate's `../../qontinui-schemas/rust` must now resolve
        // inside the container. Validate it so a layout regression fails loudly
        // here rather than as a confusing cargo path-dep error mid-build.
        let pinned_rust = schemas_wt.join("rust");
        if !pinned_rust.exists() {
            return Err(SupervisorError::Process(format!(
                "pinned schemas worktree {:?} has no rust/ — `../../qontinui-schemas/rust` \
                 path-dep would not resolve inside the spawn container",
                schemas_wt
            )));
        }
        Ok(PreparedWorktree {
            container_path: container.clone(),
            worktree_path: runner_wt.clone(),
            src_tauri,
            schemas_path: schemas_wt.clone(),
            requested_ref: git_ref.to_string(),
            resolved_sha,
        })
    };

    match finalize.await {
        Ok(prepared) => Ok(prepared),
        Err(e) => {
            if was_fresh {
                cleanup_fresh_worktree(
                    &repo_root,
                    &container,
                    &runner_wt,
                    Some(&schemas_repo_root),
                    Some(&schemas_wt),
                )
                .await;
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_ref_replaces_unsafe_chars() {
        assert_eq!(sanitize_ref("origin/main"), "origin_main");
        assert_eq!(sanitize_ref("feat/spawn-test"), "feat_spawn-test");
        assert_eq!(sanitize_ref("refs/heads/foo@{1}"), "refs_heads_foo__1_");
        assert_eq!(sanitize_ref("v1.2.3"), "v1.2.3");
        assert_eq!(sanitize_ref("a_b-c.d"), "a_b-c.d");
        // pure-unsafe input never yields an empty component
        assert_eq!(sanitize_ref("///"), "___");
        assert_eq!(sanitize_ref(""), "_");
    }

    #[test]
    fn classify_drift_flags_shared_schemas_outside_container() {
        // A cargo error referencing the SHARED schemas checkout (sibling of
        // the container) is residual drift — must be flagged.
        let ws = Path::new("/ws");
        let container = ws.join(".spawn-origin_main");
        let err = "error[E0432]: unresolved import\n  --> /ws/qontinui-schemas/rust/lib.rs:1:5";
        let got =
            classify_drift(err, &container).expect("shared schemas ref must classify as drift");
        assert!(
            got.contains("/ws/qontinui-schemas/rust/lib.rs"),
            "offending path should be the shared schemas file; got {}",
            got
        );
    }

    #[test]
    fn classify_drift_ignores_in_container_errors() {
        // A genuine compile error inside the spawn container (the runner
        // worktree OR the pinned schemas worktree) must NOT be flagged — the
        // caller's ref really did fail to compile.
        let ws = Path::new("/ws");
        let container = ws.join(".spawn-origin_main");
        let in_runner = format!(
            "error[E0277]: trait bound not satisfied\n  --> {}/qontinui-runner/src-tauri/src/main.rs:9:1",
            container.to_string_lossy()
        );
        assert!(
            classify_drift(&in_runner, &container).is_none(),
            "in-container runner error must not classify as drift"
        );
        let in_pinned = format!(
            "error[E0412]: cannot find type\n  --> {}/qontinui-schemas/rust/lib.rs:3:1",
            container.to_string_lossy()
        );
        assert!(
            classify_drift(&in_pinned, &container).is_none(),
            "in-container pinned-schemas error must not classify as drift"
        );
    }

    #[test]
    fn classify_drift_handles_windows_backslash_paths() {
        let ws = Path::new("D:/qontinui-root");
        let container = ws.join(".spawn-origin_main");
        let err = "error: failed to load source for dependency\n  D:\\qontinui-root\\qontinui-schemas\\rust\\Cargo.toml";
        let got = classify_drift(err, &container)
            .expect("backslash shared-schemas path must classify as drift");
        assert!(got.contains("qontinui-schemas/rust/Cargo.toml"));
    }

    #[test]
    fn sanitize_ref_is_stable() {
        let r = "origin/feature/long-branch-name";
        assert_eq!(sanitize_ref(r), sanitize_ref(r));
    }

    #[test]
    fn find_repo_root_errors_when_no_git() {
        // A temp dir with no .git anywhere up to the FS root.
        let tmp = std::env::temp_dir().join("qsup-no-git-xyz-test");
        let _ = std::fs::create_dir_all(&tmp);
        let res = find_repo_root(&tmp);
        // Either errors (no .git) — acceptable. If some ancestor of the
        // system temp dir happens to be a git repo we don't assert the
        // negative; we only assert the error path is well-formed.
        if let Err(e) = res {
            assert!(e.to_string().contains("git repo root"));
        }
    }

    // ---------- workspace-root derivation + placement fixture ----------
    //
    // These tests construct a tempdir simulating the real workspace layout
    // (`<root>/qontinui-runner/...` + `<root>/qontinui-schemas/...`) and
    // exercise the workspace-root derivation + the actual worktree placement.
    // The placement test is the direct empirical guard against the P1 bug:
    // it asserts that from inside the spawn worktree, the runner's
    // upward-relative path-dep `../../qontinui-schemas/rust` resolves to an
    // existing file in the workspace root.

    use std::process::Command as StdCommand;

    /// Run `git <args>` in `cwd`, asserting success.
    fn git_in(cwd: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} (in {:?}) failed: stderr={}",
            args,
            cwd,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `git init` a real repo at `dir` with the given seed file and one
    /// commit on `main`, then add an `origin` remote pointing at the repo
    /// itself and `fetch` so `origin/main` resolves (needed by the schemas
    /// pin path, which checks out `origin/main`).
    fn init_repo_with_self_origin(dir: &Path) {
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "test@example.com"]);
        git_in(dir, &["config", "user.name", "test"]);
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "initial"]);
        // Self-remote so `origin/main` is a resolvable ref for worktree pins.
        let dir_str = dir.to_string_lossy().to_string();
        git_in(dir, &["remote", "add", "origin", &dir_str]);
        git_in(dir, &["fetch", "-q", "origin"]);
    }

    /// Build the realistic two-sibling layout under `<base>`:
    ///
    /// ```text
    /// <base>/
    ///   qontinui-schemas/             (a real git repo, origin/main resolvable)
    ///     rust/Cargo.toml
    ///     rust-vision-core/Cargo.toml
    ///   qontinui-runner/              (a real git repo with one commit)
    ///     src-tauri/Cargo.toml        (path = "../../qontinui-schemas/rust")
    /// ```
    ///
    /// Returns `(workspace_root, runner_src_tauri)`.
    fn build_workspace_fixture(base: &Path) -> (PathBuf, PathBuf) {
        // --- schemas repo (its own git repo so a detached worktree can be
        // added from it, pinned at origin/main) ---
        let schemas = base.join("qontinui-schemas");
        let schemas_rust = schemas.join("rust");
        std::fs::create_dir_all(&schemas_rust).expect("mkdir schemas/rust");
        std::fs::write(
            schemas_rust.join("Cargo.toml"),
            "[package]\nname = \"qontinui-types\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"lib.rs\"\n",
        )
        .expect("write schemas Cargo.toml");
        std::fs::write(schemas_rust.join("lib.rs"), "").expect("write schemas lib.rs");
        let schemas_vision = schemas.join("rust-vision-core");
        std::fs::create_dir_all(&schemas_vision).expect("mkdir schemas/rust-vision-core");
        std::fs::write(
            schemas_vision.join("Cargo.toml"),
            "[package]\nname = \"qontinui-vision-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"lib.rs\"\n",
        )
        .expect("write vision Cargo.toml");
        std::fs::write(schemas_vision.join("lib.rs"), "").expect("write vision lib.rs");
        init_repo_with_self_origin(&schemas);

        // --- runner repo ---
        let runner = base.join("qontinui-runner");
        let src_tauri = runner.join("src-tauri");
        std::fs::create_dir_all(&src_tauri).expect("mkdir runner/src-tauri");
        std::fs::write(
            src_tauri.join("Cargo.toml"),
            "[package]\nname = \"qontinui-runner\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [dependencies]\n\
             qontinui-types = { path = \"../../qontinui-schemas/rust\" }\n",
        )
        .expect("write runner src-tauri Cargo.toml");
        init_repo_with_self_origin(&runner);

        (base.to_path_buf(), src_tauri)
    }

    #[test]
    fn derive_workspace_root_finds_sibling_layout() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let got = derive_workspace_root(&src_tauri).expect("derive ws root");
        // Canonicalize both sides — tempfile may hand back a path with the
        // platform's canonical prefix that differs from `tmp.path()`.
        assert_eq!(
            std::fs::canonicalize(&got).unwrap(),
            std::fs::canonicalize(&root).unwrap(),
            "workspace root should be the tempdir base"
        );
    }

    #[test]
    fn derive_workspace_root_errors_on_malformed_layout() {
        // Only one of the two required siblings present — schemas missing.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let runner_src_tauri = tmp.path().join("qontinui-runner").join("src-tauri");
        std::fs::create_dir_all(&runner_src_tauri).expect("mkdir");
        let res = derive_workspace_root(&runner_src_tauri);
        let err = res.expect_err("must error when qontinui-schemas is missing");
        let s = err.to_string();
        assert!(
            s.contains("workspace root") && s.contains("qontinui-schemas"),
            "error should name the missing invariant; got: {}",
            s
        );
    }

    #[tokio::test]
    async fn prepare_worktree_nests_runner_and_pins_schemas_so_path_deps_resolve() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());

        let prepared = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("prepare_worktree HEAD");

        // 1. Nested layout: container at <ws>/.spawn-HEAD/, runner worktree
        //    one level deeper at <container>/qontinui-runner/.
        let expected_container = root.join(format!(".spawn-{}", sanitize_ref("HEAD")));
        assert_eq!(
            std::fs::canonicalize(&prepared.container_path).unwrap(),
            std::fs::canonicalize(&expected_container).unwrap(),
            "container must sit at <workspace_root>/.spawn-<ref>/"
        );
        assert_eq!(
            std::fs::canonicalize(&prepared.worktree_path).unwrap(),
            std::fs::canonicalize(expected_container.join("qontinui-runner")).unwrap(),
            "runner worktree must be nested at <container>/qontinui-runner/"
        );

        // 2. The runner's upward-relative path-dep must resolve — and it must
        //    resolve INSIDE the container (the pinned schemas worktree), NOT
        //    the shared <workspace_root>/qontinui-schemas checkout.
        //    From <container>/qontinui-runner/src-tauri/,
        //    "../../qontinui-schemas/rust/Cargo.toml" is the exact path string.
        let path_dep_target = prepared
            .src_tauri
            .join("..")
            .join("..")
            .join("qontinui-schemas")
            .join("rust")
            .join("Cargo.toml");
        let canon = std::fs::canonicalize(&path_dep_target)
            .expect("upward-relative path-dep must resolve to a real file");
        let canon_pinned = std::fs::canonicalize(prepared.schemas_path.join("rust"))
            .expect("pinned schemas rust/ exists");
        assert!(
            canon.starts_with(&canon_pinned),
            "resolved path-dep {:?} must live inside the PINNED schemas worktree {:?} \
             (isolation), not the shared checkout",
            canon,
            canon_pinned
        );
        // And the pinned worktree must be inside the container, not the shared
        // <ws>/qontinui-schemas checkout.
        let canon_container = std::fs::canonicalize(&prepared.container_path).unwrap();
        assert!(
            std::fs::canonicalize(&prepared.schemas_path)
                .unwrap()
                .starts_with(&canon_container),
            "pinned schemas worktree must live inside the spawn container (isolation)"
        );

        // 3. PreparedWorktree fields are well-formed.
        assert_eq!(prepared.requested_ref, "HEAD");
        assert!(!prepared.resolved_sha.is_empty());
        assert!(prepared.src_tauri.ends_with("src-tauri"));

        // Best-effort cleanup so the tempdir's inner repos don't leak worktree
        // registrations (they live only inside `tmp`, so this is local).
        let schemas_repo = root.join("qontinui-schemas");
        cleanup_fresh_worktree(
            &find_repo_root(&src_tauri).unwrap(),
            &prepared.container_path,
            &prepared.worktree_path,
            Some(&schemas_repo),
            Some(&prepared.schemas_path),
        )
        .await;
    }

    #[tokio::test]
    async fn prepare_worktree_reuse_repins_existing_container() {
        // Two calls on the same ref must reuse the container (idempotent) and
        // re-pin both worktrees. The second call must not error.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());

        let first = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("first prepare");
        let second = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("second prepare (reuse) must succeed");

        assert_eq!(
            std::fs::canonicalize(&first.container_path).unwrap(),
            std::fs::canonicalize(&second.container_path).unwrap(),
            "reuse must hit the same container dir"
        );
        assert!(second.schemas_path.join("rust").exists());

        let schemas_repo = root.join("qontinui-schemas");
        cleanup_fresh_worktree(
            &find_repo_root(&src_tauri).unwrap(),
            &second.container_path,
            &second.worktree_path,
            Some(&schemas_repo),
            Some(&second.schemas_path),
        )
        .await;
    }

    #[tokio::test]
    async fn prepare_worktree_migrates_old_flat_layout() {
        // Simulate a pre-migration FLAT container: a runner worktree added
        // directly at <ws>/.spawn-HEAD/ (src-tauri directly inside, no nested
        // qontinui-runner/). prepare_worktree must detect it, tear it down,
        // and rebuild into the nested layout.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let repo_root = find_repo_root(&src_tauri).expect("repo root");

        let container = root.join(format!(".spawn-{}", sanitize_ref("HEAD")));
        // Old flat layout: worktree registered AT the container path.
        git_in(
            &repo_root,
            &[
                "worktree",
                "add",
                "--detach",
                container.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert!(
            is_old_flat_layout(&container),
            "fixture must produce an old flat-layout container (src-tauri directly inside)"
        );

        let prepared = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("prepare_worktree must migrate the flat layout");

        // After migration the runner worktree is nested.
        assert_eq!(
            std::fs::canonicalize(&prepared.worktree_path).unwrap(),
            std::fs::canonicalize(container.join("qontinui-runner")).unwrap(),
            "migrated container must use the nested qontinui-runner/ layout"
        );
        assert!(
            !is_old_flat_layout(&prepared.container_path),
            "post-migration container must not look like the old flat layout"
        );

        let schemas_repo = root.join("qontinui-schemas");
        cleanup_fresh_worktree(
            &repo_root,
            &prepared.container_path,
            &prepared.worktree_path,
            Some(&schemas_repo),
            Some(&prepared.schemas_path),
        )
        .await;
    }

    #[tokio::test]
    async fn cleanup_fresh_worktree_removes_container() {
        // Direct exercise of the cleanup helper: prepare a real isolated
        // worktree set, call cleanup, assert the whole container is gone.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let repo_root = find_repo_root(&src_tauri).expect("find repo root");
        let schemas_repo = root.join("qontinui-schemas");

        let prepared = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("prepare_worktree HEAD");
        assert!(prepared.container_path.exists(), "setup: container exists");

        cleanup_fresh_worktree(
            &repo_root,
            &prepared.container_path,
            &prepared.worktree_path,
            Some(&schemas_repo),
            Some(&prepared.schemas_path),
        )
        .await;

        assert!(
            !prepared.container_path.exists(),
            "cleanup_fresh_worktree must remove the container dir; still present: {:?}",
            prepared.container_path
        );
    }
}

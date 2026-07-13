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

/// A validated, caller-owned worktree the supervisor will build *as-is*.
///
/// Returned by [`validate_existing_worktree`] for the `worktree_path`
/// provenance selector. **Deliberately NOT a [`PreparedWorktree`]:** that
/// struct carries the non-optional `container_path` + `schemas_path` fields
/// that exist precisely to feed `SpawnCleanupPaths` / `cleanup_fresh_worktree`.
/// A caller-owned checkout must NEVER enter the cleanup machinery (the caller,
/// not the supervisor, created and owns it), so this is a distinct type with no
/// cleanup paths — the "never clean up a caller-owned tree" invariant is
/// enforced at the type level, not by a runtime flag check.
#[derive(Debug)]
pub struct ValidatedExistingWorktree {
    /// Canonicalized worktree root the caller supplied (parent of `src-tauri`).
    pub worktree_path: PathBuf,
    /// `<worktree_path>/src-tauri` — cargo's `current_dir` for the build (fed
    /// to `run_cargo_build_with_dir` as the `build_dir_override`).
    pub src_tauri: PathBuf,
    /// `git -C <worktree_path> rev-parse HEAD`, best-effort. `None` when the
    /// tree isn't a git checkout (a non-repo tree still builds).
    pub resolved_sha: Option<String>,
    /// Canonicalized `../qontinui-schemas` sibling the runner crate's
    /// `../../qontinui-schemas/...` path-deps resolve to. `None` only when
    /// canonicalization of the (already-validated-present) sibling fails.
    pub schemas_path: Option<PathBuf>,
    /// `git -C <schemas_path> rev-parse HEAD`, best-effort.
    pub schemas_sha: Option<String>,
    /// True iff the resolved schemas sibling is the SHARED workspace-root
    /// `qontinui-schemas` checkout (the drift hazard the pinned-schemas
    /// containers eliminate for managed worktrees). Best-effort `false` when
    /// the workspace root can't be derived.
    pub schemas_is_shared: bool,
}

/// Best-effort `git -C <dir> rev-parse HEAD`. Returns `None` on any failure
/// (no git, not a repo, detached/empty), so a non-repo tree still builds with
/// `resolved_sha = None`.
async fn rev_parse_head_best_effort(dir: &Path) -> Option<String> {
    let dir_str = dir.to_string_lossy().to_string();
    git(&["-C", &dir_str, "rev-parse", "HEAD"], dir, false)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// Validate a caller-owned existing worktree/checkout for the `worktree_path`
/// provenance selector of `POST /runners/spawn-test`.
///
/// Unlike [`prepare_worktree`] this NEVER materializes or mutates anything —
/// the tree already exists and the caller owns it. Each failure is a precise
/// [`SupervisorError::Validation`] (→ HTTP 400) so a bad path never silently
/// falls back to the live tree.
///
/// Checks (in order):
/// 1. `worktree_path` exists and is a directory (canonicalized).
/// 2. It is NOT the live repo root (`worktree_path_is_live_tree`) — the new
///    param can never be used to mutate/rebuild the live tree.
/// 3. `<wt>/src-tauri/Cargo.toml` exists (`not_a_runner_worktree`).
/// 4. `<wt>/../qontinui-schemas/rust/Cargo.toml` exists — ONE level above the
///    worktree root, because the path-deps are `../../qontinui-schemas/rust`
///    relative to `src-tauri/` (`path_deps_unresolved`).
///
/// Then records schemas provenance (best-effort, never fails the call):
/// `schemas_path`, `schemas_sha`, `schemas_is_shared`, and the worktree's own
/// `resolved_sha`.
pub async fn validate_existing_worktree(
    worktree_path: &Path,
    live_repo_root: &Path,
) -> Result<ValidatedExistingWorktree, SupervisorError> {
    // 1. Exists + is a dir. Canonicalize so the live-tree comparison and the
    //    echoed path are stable absolute paths.
    let canon_wt = tokio::fs::canonicalize(worktree_path).await.map_err(|e| {
        SupervisorError::Validation(format!(
            "worktree_path {:?} does not exist or is not accessible: {}",
            worktree_path, e
        ))
    })?;
    let meta = tokio::fs::metadata(&canon_wt).await.map_err(|e| {
        SupervisorError::Validation(format!(
            "worktree_path {:?} is not accessible: {}",
            canon_wt, e
        ))
    })?;
    if !meta.is_dir() {
        return Err(SupervisorError::Validation(format!(
            "worktree_path {:?} is not a directory",
            canon_wt
        )));
    }

    // 2. Must NOT be the live tree. Compare canonicalized forms; if the live
    //    repo root can't be canonicalized (shouldn't happen — it's the running
    //    supervisor's project), fall back to a direct compare.
    let canon_live = tokio::fs::canonicalize(live_repo_root)
        .await
        .unwrap_or_else(|_| live_repo_root.to_path_buf());
    if canon_wt == canon_live {
        return Err(SupervisorError::Validation(format!(
            "worktree_path_is_live_tree: {:?} is the live runner tree — omit \
             worktree_path (and git_ref) to build the live tree; worktree_path \
             is only for an ISOLATED caller-owned checkout, never the live tree",
            canon_wt
        )));
    }

    // 3. Must look like a runner worktree: src-tauri/Cargo.toml present.
    let src_tauri = canon_wt.join("src-tauri");
    let runner_manifest = src_tauri.join("Cargo.toml");
    if !runner_manifest.exists() {
        return Err(SupervisorError::Validation(format!(
            "not_a_runner_worktree: {:?} has no src-tauri/Cargo.toml — \
             worktree_path must point at a qontinui-runner checkout root",
            canon_wt
        )));
    }

    // 4. Path-deps must resolve. The runner crate declares
    //    `qontinui-types = {{ path = \"../../qontinui-schemas/rust\" }}` relative
    //    to src-tauri/, i.e. the SIBLING of the worktree root — ONE level up
    //    from <wt>, not two. Validate that sibling's rust/Cargo.toml exists.
    let schemas_sibling = match canon_wt.parent() {
        Some(p) => p.join("qontinui-schemas"),
        None => {
            return Err(SupervisorError::Validation(format!(
                "path_deps_unresolved: worktree_path {:?} has no parent dir, so \
                 the `../../qontinui-schemas/rust` path-dep cannot resolve",
                canon_wt
            )));
        }
    };
    let schemas_rust_manifest = schemas_sibling.join("rust").join("Cargo.toml");
    if !schemas_rust_manifest.exists() {
        return Err(SupervisorError::Validation(format!(
            "path_deps_unresolved: expected the runner's `../../qontinui-schemas/rust` \
             path-dep to resolve to {:?}, but it does not exist. The schemas checkout \
             must sit beside the worktree root (one level up): <worktree_path>/../qontinui-schemas",
            schemas_rust_manifest
        )));
    }

    // --- Provenance (best-effort; never fails the validation). ---
    let resolved_sha = rev_parse_head_best_effort(&canon_wt).await;

    let schemas_path = tokio::fs::canonicalize(&schemas_sibling).await.ok();
    let schemas_sha = match &schemas_path {
        Some(p) => rev_parse_head_best_effort(p).await,
        None => None,
    };

    // `schemas_is_shared`: the resolved sibling equals the workspace-root
    // shared `qontinui-schemas` checkout (the drift hazard). Best-effort false
    // when the workspace root can't be derived from the live project dir.
    let schemas_is_shared = match (&schemas_path, derive_workspace_root(live_repo_root).ok()) {
        (Some(sp), Some(ws)) => match tokio::fs::canonicalize(ws.join("qontinui-schemas")).await {
            Ok(shared_canon) => *sp == shared_canon,
            Err(_) => false,
        },
        _ => false,
    };

    Ok(ValidatedExistingWorktree {
        worktree_path: canon_wt,
        src_tauri,
        resolved_sha,
        schemas_path,
        schemas_sha,
        schemas_is_shared,
    })
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

/// The `.spawn-<sanitized_ref>` container dir [`prepare_worktree`] materializes
/// for `git_ref` — `<workspace_root>/.spawn-<sanitized_ref>/`.
///
/// Names the container WITHOUT touching git or creating anything (the only I/O
/// is [`derive_workspace_root`]'s sibling-dir probe), so a caller can label a
/// build with its true source root before the worktree exists.
/// `prepare_worktree` itself derives the container through this function, so the
/// two can never disagree.
pub fn container_path_for_ref(
    project_dir: &Path,
    git_ref: &str,
) -> Result<PathBuf, SupervisorError> {
    if git_ref.trim().is_empty() {
        return Err(SupervisorError::Validation(
            "git_ref must be a non-empty git ref".to_string(),
        ));
    }
    let workspace_root = derive_workspace_root(project_dir)?;
    Ok(workspace_root.join(format!("{}{}", SPAWN_DIR_PREFIX, sanitize_ref(git_ref))))
}

/// The runner worktree root inside the container for `git_ref` —
/// `<workspace_root>/.spawn-<sanitized_ref>/qontinui-runner/`. Parent of the
/// `src-tauri` dir cargo compiles. See [`container_path_for_ref`].
pub fn runner_worktree_path_for_ref(
    project_dir: &Path,
    git_ref: &str,
) -> Result<PathBuf, SupervisorError> {
    Ok(container_path_for_ref(project_dir, git_ref)?.join("qontinui-runner"))
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

    // Per-ref container holding both nested worktrees. Derived through the
    // shared helper so the ONE place that knows the layout formula is
    // [`container_path_for_ref`] — callers that need to name the container
    // before it is materialized (e.g. the spawn-test handler labelling a build
    // submission's source root) cannot drift from what this function creates.
    let container: PathBuf = container_path_for_ref(project_dir, git_ref)?;
    let runner_wt: PathBuf = runner_worktree_path_for_ref(project_dir, git_ref)?;
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

// ---------------------------------------------------------------------------
// `.spawn-*` scratch-worktree pruning (deferred #64 Phase 4)
// ---------------------------------------------------------------------------
//
// `prepare_worktree` creates supervisor-owned scratch containers named
// `.spawn-<sanitized-ref>` directly under `<workspace_root>`. They are full
// detached checkouts (GBs each) and were never cleaned, so a dev machine
// accumulates them. This pruner removes the SAFE ones.
//
// Safety is the whole point — getting it wrong deletes a checkout. The
// predicate is intentionally conservative and layered (see
// [`prune_spawn_worktrees`]). The three never-touch invariants are enforced
// structurally, not by comment:
//
//   1. Only directories whose name starts with the supervisor's own
//      `SPAWN_DIR_PREFIX`, and only directly under the derived workspace root
//      where the supervisor itself creates them. A caller-owned `worktree_path`
//      tree lives elsewhere and never matches this prefix-under-root filter.
//   2. Never a container that is the build tree of a currently-active build
//      (the caller passes the active set).
//   3. A container younger than the retention window is kept; a container that
//      is a *dirty* registered worktree is kept until DOUBLE the window (a peer
//      may have adopted the scratch tree and made edits).

/// Directory-name prefix of every supervisor-created scratch worktree
/// container. Centralized so [`prepare_worktree`] and the pruner agree.
pub const SPAWN_DIR_PREFIX: &str = ".spawn-";

/// Env var overriding the idle-retention window, in hours. Default
/// [`DEFAULT_SPAWN_RETENTION_HOURS`]. `0` prunes every idle container
/// immediately (subject to the dirty-worktree double-retention rule, which a
/// `0` window collapses to "prune dirty too" only once it has also aged out —
/// i.e. with `0`, dirty trees are still pruned, matching "prune all idle").
pub const SPAWN_RETENTION_ENV: &str = "QONTINUI_SPAWN_WORKTREE_RETENTION_HOURS";

/// Default idle-retention window for `.spawn-*` containers: 48h.
pub const DEFAULT_SPAWN_RETENTION_HOURS: u64 = 48;

/// Resolve the configured retention window (in hours) from
/// [`SPAWN_RETENTION_ENV`], falling back to [`DEFAULT_SPAWN_RETENTION_HOURS`].
/// A malformed value falls back to the default (logged once by the caller is
/// unnecessary — we just ignore garbage and keep the safe default).
fn retention_hours() -> u64 {
    match std::env::var(SPAWN_RETENTION_ENV) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .unwrap_or(DEFAULT_SPAWN_RETENTION_HOURS),
        Err(_) => DEFAULT_SPAWN_RETENTION_HOURS,
    }
}

/// Outcome of a single prune sweep — returned so the caller can log a summary
/// and tests can assert on what happened.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Container dirs successfully removed.
    pub removed: Vec<PathBuf>,
    /// Container dirs inspected but kept (with a short reason), e.g. too young,
    /// active build, dirty-and-not-double-aged.
    pub kept: Vec<(PathBuf, String)>,
    /// Container dirs we attempted to remove but failed (logged + skipped).
    pub failed: Vec<(PathBuf, String)>,
}

/// Age of a path relative to `now`, from its filesystem mtime, as a
/// `Duration`. `None` when the metadata/mtime can't be read OR when the mtime
/// is in the future relative to `now` (clock skew) — both treated by the caller
/// as "unknown/zero age ⇒ do not prune", the safe default. `now` is injected so
/// tests can advance the clock deterministically without touching mtimes.
fn dir_age(path: &Path, now: std::time::SystemTime) -> Option<std::time::Duration> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    now.duration_since(mtime).ok()
}

/// True if `dir` is registered as a git worktree of `repo_root` AND has
/// uncommitted changes. Best-effort: any git failure (not a worktree, no git,
/// broken registration) returns `false` — a tree we can't prove is dirty is
/// treated as clean for the double-retention rule, but it is still subject to
/// the normal single-window retention, so this can never *shorten* retention,
/// only extend it when we positively detect edits.
///
/// The runner worktree lives at `<container>/qontinui-runner`; the schemas
/// worktree at `<container>/qontinui-schemas`. We check BOTH — a dirty edit in
/// either nested worktree means "someone may be using this scratch tree."
async fn container_has_dirty_worktree(container: &Path) -> bool {
    for sub in ["qontinui-runner", "qontinui-schemas"] {
        let wt = container.join(sub);
        if !wt.exists() {
            continue;
        }
        let wt_str = wt.to_string_lossy().to_string();
        // `git -C <wt> status --porcelain` — non-empty stdout ⇒ dirty.
        if let Ok(out) = git(&["-C", &wt_str, "status", "--porcelain"], &wt, false).await {
            if !out.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

/// Remove a single `.spawn-*` container: deregister BOTH nested worktrees from
/// their owning repos (`git worktree remove --force`), `prune` the
/// registrations, then delete the container dir. If the worktree registration
/// is already broken, the `remove`/`prune` are best-effort and the final
/// `remove_dir_all` still mops up the directory (the fallback path). Returns
/// `Err(reason)` only when the directory itself could not be removed.
///
/// `repo_root` is the runner repo; `schemas_repo_root` is the shared
/// `<workspace_root>/qontinui-schemas` repo. Both `git worktree remove` calls
/// are best-effort because a stale container may have a dangling registration
/// (the repo was re-cloned, the gitdir file was deleted, etc.) — the
/// authoritative cleanup is the directory delete.
async fn remove_spawn_container(
    container: &Path,
    repo_root: &Path,
    schemas_repo_root: Option<&Path>,
) -> Result<(), String> {
    let runner_wt = container.join("qontinui-runner");
    let schemas_wt = container.join("qontinui-schemas");

    // Deregister the runner worktree, then prune the runner repo.
    remove_worktree_registration(repo_root, &runner_wt).await;
    // Deregister the schemas worktree from the shared schemas repo, if known.
    if let Some(srepo) = schemas_repo_root {
        remove_worktree_registration(srepo, &schemas_wt).await;
    }

    // Authoritative cleanup: delete the container dir. This is the fallback
    // that handles an already-broken worktree whose `git worktree remove`
    // no-op'd above.
    if container.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(container).await {
            return Err(format!("remove_dir_all {:?} failed: {}", container, e));
        }
    }
    Ok(())
}

/// Enumerate and prune stale `.spawn-*` scratch worktree containers.
///
/// `project_dir` is the supervisor's configured runner `src-tauri` (used to
/// derive the workspace root and the owning repos — the SAME derivation
/// `prepare_worktree` uses, so the pruner looks exactly where the supervisor
/// creates these dirs). `active_containers` is the set of container paths a
/// build is currently using (the active-build exclusion). `now` is injected so
/// tests can pin time; production passes `SystemTime::now()`.
///
/// Retention:
/// - A container is "idle" (no active build) AND its mtime is older than the
///   single window ⇒ prunable, unless it is a dirty registered worktree.
/// - A dirty registered worktree requires DOUBLE the window before it is
///   prunable (a peer may have adopted the scratch tree).
///
/// Best-effort throughout: a failure to derive roots, stat a dir, or remove a
/// dir is logged and skipped — this never returns an error and never fails the
/// caller. Returns a [`PruneReport`] for logging/tests.
pub async fn prune_spawn_worktrees(
    project_dir: &Path,
    active_containers: &std::collections::HashSet<PathBuf>,
    now: std::time::SystemTime,
) -> PruneReport {
    // Delegate to the window-parameterized inner fn with no override, so the
    // engine resolves the retention window from env exactly as before. Existing
    // callers (startup sweep, opportunistic sweep) are unchanged.
    prune_spawn_worktrees_with_window(project_dir, active_containers, now, None).await
}

/// Same as [`prune_spawn_worktrees`] but with an explicit retention-window
/// override (in hours). `window_hours_override`:
/// - `None` ⇒ resolve the window from [`SPAWN_RETENTION_ENV`] / the default
///   (the behavior of [`prune_spawn_worktrees`]);
/// - `Some(h)` ⇒ use `h` hours as the single retention window (and `2*h` as the
///   dirty-worktree double-retention window). `Some(0)` prunes every idle
///   container immediately, subject to the dirty double-retention rule.
///
/// Exists so the `DELETE /spawn-worktrees?older_than_hours=<h>` endpoint can
/// pass an operator-chosen window WITHOUT changing the env-derived default for
/// the background sweeps. All selection/safety invariants are identical — only
/// the window source differs.
pub async fn prune_spawn_worktrees_with_window(
    project_dir: &Path,
    active_containers: &std::collections::HashSet<PathBuf>,
    now: std::time::SystemTime,
    window_hours_override: Option<u64>,
) -> PruneReport {
    let mut report = PruneReport::default();

    // Derive the workspace root + owning repos exactly as `prepare_worktree`
    // does. If we can't, there is nothing we can safely prune — bail (no error).
    let workspace_root = match derive_workspace_root(project_dir) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(
                "spawn-prune: cannot derive workspace root from {:?} ({}); skipping sweep",
                project_dir,
                e
            );
            return report;
        }
    };
    let repo_root = match find_repo_root(project_dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                "spawn-prune: cannot find runner repo root from {:?} ({}); skipping sweep",
                project_dir,
                e
            );
            return report;
        }
    };
    let schemas_repo_root = {
        let s = workspace_root.join("qontinui-schemas");
        if s.is_dir() {
            Some(s)
        } else {
            None
        }
    };

    let window_hours = window_hours_override.unwrap_or_else(retention_hours);
    let retention = std::time::Duration::from_secs(window_hours.saturating_mul(3600));
    let double_retention = retention.saturating_mul(2);

    // Enumerate `.spawn-*` dirs directly under the workspace root. Anything
    // that isn't a directory whose name starts with the supervisor's own
    // prefix is structurally ignored (invariant 1).
    let mut entries = match tokio::fs::read_dir(&workspace_root).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(
                "spawn-prune: cannot read workspace root {:?} ({}); skipping sweep",
                workspace_root,
                e
            );
            return report;
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(
                    "spawn-prune: read_dir iteration error: {}; stopping sweep",
                    e
                );
                break;
            }
        };
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Invariant 1: only the supervisor's own scratch prefix, directly under
        // the workspace root. (A bare `.spawn-` with no suffix is still ours.)
        if !name.starts_with(SPAWN_DIR_PREFIX) {
            continue;
        }
        let is_dir = match entry.file_type().await {
            Ok(ft) => ft.is_dir(),
            Err(_) => path.is_dir(),
        };
        if !is_dir {
            continue;
        }

        // Invariant 2: never prune a container with an active build.
        if active_containers.contains(&path) {
            report.kept.push((path.clone(), "active build".to_string()));
            continue;
        }

        // Age check (invariant 3, part A). Unknown age ⇒ keep (safe default).
        let age = match dir_age(&path, now) {
            Some(a) => a,
            None => {
                report
                    .kept
                    .push((path.clone(), "age unknown (mtime unreadable)".to_string()));
                continue;
            }
        };
        if age < retention {
            report.kept.push((
                path.clone(),
                format!("younger than retention ({}h)", retention.as_secs() / 3600),
            ));
            continue;
        }

        // Dirty double-retention (invariant 3, part B). A dirty registered
        // worktree is only prunable once it has aged past DOUBLE the window.
        if container_has_dirty_worktree(&path).await && age < double_retention {
            report.kept.push((
                path.clone(),
                format!(
                    "dirty worktree, awaiting double-retention ({}h)",
                    double_retention.as_secs() / 3600
                ),
            ));
            continue;
        }

        // Safe to remove.
        match remove_spawn_container(&path, &repo_root, schemas_repo_root.as_deref()).await {
            Ok(()) => {
                tracing::info!(
                    "spawn-prune: removed stale scratch worktree {:?} (age {}h, retention {}h)",
                    path,
                    age.as_secs() / 3600,
                    retention.as_secs() / 3600
                );
                report.removed.push(path);
            }
            Err(e) => {
                tracing::warn!(
                    "spawn-prune: failed to remove scratch worktree {:?}: {} (skipping)",
                    path,
                    e
                );
                report.failed.push((path, e));
            }
        }
    }

    report
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

    // ---------- validate_existing_worktree (Phase 2) ----------
    //
    // Builds a caller-owned worktree fixture (NOT a managed .spawn-<ref>
    // container) and exercises the happy path + each precise 400.

    /// Build a caller-owned worktree layout under `<base>`:
    ///
    /// ```text
    /// <base>/
    ///   qontinui-schemas/rust/Cargo.toml
    ///   my-checkout/                  ← the "worktree_path" the caller owns
    ///     src-tauri/Cargo.toml        ← path = "../../qontinui-schemas/rust"
    /// ```
    ///
    /// `wt_name` is the worktree dir name (sibling of `qontinui-schemas`).
    /// Returns `(workspace_root, worktree_path)`. The worktree is a real git
    /// repo so `resolved_sha` populates; pass `git_init=false` for the
    /// non-repo variant.
    fn build_existing_worktree_fixture(base: &Path, wt_name: &str, git_init: bool) -> PathBuf {
        let schemas_rust = base.join("qontinui-schemas").join("rust");
        std::fs::create_dir_all(&schemas_rust).expect("mkdir schemas/rust");
        std::fs::write(
            schemas_rust.join("Cargo.toml"),
            "[package]\nname = \"qontinui-types\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("write schemas Cargo.toml");

        let wt = base.join(wt_name);
        let src_tauri = wt.join("src-tauri");
        std::fs::create_dir_all(&src_tauri).expect("mkdir wt/src-tauri");
        std::fs::write(
            src_tauri.join("Cargo.toml"),
            "[package]\nname = \"qontinui-runner\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [dependencies]\n\
             qontinui-types = { path = \"../../qontinui-schemas/rust\" }\n",
        )
        .expect("write runner Cargo.toml");
        if git_init {
            git_in(&wt, &["init", "-q", "-b", "main"]);
            git_in(&wt, &["config", "user.email", "test@example.com"]);
            git_in(&wt, &["config", "user.name", "test"]);
            git_in(&wt, &["add", "-A"]);
            git_in(&wt, &["commit", "-q", "-m", "initial"]);
        }
        wt
    }

    #[tokio::test]
    async fn validate_existing_worktree_happy_path_resolves_provenance() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let wt = build_existing_worktree_fixture(tmp.path(), "my-checkout", true);
        // A distinct (non-matching) live repo root so the live-tree guard
        // doesn't trip. Any existing dir works for the canonicalize.
        let live_root = tmp.path().join("live-runner");
        std::fs::create_dir_all(&live_root).expect("mkdir live-runner");

        let v = validate_existing_worktree(&wt, &live_root)
            .await
            .expect("validate happy path");
        assert!(
            v.src_tauri.ends_with("src-tauri"),
            "src_tauri must point at the worktree's src-tauri"
        );
        assert!(
            v.resolved_sha.is_some(),
            "a git checkout must populate resolved_sha"
        );
        assert!(v.schemas_path.is_some(), "schemas sibling must be recorded");
        // The fixture's schemas is a sibling of the worktree, not a workspace
        // root containing BOTH qontinui-runner AND qontinui-schemas, so
        // schemas_is_shared is false here.
        assert!(!v.schemas_is_shared);
    }

    #[tokio::test]
    async fn validate_existing_worktree_non_repo_tree_still_validates() {
        // A worktree that isn't a git repo still builds (resolved_sha = None).
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let wt = build_existing_worktree_fixture(tmp.path(), "no-git-checkout", false);
        let live_root = tmp.path().join("live-runner");
        std::fs::create_dir_all(&live_root).expect("mkdir live-runner");

        let v = validate_existing_worktree(&wt, &live_root)
            .await
            .expect("non-repo tree must still validate");
        assert!(
            v.resolved_sha.is_none(),
            "a non-repo tree must have resolved_sha = None, got {:?}",
            v.resolved_sha
        );
    }

    #[tokio::test]
    async fn validate_existing_worktree_rejects_nonexistent_path() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let live_root = tmp.path().join("live-runner");
        std::fs::create_dir_all(&live_root).expect("mkdir live-runner");
        let err = validate_existing_worktree(&missing, &live_root)
            .await
            .expect_err("nonexistent path must 400");
        assert!(matches!(err, SupervisorError::Validation(_)));
        assert!(err.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn validate_existing_worktree_rejects_live_tree() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let wt = build_existing_worktree_fixture(tmp.path(), "my-checkout", true);
        // Pass the worktree itself as the live repo root → must be refused.
        let err = validate_existing_worktree(&wt, &wt)
            .await
            .expect_err("worktree == live tree must 400");
        assert!(err.to_string().contains("worktree_path_is_live_tree"));
    }

    #[tokio::test]
    async fn validate_existing_worktree_rejects_missing_src_tauri() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // A dir with the schemas sibling present but NO src-tauri/Cargo.toml.
        let schemas_rust = tmp.path().join("qontinui-schemas").join("rust");
        std::fs::create_dir_all(&schemas_rust).expect("mkdir schemas/rust");
        std::fs::write(schemas_rust.join("Cargo.toml"), "[package]\nname=\"x\"\n").expect("write");
        let wt = tmp.path().join("not-a-runner");
        std::fs::create_dir_all(&wt).expect("mkdir");
        let live_root = tmp.path().join("live-runner");
        std::fs::create_dir_all(&live_root).expect("mkdir live-runner");

        let err = validate_existing_worktree(&wt, &live_root)
            .await
            .expect_err("missing src-tauri must 400");
        assert!(err.to_string().contains("not_a_runner_worktree"));
    }

    #[tokio::test]
    async fn validate_existing_worktree_rejects_unresolved_path_deps() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // A runner worktree WITHOUT a ../qontinui-schemas sibling.
        let wt = tmp.path().join("lonely-checkout");
        let src_tauri = wt.join("src-tauri");
        std::fs::create_dir_all(&src_tauri).expect("mkdir wt/src-tauri");
        std::fs::write(
            src_tauri.join("Cargo.toml"),
            "[package]\nname=\"qontinui-runner\"\nversion=\"0.0.0\"\nedition=\"2021\"\n",
        )
        .expect("write runner Cargo.toml");
        let live_root = tmp.path().join("live-runner");
        std::fs::create_dir_all(&live_root).expect("mkdir live-runner");

        let err = validate_existing_worktree(&wt, &live_root)
            .await
            .expect_err("missing schemas sibling must 400");
        assert!(err.to_string().contains("path_deps_unresolved"));
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

    // ---------- prune_spawn_worktrees (Phase 4) ----------
    //
    // These build a real workspace fixture, materialize one or more genuine
    // `.spawn-*` containers via `prepare_worktree`, and drive the pruner with
    // an injected `now` so retention boundaries are deterministic without
    // touching mtimes.

    use std::collections::HashSet;
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    /// Serializes tests that read or mutate the process-wide retention env var
    /// so a `set_var("0")` in one test can't bleed into another's
    /// `retention_hours()` read while cargo runs tests in parallel threads.
    /// A tokio (async-aware) mutex so the guard may be held across the prune
    /// `.await` without tripping clippy's `await_holding_lock`.
    fn retention_env_guard() -> &'static tokio::sync::Mutex<()> {
        static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// `now` far enough in the future that a just-created container reads as
    /// `hours` old.
    fn now_plus_hours(hours: u64) -> SystemTime {
        SystemTime::now() + Duration::from_secs(hours * 3600)
    }

    #[tokio::test]
    async fn prune_respects_retention_boundary() {
        let _g = retention_env_guard().lock().await;
        // A fresh `.spawn-*` container younger than the window is KEPT; the same
        // container viewed past the window is REMOVED.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let prepared = prepare_worktree(&src_tauri, "HEAD").await.expect("prepare");
        assert!(prepared.container_path.exists());

        let empty: HashSet<PathBuf> = HashSet::new();

        // 24h old, default 48h window → kept.
        let r = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(24)).await;
        assert!(
            prepared.container_path.exists(),
            "container younger than retention must be kept"
        );
        assert!(r.removed.is_empty());
        assert_eq!(r.kept.len(), 1);

        // 49h old, default 48h window → removed (clean worktree, single window).
        let r = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(49)).await;
        assert!(
            !prepared.container_path.exists(),
            "container older than retention must be removed; still present: {:?}",
            prepared.container_path
        );
        assert_eq!(r.removed.len(), 1);
        assert!(
            !root.join(".spawn-HEAD").exists(),
            "the named container dir must be gone after prune"
        );
    }

    #[tokio::test]
    async fn prune_window_override_maps_to_age_window() {
        let _g = retention_env_guard().lock().await;
        // With an explicit `Some(window_hours)` override, the env-derived
        // default (48h) is ignored. A 12h-old container is KEPT under a 24h
        // override and REMOVED under a 6h override — same container, same env.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (_root, src_tauri) = build_workspace_fixture(tmp.path());
        let prepared = prepare_worktree(&src_tauri, "HEAD").await.expect("prepare");
        let empty: HashSet<PathBuf> = HashSet::new();

        // 12h old, override 24h → kept (younger than the override window).
        let r = prune_spawn_worktrees_with_window(&src_tauri, &empty, now_plus_hours(12), Some(24))
            .await;
        assert!(
            prepared.container_path.exists(),
            "12h container must be kept under a 24h override window"
        );
        assert!(r.removed.is_empty());

        // 12h old, override 6h → removed (older than the override window).
        let r = prune_spawn_worktrees_with_window(&src_tauri, &empty, now_plus_hours(12), Some(6))
            .await;
        assert!(
            !prepared.container_path.exists(),
            "12h container must be removed under a 6h override window"
        );
        assert_eq!(r.removed.len(), 1);
    }

    #[tokio::test]
    async fn prune_excludes_active_build_container() {
        let _g = retention_env_guard().lock().await;
        // An aged container that is in the active-build set is KEPT regardless
        // of age.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (_root, src_tauri) = build_workspace_fixture(tmp.path());
        let prepared = prepare_worktree(&src_tauri, "HEAD").await.expect("prepare");

        let mut active: HashSet<PathBuf> = HashSet::new();
        active.insert(prepared.container_path.clone());

        // 999h old but active → kept.
        let r = prune_spawn_worktrees(&src_tauri, &active, now_plus_hours(999)).await;
        assert!(
            prepared.container_path.exists(),
            "active-build container must never be pruned"
        );
        assert!(r.removed.is_empty());
        assert!(
            r.kept
                .iter()
                .any(|(p, why)| p == &prepared.container_path && why.contains("active build")),
            "kept reason must cite the active build; got {:?}",
            r.kept
        );

        // cleanup
        let schemas_repo = _root.join("qontinui-schemas");
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
    async fn prune_leaves_non_spawn_dirs_untouched() {
        let _g = retention_env_guard().lock().await;
        // A sibling dir that does NOT start with `.spawn-` is never touched,
        // even when very old.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());

        // A decoy dir adjacent to where spawn containers live.
        let decoy = root.join("important-checkout");
        std::fs::create_dir_all(decoy.join("src")).expect("mkdir decoy");
        std::fs::write(decoy.join("src").join("keep.txt"), "do not delete").expect("write decoy");
        // And a near-miss that starts with `.spawn` but not `.spawn-`.
        let near_miss = root.join(".spawning");
        std::fs::create_dir_all(&near_miss).expect("mkdir near miss");

        let empty: HashSet<PathBuf> = HashSet::new();
        let _ = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(999)).await;

        assert!(decoy.exists(), "non-spawn dir must be untouched");
        assert!(
            decoy.join("src").join("keep.txt").exists(),
            "non-spawn dir contents must be untouched"
        );
        assert!(
            near_miss.exists(),
            "a `.spawning` dir (not `.spawn-`) must be untouched"
        );
        // The two real sibling repos must also survive.
        assert!(root.join("qontinui-runner").exists());
        assert!(root.join("qontinui-schemas").exists());
    }

    #[tokio::test]
    async fn prune_keeps_dirty_worktree_until_double_retention() {
        let _g = retention_env_guard().lock().await;
        // A container whose runner worktree has uncommitted changes is kept
        // past the single window but removed past DOUBLE the window.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let prepared = prepare_worktree(&src_tauri, "HEAD").await.expect("prepare");

        // Dirty the runner worktree: add an untracked file (porcelain reports it).
        std::fs::write(
            prepared.worktree_path.join("src-tauri").join("DIRTY.txt"),
            "peer edit",
        )
        .expect("write dirty file");

        let empty: HashSet<PathBuf> = HashSet::new();

        // 50h old: past single 48h window but a dirty tree needs 96h → kept.
        let r = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(50)).await;
        assert!(
            prepared.container_path.exists(),
            "dirty container past single window but under double must be kept"
        );
        assert!(
            r.kept
                .iter()
                .any(|(p, why)| p == &prepared.container_path && why.contains("dirty")),
            "kept reason must cite the dirty double-retention rule; got {:?}",
            r.kept
        );

        // 97h old: past double 96h window → removed even though dirty.
        let r = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(97)).await;
        assert!(
            !prepared.container_path.exists(),
            "dirty container past double window must be removed"
        );
        assert_eq!(r.removed.len(), 1);
        let _ = root; // root used above
    }

    #[tokio::test]
    async fn prune_broken_worktree_fallback_dir_delete() {
        let _g = retention_env_guard().lock().await;
        // A `.spawn-*` container that is NOT a registered git worktree (broken /
        // never-registered) must still be removed via the directory-delete
        // fallback path, once aged out.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());

        // Hand-build a fake spawn container with NO git worktree registration:
        // just directories that look like the nested layout, no `.git`.
        let broken = root.join(".spawn-broken");
        std::fs::create_dir_all(broken.join("qontinui-runner").join("src-tauri"))
            .expect("mkdir broken runner");
        std::fs::create_dir_all(broken.join("qontinui-schemas").join("rust"))
            .expect("mkdir broken schemas");
        std::fs::write(broken.join("qontinui-runner").join("marker"), "x").expect("write marker");
        assert!(broken.exists());

        let empty: HashSet<PathBuf> = HashSet::new();
        // Aged well past retention → the `git worktree remove` no-ops (not a
        // registered worktree) and the dir delete fallback removes it.
        let r = prune_spawn_worktrees(&src_tauri, &empty, now_plus_hours(999)).await;
        assert!(
            !broken.exists(),
            "broken/unregistered spawn container must be removed via dir-delete fallback"
        );
        assert!(
            r.removed.iter().any(|p| p == &broken),
            "report must list the broken container as removed; got {:?}",
            r.removed
        );
    }

    #[tokio::test]
    async fn prune_retention_env_override_zero_prunes_idle_immediately() {
        let _g = retention_env_guard().lock().await;
        // QONTINUI_SPAWN_WORKTREE_RETENTION_HOURS=0 prunes an idle clean
        // container even at age ~0. The env guard serializes against the other
        // prune tests so this `set_var("0")` can't bleed into their default-window
        // reads.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (_root, src_tauri) = build_workspace_fixture(tmp.path());
        let prepared = prepare_worktree(&src_tauri, "HEAD").await.expect("prepare");

        let empty: HashSet<PathBuf> = HashSet::new();

        // With a 0h window, a brand-new (age ~0) clean container is prunable.
        std::env::set_var(SPAWN_RETENTION_ENV, "0");
        let r = prune_spawn_worktrees(&src_tauri, &empty, SystemTime::now()).await;
        std::env::remove_var(SPAWN_RETENTION_ENV);

        assert!(
            !prepared.container_path.exists(),
            "with retention=0 an idle clean container must be pruned immediately"
        );
        assert_eq!(r.removed.len(), 1);
    }
}

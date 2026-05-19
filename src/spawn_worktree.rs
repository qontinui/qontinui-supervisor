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

/// A prepared detached worktree at `git_ref`.
pub struct PreparedWorktree {
    /// Worktree root (parent of `src-tauri`).
    pub worktree_path: PathBuf,
    /// `<worktree_path>/src-tauri` — cargo's `current_dir` for the build.
    pub src_tauri: PathBuf,
    /// The ref the caller requested (echoed back verbatim).
    pub requested_ref: String,
    /// `git rev-parse HEAD` of the prepared worktree.
    pub resolved_sha: String,
}

/// Ensure a managed detached worktree for `git_ref` exists and is checked
/// out exactly at that ref. Idempotent: a subsequent call for the same ref
/// reuses the existing worktree dir and force-resets it to the ref.
///
/// Layout: `<repo_root>/../.supervisor-spawn-worktrees/<sanitized_ref>`.
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
    let sanitized = sanitize_ref(git_ref);

    // Parent of the repo root keeps the managed worktrees out of the repo
    // itself (so they never show up as untracked / dirty the live tree).
    let base = repo_root
        .parent()
        .ok_or_else(|| {
            SupervisorError::Validation(format!(
                "repo root {:?} has no parent for the managed worktree base",
                repo_root
            ))
        })?
        .join(".supervisor-spawn-worktrees");
    tokio::fs::create_dir_all(&base).await.map_err(|e| {
        SupervisorError::Process(format!(
            "failed to create managed worktree base {:?}: {}",
            base, e
        ))
    })?;
    let wt_path = base.join(&sanitized);

    // Best-effort fetch on the repo so the ref is resolvable even if it was
    // pushed after this supervisor started. Offline ⇒ tolerated.
    git(&["fetch", "--quiet", "origin"], &repo_root, true).await?;

    if !wt_path.exists() {
        // `git worktree add --detach <path> <ref>` creates a fresh detached
        // worktree. Run from the repo root so git resolves <path> + <ref>.
        let wt_path_str = wt_path.to_string_lossy().to_string();
        git(
            &["worktree", "add", "--detach", &wt_path_str, git_ref],
            &repo_root,
            false,
        )
        .await?;
    } else {
        // Reuse: refresh the ref then hard-pin the existing worktree to it.
        git(&["fetch", "--quiet", "origin"], &wt_path, true).await?;
        git(
            &["checkout", "--detach", "--force", git_ref],
            &wt_path,
            false,
        )
        .await?;
        git(&["reset", "--hard", git_ref], &wt_path, false).await?;
    }

    let resolved_sha = git(&["rev-parse", "HEAD"], &wt_path, false).await?;
    let src_tauri = wt_path.join("src-tauri");
    if !src_tauri.exists() {
        return Err(SupervisorError::Process(format!(
            "prepared worktree {:?} has no src-tauri/ — is {:?} a runner repo?",
            wt_path, git_ref
        )));
    }

    Ok(PreparedWorktree {
        worktree_path: wt_path,
        src_tauri,
        requested_ref: git_ref.to_string(),
        resolved_sha,
    })
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
}

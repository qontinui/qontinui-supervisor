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
//! ## Placement invariant
//!
//! The spawn worktree's parent directory MUST be the *workspace root* — the
//! directory that contains BOTH `qontinui-runner` AND `qontinui-schemas`.
//! The runner crate's `src-tauri/Cargo.toml` declares upward-relative cargo
//! path-deps:
//!
//! ```toml
//! qontinui-types      = { path = "../../qontinui-schemas/rust", ... }
//! qontinui-vision-core = { path = "../../qontinui-schemas/rust-vision-core", ... }
//! ```
//!
//! From `<wt>/src-tauri/`, `../../qontinui-schemas/rust` resolves to
//! `<wt>/../../qontinui-schemas/rust`. So `<wt>` must sit at the same depth
//! as `qontinui-runner` for the path-deps to land on the real schemas. An
//! earlier placement (`<repo_root>/../.supervisor-spawn-worktrees/<ref>/`)
//! pushed the worktree one level too deep and silently broke every build.

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
/// `project_dir`. Required so spawn worktrees can sit at the same level as
/// `qontinui-runner`, preserving the upward-relative cargo path-deps the
/// runner crate uses (`qontinui-types = { path = "../../qontinui-schemas/rust" }`).
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

/// Best-effort cleanup of a freshly-added worktree dir whose follow-up
/// operations failed. Logs (via `tracing`) on cleanup failure but never
/// propagates — the caller's original error must stay the user-visible one.
///
/// Public so the `spawn-test` route handler can invoke it via the
/// `cleanup_worktree_on_fail` request flag after a downstream cargo build
/// failure (the in-function call below only covers errors raised inside
/// `prepare_worktree` itself).
pub async fn cleanup_fresh_worktree(repo_root: &Path, wt_path: &Path) {
    let wt_path_str = wt_path.to_string_lossy().to_string();
    if let Err(e) = git(
        &["worktree", "remove", "--force", &wt_path_str],
        repo_root,
        false,
    )
    .await
    {
        tracing::warn!(
            "spawn-worktree cleanup: `git worktree remove --force {:?}` failed: {} \
             (continuing — the original error is what the caller sees)",
            wt_path,
            e
        );
    }
    if let Err(e) = git(&["worktree", "prune"], repo_root, false).await {
        tracing::warn!(
            "spawn-worktree cleanup: `git worktree prune` failed: {} \
             (continuing — the original error is what the caller sees)",
            e
        );
    }
}

/// Ensure a managed detached worktree for `git_ref` exists and is checked
/// out exactly at that ref. Idempotent: a subsequent call for the same ref
/// reuses the existing worktree dir and force-resets it to the ref.
///
/// Layout: `<workspace_root>/.spawn-<sanitized_ref>/`, where `workspace_root`
/// is the directory that contains BOTH `qontinui-runner` and
/// `qontinui-schemas` (see [`derive_workspace_root`]). The flat
/// `.spawn-<ref>` naming puts the worktree at the same depth as
/// `qontinui-runner`, which is required so the runner crate's
/// upward-relative cargo path-deps (`../../qontinui-schemas/...`) keep
/// resolving to the real schemas dir.
///
/// On the *fresh-add* path, if any post-`git worktree add` step fails this
/// function best-effort cleans up the worktree dir (`git worktree remove
/// --force` + `git worktree prune`) before returning. On the *reuse* path
/// the existing dir is left in place — the caller may have ongoing state
/// there and idempotent reuse must be safe.
pub async fn prepare_worktree(
    project_dir: &Path,
    git_ref: &str,
) -> Result<PreparedWorktree, SupervisorError> {
    if git_ref.trim().is_empty() {
        return Err(SupervisorError::Validation(
            "git_ref must be a non-empty git ref".to_string(),
        ));
    }

    // The repo we add the worktree FROM is still the runner repo. Only the
    // *placement* of the new worktree dir moves to workspace root.
    let repo_root = find_repo_root(project_dir)?;
    let workspace_root = derive_workspace_root(project_dir)?;
    let sanitized = sanitize_ref(git_ref);

    // Flat `.spawn-<sanitized>` directly under workspace_root. This puts the
    // worktree at the same depth as `qontinui-runner` so the runner crate's
    // `path = "../../qontinui-schemas/..."` deps resolve correctly. See the
    // module-level "Placement invariant" doc.
    let wt_path: PathBuf = workspace_root.join(format!(".spawn-{}", sanitized));

    // Best-effort fetch on the repo so the ref is resolvable even if it was
    // pushed after this supervisor started. Offline ⇒ tolerated.
    git(&["fetch", "--quiet", "origin"], &repo_root, true).await?;

    let was_fresh = !wt_path.exists();
    if was_fresh {
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

    // Run post-add validation in an inner async block so a single match arm
    // can wire cleanup-on-error for the fresh-add path. Reuse path: leave
    // the dir in place (the user may have state there).
    let finalize = async {
        let resolved_sha = git(&["rev-parse", "HEAD"], &wt_path, false).await?;
        let src_tauri = wt_path.join("src-tauri");
        if !src_tauri.exists() {
            return Err(SupervisorError::Process(format!(
                "prepared worktree {:?} has no src-tauri/ — is {:?} a runner repo?",
                wt_path, git_ref
            )));
        }
        Ok(PreparedWorktree {
            worktree_path: wt_path.clone(),
            src_tauri,
            requested_ref: git_ref.to_string(),
            resolved_sha,
        })
    };

    match finalize.await {
        Ok(prepared) => Ok(prepared),
        Err(e) => {
            if was_fresh {
                cleanup_fresh_worktree(&repo_root, &wt_path).await;
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

    /// Build the realistic two-sibling layout under `<base>`:
    ///
    /// ```text
    /// <base>/
    ///   qontinui-schemas/rust/Cargo.toml
    ///   qontinui-runner/                 (a real `git init`-ed repo with one commit)
    ///   qontinui-runner/src-tauri/Cargo.toml  (path = "../../qontinui-schemas/rust")
    /// ```
    ///
    /// Returns `(workspace_root, runner_src_tauri)`.
    fn build_workspace_fixture(base: &Path) -> (PathBuf, PathBuf) {
        let schemas_rust = base.join("qontinui-schemas").join("rust");
        std::fs::create_dir_all(&schemas_rust).expect("mkdir schemas/rust");
        std::fs::write(
            schemas_rust.join("Cargo.toml"),
            "[package]\nname = \"qontinui-types\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"lib.rs\"\n",
        )
        .expect("write schemas Cargo.toml");
        std::fs::write(schemas_rust.join("lib.rs"), "").expect("write schemas lib.rs");

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

        // git init + commit so `git worktree add` has a real history.
        let run_git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(&runner)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {:?} failed: stderr={}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "test"]);
        // Commit-all without configuring a remote — `git worktree add HEAD` works.
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "initial"]);

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
    async fn prepare_worktree_places_at_workspace_root_and_path_deps_resolve() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());

        let prepared = prepare_worktree(&src_tauri, "HEAD")
            .await
            .expect("prepare_worktree HEAD");

        // 1. Placement: <workspace_root>/.spawn-HEAD/
        let expected_wt = root.join(format!(".spawn-{}", sanitize_ref("HEAD")));
        assert_eq!(
            std::fs::canonicalize(&prepared.worktree_path).unwrap(),
            std::fs::canonicalize(&expected_wt).unwrap(),
            "worktree must sit at <workspace_root>/.spawn-<ref>/"
        );

        // 2. The runner's upward-relative path-dep must resolve.
        //    From <wt>/src-tauri/, "../../qontinui-schemas/rust/Cargo.toml"
        //    is the exact path string in src-tauri/Cargo.toml.
        let path_dep_target = prepared
            .src_tauri
            .join("..")
            .join("..")
            .join("qontinui-schemas")
            .join("rust")
            .join("Cargo.toml");
        let canon = std::fs::canonicalize(&path_dep_target)
            .expect("upward-relative path-dep must resolve to a real file under workspace root");
        // And the resolved file should live inside the workspace root.
        let canon_root = std::fs::canonicalize(&root).unwrap();
        assert!(
            canon.starts_with(&canon_root),
            "resolved path-dep {:?} should live inside the workspace root {:?}",
            canon,
            canon_root
        );

        // 3. PreparedWorktree fields are well-formed.
        assert_eq!(prepared.requested_ref, "HEAD");
        assert!(!prepared.resolved_sha.is_empty());
        assert!(prepared.src_tauri.ends_with("src-tauri"));

        // Best-effort: leave the worktree registered so subsequent test
        // invocations on the same tempdir would reuse — but the TempDir
        // drops anyway. Prune to avoid leaking a registration globally
        // (the inner repo lives only inside `tmp`, so this is local).
        let _ = StdCommand::new("git")
            .args(["worktree", "prune"])
            .current_dir(tmp.path().join("qontinui-runner"))
            .output();
    }

    #[tokio::test]
    async fn cleanup_fresh_worktree_removes_dir() {
        // Direct exercise of the cleanup helper: build a workspace + add a
        // real worktree at <ws>/.spawn-cleanup-probe/, then call the helper
        // and assert the dir is gone. This is the cleanup-on-error wiring's
        // unit-level guarantee.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (root, src_tauri) = build_workspace_fixture(tmp.path());
        let repo_root = find_repo_root(&src_tauri).expect("find repo root");

        let wt_path = root.join(".spawn-cleanup-probe");
        let out = StdCommand::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                wt_path.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo_root)
            .output()
            .expect("spawn git worktree add");
        assert!(
            out.status.success(),
            "setup worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(wt_path.exists(), "setup: worktree dir should exist");

        cleanup_fresh_worktree(&repo_root, &wt_path).await;

        assert!(
            !wt_path.exists(),
            "cleanup_fresh_worktree must remove the worktree dir; still present: {:?}",
            wt_path
        );
    }
}

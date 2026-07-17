//! Shared git-provenance helpers — origin/main staleness detection (Phase A of
//! `plans/supervisor-origin-main-build-provenance-2026-06-07.md`).
//!
//! The 2026-06-07 incident: a primary rebuild compiled the contested working
//! checkout while it sat behind `origin/main`, silently producing a binary that
//! did NOT contain the just-merged fix. "Build succeeded", runner came up
//! "healthy", and the only detection was a human diffing `/health` `gitSha`
//! against `origin/main`.
//!
//! This module turns that silent stale-build into a loud, API-surfaced signal:
//!
//! * [`fetch_origin`] — `git fetch origin --quiet`, best-effort and
//!   offline-tolerant (a stale origin ref must never block a build).
//! * [`origin_main_drift`] — compares a built SHA against `origin/main`,
//!   returning [`OriginMainDrift`] with a behind-count and an ancestor test:
//!   - `is_ancestor == true && behind_count > 0` = clean-but-stale (the
//!     incident).
//!   - `is_ancestor == false` = diverged / on a feature branch (a peer parked
//!     it) — the more dangerous case.
//!   - `behind_count == 0 && is_ancestor` = up to date.
//! * [`drift_against`] — the same compare against an ARBITRARY base ref/sha.
//!   `origin_main_drift` is an alias for it with `base_ref = "origin/main"`.
//!   `spawn-test {rebuild:true}` uses it with the LKG's recorded sha as the
//!   base to answer "is the tree I'm about to build older than the LKG?" —
//!   the 2026-07-17 trap, where a checkout parked 71 commits behind produced a
//!   binary predating the very fixes the spawn was commissioned to verify.
//!
//! The git runner ([`git`]) mirrors the private `git()` helper in
//! `spawn_worktree.rs:145`: a native tokio `Command::output()` that captures
//! stdout/stderr separately, so the Windows PowerShell-5.1 `2>&1` stderr hazard
//! does NOT apply (it's not a shell pipeline). `quiet_offline_ok` makes a failed
//! `fetch` non-fatal.

use std::path::Path;

use serde::Serialize;
use tokio::process::Command;
use tracing::debug;

use crate::error::SupervisorError;

/// Drift of a built tree's SHA relative to `origin/main`.
///
/// Computed by [`origin_main_drift`]. Serialized verbatim into the
/// `origin_main_drift` field on `GET /builds` and the detached rebuild outcome,
/// so the field names here are the wire field names (snake_case by default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OriginMainDrift {
    /// The SHA that was (or will be) built.
    pub built_sha: String,
    /// `git rev-parse origin/main` (post-fetch). Empty when the ref could not
    /// be resolved (no remote / no `origin/main`), in which case the other
    /// fields are best-effort defaults and the caller should treat the result
    /// as "not computable".
    pub origin_main_sha: String,
    /// `git rev-list --count <built_sha>..origin/main` — how many commits
    /// `origin/main` is ahead of the built SHA. `0` when up to date.
    pub behind_count: u32,
    /// `git merge-base --is-ancestor <built_sha> origin/main` (exit 0 =
    /// ancestor). `true` = the built SHA is on `origin/main`'s history
    /// (clean, possibly stale); `false` = diverged / a feature branch.
    pub is_ancestor: bool,
    /// Whether the pre-compare `fetch_origin` succeeded. `false` ⇒ the compare
    /// was against a possibly-stale local `origin/main` ref.
    pub fetched: bool,
}

impl OriginMainDrift {
    /// `true` when the built SHA is `origin/main` (or an ancestor with nothing
    /// ahead) — i.e. nothing to warn about.
    pub fn is_up_to_date(&self) -> bool {
        self.is_ancestor && self.behind_count == 0
    }

    /// `true` when the built SHA is NOT on `origin/main`'s history — a
    /// feature-branch / diverged build, the more dangerous staleness case.
    pub fn is_diverged(&self) -> bool {
        !self.is_ancestor
    }
}

/// Run a git command, returning a clear `SupervisorError::Process` on non-zero
/// exit or spawn failure. `quiet_offline_ok` makes a failed command (e.g.
/// `fetch`) non-fatal by returning an empty string — the caller still validates
/// any resulting ref via a subsequent rev-parse.
///
/// Mirrors the private `git()` helper at `spawn_worktree.rs:145`: a native
/// tokio `Command::output()` capturing stdout/stderr separately. No PowerShell
/// `2>&1` redirect is involved, so the PS-5.1 native-cmd-stderr hazard is N/A.
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
        // Best-effort: tolerate offline / transient remote errors.
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

/// Best-effort `git fetch origin --quiet` in `repo_root`.
///
/// Offline-tolerant: a network / remote failure resolves to `Ok(())` (like the
/// `quiet_offline_ok` path of [`git`]) because a stale origin ref must never
/// block a build. Returns `Ok(())` on success OR a tolerated failure; only a
/// spawn failure (git missing) bubbles up as `Err`.
pub async fn fetch_origin(repo_root: &Path) -> Result<(), SupervisorError> {
    git(&["fetch", "origin", "--quiet"], repo_root, true)
        .await
        .map(|_| ())
}

/// Compute the drift of `built_sha` relative to `origin/main` in `repo_root`.
///
/// Thin alias for [`drift_against`] with `base_ref = "origin/main"` — see that
/// function for the probe list and the best-effort contract.
pub async fn origin_main_drift(repo_root: &Path, built_sha: &str) -> OriginMainDrift {
    drift_against(repo_root, built_sha, "origin/main").await
}

/// Compute the drift of `built_sha` relative to an arbitrary `base_ref` (a
/// branch, tag, or raw SHA — anything `git rev-parse` accepts) in `repo_root`.
///
/// Generalized from [`origin_main_drift`] so the same ancestry machinery can
/// answer "is this build older than the LKG?" (`base_ref` = the LKG's recorded
/// sha) as well as "is this build older than main?". Fetches origin first
/// (best-effort, offline-tolerant), then runs three probes:
/// - `git rev-parse <base_ref>` → [`OriginMainDrift::origin_main_sha`] (the
///   resolved BASE sha — the field keeps its wire name for `GET /builds`
///   compatibility);
/// - `git rev-list --count <built_sha>..<base_ref>` →
///   [`OriginMainDrift::behind_count`];
/// - `git merge-base --is-ancestor <built_sha> <base_ref>` (exit 0 = ancestor)
///   → [`OriginMainDrift::is_ancestor`].
///
/// Best-effort throughout: if `base_ref` cannot be resolved (no remote / no
/// branch / unknown sha / not a repo) the returned struct has an empty
/// `origin_main_sha`, `behind_count == 0`, and `is_ancestor == true` (i.e. it
/// reads as "up to date / not computable" — never a false staleness alarm). The
/// `fetched` flag records whether the pre-fetch succeeded so the caller can
/// tell a fresh compare from a possibly-stale one.
pub async fn drift_against(repo_root: &Path, built_sha: &str, base_ref: &str) -> OriginMainDrift {
    let fetched = fetch_origin(repo_root).await.is_ok();

    // Resolve the base ref. An empty / errored result means the ref is not
    // available — treat the whole compare as not-computable (up-to-date-ish)
    // rather than inventing a behind-count.
    let origin_main_sha = match git(&["rev-parse", base_ref], repo_root, false).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            debug!("drift_against: `git rev-parse {base_ref}` returned empty in {repo_root:?}");
            return OriginMainDrift {
                built_sha: built_sha.to_string(),
                origin_main_sha: String::new(),
                behind_count: 0,
                is_ancestor: true,
                fetched,
            };
        }
        Err(e) => {
            debug!("drift_against: `git rev-parse {base_ref}` failed in {repo_root:?}: {e}");
            return OriginMainDrift {
                built_sha: built_sha.to_string(),
                origin_main_sha: String::new(),
                behind_count: 0,
                is_ancestor: true,
                fetched,
            };
        }
    };

    // How many commits the base ref is ahead of the built SHA. A non-numeric /
    // errored result (e.g. unknown built_sha) is treated as 0.
    let behind_count = match git(
        &["rev-list", "--count", &format!("{built_sha}..{base_ref}")],
        repo_root,
        false,
    )
    .await
    {
        Ok(s) => s.trim().parse::<u32>().unwrap_or(0),
        Err(e) => {
            debug!("drift_against: `git rev-list --count` failed in {repo_root:?}: {e}");
            0
        }
    };

    // Is the built SHA on the base ref's history? `merge-base --is-ancestor`
    // exits 0 (ancestor) / 1 (not). `quiet_offline_ok = false` so a real exit-1
    // surfaces as Err; we map Err → not-ancestor (the dangerous case is the one
    // we must not under-report). A genuinely diverged feature branch exits 1.
    let is_ancestor = git(
        &["merge-base", "--is-ancestor", built_sha, base_ref],
        repo_root,
        false,
    )
    .await
    .is_ok();

    OriginMainDrift {
        built_sha: built_sha.to_string(),
        origin_main_sha,
        behind_count,
        is_ancestor,
        fetched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// Run `git <args>` in `cwd`, asserting success. Modeled on the `git_in`
    /// test helper at `spawn_worktree.rs:1133`.
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

    /// Capture trimmed stdout of `git <args>` in `cwd`, asserting success.
    fn git_out(cwd: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Commit a one-line file `name` with `content` on the current branch and
    /// return the resulting HEAD SHA.
    fn commit_file(dir: &Path, name: &str, content: &str) -> String {
        std::fs::write(dir.join(name), content).expect("write file");
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", &format!("add {name}")]);
        git_out(dir, &["rev-parse", "HEAD"])
    }

    /// Initialize a repo whose `origin/main` ref is a LOCAL remote-tracking ref
    /// — no network. We `git init` a real repo, add an `origin` remote pointing
    /// at the repo itself, and `fetch`, so `origin/main` resolves exactly the
    /// way the production helper expects. The whole test runs offline.
    fn init_repo_with_self_origin(dir: &Path) -> String {
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "test@example.com"]);
        git_in(dir, &["config", "user.name", "test"]);
        let sha = commit_file(dir, "seed.txt", "seed");
        let dir_str = dir.to_string_lossy().to_string();
        git_in(dir, &["remote", "add", "origin", &dir_str]);
        git_in(dir, &["fetch", "-q", "origin"]);
        sha
    }

    /// Up-to-date: built SHA == origin/main ⇒ behind_count 0, is_ancestor true.
    #[tokio::test]
    async fn drift_up_to_date() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let head = init_repo_with_self_origin(dir);
        // origin/main now points at `head` (we fetched after the seed commit).

        let drift = origin_main_drift(dir, &head).await;
        assert_eq!(drift.behind_count, 0, "up-to-date ⇒ behind_count 0");
        assert!(drift.is_ancestor, "built SHA is origin/main ⇒ ancestor");
        assert!(drift.is_up_to_date());
        assert!(!drift.is_diverged());
        assert_eq!(drift.origin_main_sha, head);
    }

    /// Behind-on-main: built SHA is an older commit on main, then main advances
    /// and is re-fetched ⇒ behind_count > 0, is_ancestor true (clean-but-stale,
    /// the incident).
    #[tokio::test]
    async fn drift_behind_on_main() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let old = init_repo_with_self_origin(dir);

        // Advance main by two commits and re-fetch so origin/main moves ahead
        // of `old`.
        commit_file(dir, "a.txt", "a");
        commit_file(dir, "b.txt", "b");
        git_in(dir, &["fetch", "-q", "origin"]);

        let drift = origin_main_drift(dir, &old).await;
        assert_eq!(
            drift.behind_count, 2,
            "origin/main is two commits ahead of the built SHA"
        );
        assert!(
            drift.is_ancestor,
            "the built SHA is still on main's history (clean-but-stale)"
        );
        assert!(!drift.is_up_to_date());
        assert!(!drift.is_diverged());
    }

    /// Diverged / feature branch: built SHA is on a branch that is NOT an
    /// ancestor of origin/main ⇒ is_ancestor false (the dangerous case).
    #[tokio::test]
    async fn drift_diverged_feature_branch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        init_repo_with_self_origin(dir);

        // Create a feature branch with its own commit, then advance main and
        // re-fetch so origin/main and the feature commit diverge.
        git_in(dir, &["checkout", "-q", "-b", "feature"]);
        let feature_sha = commit_file(dir, "feature.txt", "feature");

        git_in(dir, &["checkout", "-q", "main"]);
        commit_file(dir, "main-only.txt", "main");
        git_in(dir, &["fetch", "-q", "origin"]);

        let drift = origin_main_drift(dir, &feature_sha).await;
        assert!(
            !drift.is_ancestor,
            "a feature-branch commit is not an ancestor of origin/main"
        );
        assert!(drift.is_diverged());
        assert!(!drift.is_up_to_date());
    }

    /// Generalized base — the `spawn-test {rebuild:true}` build-vintage case:
    /// the tree about to be built sits behind the LKG's recorded SHA (a raw
    /// sha base, not a ref) ⇒ behind_count > 0, is_ancestor true.
    #[tokio::test]
    async fn drift_against_raw_sha_base_behind_lkg() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let built = init_repo_with_self_origin(dir);

        // The LKG was built from a newer commit than the checkout's HEAD.
        commit_file(dir, "a.txt", "a");
        let lkg_sha = commit_file(dir, "b.txt", "b");

        let drift = drift_against(dir, &built, &lkg_sha).await;
        assert_eq!(
            drift.behind_count, 2,
            "the LKG contains two commits the built tree lacks"
        );
        assert!(drift.is_ancestor, "built SHA is an ancestor of the LKG SHA");
        assert!(!drift.is_up_to_date());
        assert_eq!(drift.origin_main_sha, lkg_sha, "base sha is resolved");
    }

    /// Generalized base, not-older case: the built tree already CONTAINS the
    /// LKG's commit ⇒ behind_count 0 (no warning), even though the LKG sha is
    /// not an ancestor-test subject of the built one.
    #[tokio::test]
    async fn drift_against_raw_sha_base_ahead_of_lkg() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let lkg_sha = init_repo_with_self_origin(dir);
        let built = commit_file(dir, "newer.txt", "newer");

        let drift = drift_against(dir, &built, &lkg_sha).await;
        assert_eq!(
            drift.behind_count, 0,
            "the built tree contains the LKG commit ⇒ not older"
        );
    }

    /// Best-effort: a non-repo directory yields a not-computable drift
    /// (empty origin_main_sha, reads as up-to-date) rather than panicking.
    #[tokio::test]
    async fn drift_non_repo_is_not_computable() {
        let tmp = TempDir::new().unwrap();
        let drift = origin_main_drift(tmp.path(), "deadbeef").await;
        assert!(drift.origin_main_sha.is_empty());
        assert!(drift.is_up_to_date(), "not-computable reads as up-to-date");
    }
}

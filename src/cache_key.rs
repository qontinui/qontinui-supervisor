//! Row 10 Item 5 — canonical-diff cache-key derivation.
//!
//! Composes the content-addressed build-cache key the supervisor pool
//! uses to query / populate bazel-remote's Action Cache:
//!
//! ```text
//! key = sha256(base_sha   || ":"
//!           || diff_sha    || ":"
//!           || rustc_version || ":"
//!           || cargo_profile || ":"
//!           || target_triple)
//! ```
//!
//! `base_sha` + `diff_sha` come from the Wave-0 promoted
//! `qontinui-runner/scripts/canonical_diff_hash.py` (called as a
//! subprocess — see "Why a subprocess" below). The remaining three
//! dimensions are toolchain/profile values this module fills in; they
//! prevent cross-toolchain false hits (a debug entry serving a release
//! build, x86_64 serving aarch64, or — folded into `cargo_profile` here
//! — a `cargo check` result serving a `cargo build`).
//!
//! See memory `proj_canonical_diff_hash_recipe` for the empirical proof
//! that identical content at different worktree paths produces an
//! identical `diff_sha`, and for the reproducibility-flag caveat
//! (artifact bytes are only stable with `/Brepro` / `--build-id=none`).
//!
//! ## Why a subprocess (vs a Rust port)
//!
//! The prompt allows promoting `canonical_diff_hash.py` to Rust "if
//! performance demands it; otherwise call the Python script as a
//! subprocess." It does not. The script runs one `git diff` plus a
//! sha256 — git itself dominates the wall-clock (tens of ms); a Rust
//! reimplementation would save nothing measurable while introducing a
//! *second* canonicalizer that must stay byte-identical to the Python
//! one or the cache silently misses across producers that picked
//! different implementations. One canonicalizer, one source of truth.
//! Calling the subprocess keeps the recipe in exactly one place
//! (memory `proj_canonical_diff_hash_recipe` explicitly warns against
//! deviating from it).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::build_submissions::BuildKind;

/// The fully composed cache key plus the dimensions that fed it.
/// Serialized onto every `BuildSubmission` so `GET /build/:id/status`
/// and telemetry can show *why* a key was what it was (debugging
/// cross-worktree misses is otherwise opaque).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CanonicalDiffKey {
    /// Full 40-hex base commit the diff is rooted at.
    pub base_sha: String,
    /// sha256 of the canonicalized working-tree diff.
    pub diff_sha: String,
    /// Normalized `rustc -vV` (release + commit-hash); see [`rustc_version`].
    pub rustc_version: String,
    /// `<build_kind>:<profile>` — e.g. `build:dev`, `release:release`,
    /// `check:dev`. Folding `build_kind` in keeps the key a 5-tuple (as
    /// documented in the recipe) while preventing a `check` artifact
    /// from being served for a `build`.
    pub cargo_profile: String,
    /// Rust host/target triple (e.g. `x86_64-pc-windows-msvc`).
    pub target_triple: String,
    /// The composed sha256 hex — the actual AC lookup/store key.
    pub key: String,
}

/// Map a [`BuildKind`] to the `cargo_profile` key dimension.
///
/// `Release` compiles into `target/release/`; the other kinds into
/// `target/debug/`. We additionally prefix the kind so artifacts of
/// different shapes (a `check` produces no binary; a `test --no-run`
/// produces test harnesses) never collide on one key.
pub fn profile_for(kind: BuildKind) -> String {
    let base = match kind {
        BuildKind::Release => "release",
        BuildKind::Check | BuildKind::Build | BuildKind::Test => "dev",
    };
    let kind_tag = match kind {
        BuildKind::Check => "check",
        BuildKind::Build => "build",
        BuildKind::Release => "release",
        BuildKind::Test => "test",
    };
    format!("{kind_tag}:{base}")
}

/// `target/<debug|release>` sub-path for a profile, used by the AC
/// write/read paths to locate the artifact.
pub fn target_subdir(kind: BuildKind) -> &'static str {
    match kind {
        BuildKind::Release => "release",
        BuildKind::Check | BuildKind::Build | BuildKind::Test => "debug",
    }
}

/// Resolve the path to `canonical_diff_hash.py`.
///
/// Order: explicit env override → `<project_dir>/../scripts/` (the
/// Wave-0 promotion site `qontinui-runner/scripts/`, where
/// `project_dir` is `qontinui-runner/src-tauri`) → `<worktree>/scripts/`
/// (some repos vendor their own copy). Returns the first that exists.
pub fn resolve_script(project_dir: &Path, worktree: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("QONTINUI_CANONICAL_DIFF_HASH_SCRIPT") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let from_project = project_dir
        .parent()
        .map(|runner| runner.join("scripts").join("canonical_diff_hash.py"));
    if let Some(p) = from_project {
        if p.exists() {
            return Some(p);
        }
    }
    let from_wt = worktree.join("scripts").join("canonical_diff_hash.py");
    if from_wt.exists() {
        return Some(from_wt);
    }
    None
}

/// Run `rustc -vV` and return a normalized version string + the host
/// target triple. The version folds in the commit-hash so a toolchain
/// bump (which can change codegen) invalidates cache entries.
pub async fn rustc_version_and_host() -> Result<(String, String), String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(&rustc)
        .arg("-vV")
        .output()
        .await
        .map_err(|e| format!("failed to run `{rustc} -vV`: {e}"))?;
    if !out.status.success() {
        return Err(format!("`{rustc} -vV` exited {:?}", out.status.code()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut release = None;
    let mut commit = None;
    let mut host = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("release:") {
            release = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("commit-hash:") {
            commit = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("host:") {
            host = Some(v.trim().to_string());
        }
    }
    let release = release.ok_or("`rustc -vV` had no `release:` line")?;
    let commit = commit.unwrap_or_else(|| "unknown".to_string());
    // Allow an explicit cross-compile target to override the host triple.
    let triple = std::env::var("CARGO_BUILD_TARGET")
        .ok()
        .filter(|s| !s.is_empty())
        .or(host)
        .ok_or("`rustc -vV` had no `host:` line and CARGO_BUILD_TARGET unset")?;
    Ok((format!("rustc {release} ({commit})"), triple))
}

/// Invoke `canonical_diff_hash.py <worktree> [--base <base>]` and parse
/// its `base_sha=` / `diff_sha=` lines.
async fn run_canonical_diff(
    script: &Path,
    worktree: &Path,
    base_ref: Option<&str>,
) -> Result<(String, String), String> {
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python".to_string());
    let mut cmd = Command::new(&python);
    cmd.arg(script).arg(worktree);
    if let Some(b) = base_ref {
        cmd.arg("--base").arg(b);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("failed to spawn `{python} canonical_diff_hash.py`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "canonical_diff_hash.py exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut base_sha = None;
    let mut diff_sha = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("base_sha=") {
            base_sha = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("diff_sha=") {
            diff_sha = Some(v.trim().to_string());
        }
    }
    match (base_sha, diff_sha) {
        (Some(b), Some(d)) => Ok((b, d)),
        _ => Err(format!(
            "canonical_diff_hash.py output missing base_sha/diff_sha: {}",
            stdout.trim()
        )),
    }
}

/// Compose the cache key from its five dimensions. Pure — separated so
/// it is unit-testable without git/rustc/python on the box.
pub fn compose(
    base_sha: &str,
    diff_sha: &str,
    rustc_version: &str,
    cargo_profile: &str,
    target_triple: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(base_sha.as_bytes());
    h.update(b":");
    h.update(diff_sha.as_bytes());
    h.update(b":");
    h.update(rustc_version.as_bytes());
    h.update(b":");
    h.update(cargo_profile.as_bytes());
    h.update(b":");
    h.update(target_triple.as_bytes());
    hex::encode(h.finalize())
}

/// Full key derivation for one build submission. Best-effort by
/// contract: the *caller* treats any `Err` as "cache disabled for this
/// build" and proceeds with a normal cargo invocation. Never let a
/// cache-key failure fail a build.
pub async fn compute(
    script: &Path,
    worktree: &Path,
    base_ref: Option<&str>,
    kind: BuildKind,
) -> Result<CanonicalDiffKey, String> {
    let (base_sha, diff_sha) = run_canonical_diff(script, worktree, base_ref).await?;
    let (rustc_version, target_triple) = rustc_version_and_host().await?;
    let cargo_profile = profile_for(kind);
    let key = compose(
        &base_sha,
        &diff_sha,
        &rustc_version,
        &cargo_profile,
        &target_triple,
    );
    Ok(CanonicalDiffKey {
        base_sha,
        diff_sha,
        rustc_version,
        cargo_profile,
        target_triple,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_is_stable_and_order_sensitive() {
        let a = compose(
            "base",
            "diff",
            "rustc 1.95.0 (abc)",
            "build:dev",
            "x86_64-pc-windows-msvc",
        );
        let b = compose(
            "base",
            "diff",
            "rustc 1.95.0 (abc)",
            "build:dev",
            "x86_64-pc-windows-msvc",
        );
        assert_eq!(a, b, "same inputs => same key");
        assert_eq!(a.len(), 64, "sha256 hex");

        // Each dimension must move the key.
        assert_ne!(
            a,
            compose(
                "base2",
                "diff",
                "rustc 1.95.0 (abc)",
                "build:dev",
                "x86_64-pc-windows-msvc"
            )
        );
        assert_ne!(
            a,
            compose(
                "base",
                "diff2",
                "rustc 1.95.0 (abc)",
                "build:dev",
                "x86_64-pc-windows-msvc"
            )
        );
        assert_ne!(
            a,
            compose(
                "base",
                "diff",
                "rustc 1.96.0 (def)",
                "build:dev",
                "x86_64-pc-windows-msvc"
            )
        );
        assert_ne!(
            a,
            compose(
                "base",
                "diff",
                "rustc 1.95.0 (abc)",
                "release:release",
                "x86_64-pc-windows-msvc"
            )
        );
        assert_ne!(
            a,
            compose(
                "base",
                "diff",
                "rustc 1.95.0 (abc)",
                "build:dev",
                "aarch64-apple-darwin"
            )
        );
    }

    #[test]
    fn delimiter_prevents_field_smear() {
        // Without the ':' separator these two would hash identically.
        let x = compose("ab", "c", "r", "p", "t");
        let y = compose("a", "bc", "r", "p", "t");
        assert_ne!(x, y, "':' separator must keep field boundaries distinct");
    }

    #[test]
    fn profile_distinguishes_kinds() {
        assert_eq!(profile_for(BuildKind::Build), "build:dev");
        assert_eq!(profile_for(BuildKind::Release), "release:release");
        assert_eq!(profile_for(BuildKind::Check), "check:dev");
        assert_eq!(profile_for(BuildKind::Test), "test:dev");
        // A check and a build must not share a key.
        assert_ne!(profile_for(BuildKind::Check), profile_for(BuildKind::Build));
    }

    #[test]
    fn target_subdir_matches_cargo_layout() {
        assert_eq!(target_subdir(BuildKind::Release), "release");
        assert_eq!(target_subdir(BuildKind::Build), "debug");
        assert_eq!(target_subdir(BuildKind::Check), "debug");
    }
}

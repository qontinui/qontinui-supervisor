fn main() {
    // FIRST, unconditionally: the crate does `env!("QONTINUI_SUPERVISOR_GIT_SHA")`,
    // which is a COMPILE error when the var was never emitted. The frontend
    // block below has an early `return` on an npm failure, so emitting the
    // provenance after it would turn a missing npm into "the supervisor crate
    // does not compile".
    emit_self_build_provenance();

    // Only build frontend if dist/ is missing index.html or in release mode.
    // During dev, run `cd frontend && npm run dev` separately for HMR.
    let dist_index = std::path::Path::new("dist/index.html");
    if !dist_index.exists() {
        // Check if frontend/ directory exists with package.json
        let frontend_pkg = std::path::Path::new("frontend/package.json");
        if frontend_pkg.exists() {
            // Check if node_modules exists, install if not
            let node_modules = std::path::Path::new("frontend/node_modules");
            if !node_modules.exists() {
                let status = std::process::Command::new("npm")
                    .args(["install"])
                    .current_dir("frontend")
                    .status();
                if let Err(e) = status {
                    println!("cargo:warning=Failed to run npm install: {}", e);
                    println!("cargo:warning=SPA will not be embedded. Run `cd frontend && npm install && npm run build` manually.");
                    return;
                }
            }

            let status = std::process::Command::new("npm")
                .args(["run", "build"])
                .current_dir("frontend")
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!("cargo:warning=Frontend built successfully into dist/");
                }
                Ok(s) => {
                    println!(
                        "cargo:warning=Frontend build failed with status: {}. Dashboard will use legacy HTML fallback.",
                        s
                    );
                }
                Err(e) => {
                    println!("cargo:warning=Failed to run npm build: {}. Dashboard will use legacy HTML fallback.", e);
                }
            }
        }
    }

    // Rebuild if frontend source changes
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=dist");
}

/// The value stamped when this build could not establish a commit at all.
///
/// A distinct sentinel, never an empty string: `""` deserialises downstream as
/// a present-but-blank field and reads as "clean/unset", which is exactly the
/// plausible-looking wrong answer this provenance exists to prevent.
const UNKNOWN: &str = "unknown";

/// Stamp the supervisor's OWN build commit into the binary
/// (`plans/2026-08-04-landed-infra-fixes-not-in-effect-on-this-machine.md`
/// Phase 2.3).
///
/// Emits two `rustc-env` vars, read back by `crate::self_provenance`:
///
/// * `QONTINUI_SUPERVISOR_GIT_SHA` — the full 40-hex commit `HEAD` pointed at
///   when this binary was compiled, or `unknown`.
/// * `QONTINUI_SUPERVISOR_GIT_DIRTY` — `true` / `false` / `unknown`: whether
///   TRACKED files were modified in the working tree at build time.
///
/// **Why build time and not a runtime `git` call.** The question `/health` has
/// to answer is "which commit is the RUNNING binary made of" — a runtime probe
/// answers "what does the checkout say right now", which is a different
/// question and diverges the moment anyone pulls, rebases, or switches branch
/// under a long-lived supervisor. That divergence is the 2026-08-04 incident
/// class: diagnosing whether the live supervisor carried PR #119 fell back to
/// an exe mtime and measured the wrong exe entirely.
///
/// `QONTINUI_SUPERVISOR_GIT_SHA` may also be supplied by the build ENV (source
/// tarballs, or a CI checkout with no `.git`); an env value that does not look
/// like a sha is ignored rather than propagated as a lie.
fn emit_self_build_provenance() {
    // An explicit override always wins, so a build from an exported tree can
    // still report honest provenance.
    println!("cargo:rerun-if-env-changed=QONTINUI_SUPERVISOR_GIT_SHA");
    let override_sha = std::env::var("QONTINUI_SUPERVISOR_GIT_SHA")
        .ok()
        .filter(|s| looks_like_sha(s.trim()));

    let sha = override_sha
        .map(|s| s.trim().to_string())
        .or_else(|| git(&["rev-parse", "HEAD"]).filter(|s| looks_like_sha(s)))
        .unwrap_or_else(|| UNKNOWN.to_string());

    // `--untracked-files=no`: an untracked scratch file is not part of what was
    // compiled, so counting it would report every worktree as permanently
    // dirty and the marker would carry no information.
    let dirty = if sha == UNKNOWN {
        // No commit ⇒ no honest statement about drift FROM that commit.
        UNKNOWN.to_string()
    } else {
        match git(&["status", "--porcelain", "--untracked-files=no"]) {
            Some(out) => (!out.trim().is_empty()).to_string(),
            None => UNKNOWN.to_string(),
        }
    };

    println!("cargo:rustc-env=QONTINUI_SUPERVISOR_GIT_SHA={sha}");
    println!("cargo:rustc-env=QONTINUI_SUPERVISOR_GIT_DIRTY={dirty}");

    // Refresh triggers. `src` is listed so an edit to the tree re-runs this
    // script and the dirty marker tracks the sources actually compiled;
    // HEAD / the checked-out ref / packed-refs cover commit, branch switch and
    // `git gc`. Emitting ANY rerun-if-changed disables cargo's default
    // "rerun on any package change", which is why `src` has to be explicit.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    for path in git_ref_paths() {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// Paths whose mtime should re-trigger the provenance capture: `HEAD`,
/// `packed-refs`, and the file backing the currently checked-out branch.
///
/// Resolved through `git rev-parse --git-path` rather than assuming `.git/…`:
/// this repo is routinely built from linked worktrees, where `.git` is a FILE
/// and the real ref store lives elsewhere. Silently emitting a non-existent
/// path would make cargo re-run the script on every build (harmless) or, worse,
/// look like coverage that is not there.
fn git_ref_paths() -> Vec<String> {
    let mut paths = Vec::new();
    for name in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", name]) {
            paths.push(p);
        }
    }
    // Detached HEAD ⇒ no symbolic ref ⇒ nothing more to watch.
    if let Some(sym) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &sym]) {
            paths.push(p);
        }
    }
    paths
}

/// Run `git` in the package root, returning trimmed stdout on success only.
///
/// Every failure mode (git not installed, not a repository, a broken index)
/// collapses to `None` — build provenance must never be able to fail a build.
fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Is `s` a plausible git object id (7-40 lowercase-hex characters)?
///
/// Guards the env-override path and the `rev-parse` result so a stray value can
/// never be stamped as a commit. Duplicated (small, and deliberately) in
/// `crate::self_provenance`, which re-validates at READ time — build.rs cannot
/// import from the crate it builds.
fn looks_like_sha(s: &str) -> bool {
    let s = s.trim();
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

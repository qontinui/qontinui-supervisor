fn main() {
    // FIRST, unconditionally: the crate does `env!("QONTINUI_SUPERVISOR_GIT_SHA")`,
    // which is a COMPILE error when the var was never emitted. Kept first so no
    // early return added to `main` later can skip it and turn a frontend
    // failure into "the supervisor crate does not compile".
    emit_self_build_provenance();

    // Re-run this script (and re-embed `dist/`) when the frontend sources or
    // `dist/` change. This does not rebuild a `dist/` that already has an
    // `index.html`. Emitted before the frontend step, so a frontend built by
    // hand after a failed attempt still re-runs this script and gets embedded.
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=dist");

    build_frontend_if_missing();

    // Cargo reads a `rerun-if-changed` path that does not exist as changed on
    // every invocation, so after a failed frontend build every cargo command
    // would retry npm and recompile the crate. An empty `dist/` gives it a real
    // directory to track; the next Vite build writes into it. Creating it here
    // makes it newer than this run, so the next invocation re-runs once more
    // and then settles. The embed is `allow_missing`, and an empty folder
    // serves the same legacy fallback.
    if let Err(e) = std::fs::create_dir_all("dist") {
        println!(
            "cargo:warning=Could not create dist/ ({e}); every cargo command will re-run this build script until it exists."
        );
    }
}

/// Build the SPA into `dist/` when `dist/index.html` is absent. During dev,
/// run `cd frontend && npm run dev` separately for HMR.
fn build_frontend_if_missing() {
    if std::path::Path::new("dist/index.html").exists()
        || !std::path::Path::new("frontend/package.json").exists()
    {
        return;
    }
    // `npm ci`, not `npm install`: it installs exactly what the tracked
    // `frontend/package-lock.json` says and never rewrites it, which would leave
    // the worktree dirty (restart-supervisor.ps1 uses `npm ci` for the same
    // reason).
    if !std::path::Path::new("frontend/node_modules").exists() {
        match npm(&["ci"]) {
            Ok(s) if s.success() => {}
            outcome => {
                println!(
                    "cargo:warning=npm ci did not succeed ({}; is npm on PATH?). The dashboard will use the legacy HTML fallback. Run `cd frontend && npm ci && npm run build` manually.",
                    describe(&outcome)
                );
                return;
            }
        }
    }
    match npm(&["run", "build"]) {
        Ok(s) if s.success() => {
            println!("cargo:warning=Frontend built successfully into dist/");
        }
        outcome => {
            println!(
                "cargo:warning=Frontend build did not succeed ({}; is npm on PATH?). The dashboard will use the legacy HTML fallback. Run `cd frontend && npm ci && npm run build` manually.",
                describe(&outcome)
            );
        }
    }
}

/// Run `npm <args>` in `frontend/`. On Windows npm is a `.cmd` shim, and
/// `Command::new("npm")` cannot find it (it appends only `.exe` when it
/// searches `PATH`), so a fresh checkout with no `dist/` never got its SPA
/// built. It goes through `cmd /C` instead, the way `expo.rs` and
/// `build_monitor.rs` launch theirs, and cmd resolves `npm` through `PATHEXT`.
/// So a missing npm is a non-zero exit there rather than an `Err`. `/D` skips
/// the user's cmd AutoRun, which could change directory or print into the
/// build script's stdout, which cargo parses.
fn npm(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    let mut command = if cfg!(windows) {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/D", "/C", "npm"]);
        cmd
    } else {
        std::process::Command::new("npm")
    };
    command.args(args).current_dir("frontend").status()
}

fn describe(outcome: &std::io::Result<std::process::ExitStatus>) -> String {
    match outcome {
        Ok(status) => status.to_string(),
        Err(e) => format!("could not run npm: {e}"),
    }
}

// The pure classification rules (`UNKNOWN_STAMP`, `looks_like_sha`,
// `dirty_marker`), shared verbatim with the crate as
// `crate::provenance_stamp`. A build script cannot import from the crate it
// builds, so the file is `include!`d here and compiled again over there — which
// is what puts these rules under the merge-blocking CI gate. See that file's
// module docs for the dirty-marker defect that made the sharing necessary.
include!("src/provenance_stamp.rs");

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
        .unwrap_or_else(|| UNKNOWN_STAMP.to_string());

    // `--untracked-files=no`: an untracked scratch file is not part of what was
    // compiled, so counting it would report every worktree as permanently
    // dirty and the marker would carry no information.
    //
    // Classified through `dirty_marker` on the raw (status, stdout) pair, NOT
    // through the `git()` helper below: a clean tree exits 0 with zero bytes,
    // and `git()` reports empty stdout as "no answer". Routing this call
    // through it made `"false"` unreachable — see `src/provenance_stamp.rs`.
    let dirty = if sha == UNKNOWN_STAMP {
        // No commit ⇒ no honest statement about drift FROM that commit.
        UNKNOWN_STAMP.to_string()
    } else {
        match std::process::Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=no"])
            .output()
        {
            Ok(out) => dirty_marker(out.status.success(), &String::from_utf8_lossy(&out.stdout))
                .to_string(),
            // Could not even run git ⇒ unknown, same as a non-zero exit.
            Err(_) => UNKNOWN_STAMP.to_string(),
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

/// Run `git` in the package root for a command whose answer IS its stdout
/// (`rev-parse`, `symbolic-ref`), returning that stdout trimmed.
///
/// Every failure mode (git not installed, not a repository, a broken index)
/// collapses to `None` — build provenance must never be able to fail a build.
///
/// **Only for commands where empty output means failure.** `git status
/// --porcelain` is the counter-example — empty stdout there is the *answer*
/// (clean tree), which is why the dirty classification above calls `output()`
/// directly and keys on the exit status instead.
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

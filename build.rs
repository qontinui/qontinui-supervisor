fn main() {
    // FIRST, unconditionally: the crate does `env!("QONTINUI_SUPERVISOR_GIT_SHA")`,
    // which is a COMPILE error when the var was never emitted, so nothing the
    // frontend build does may come before it.
    emit_self_build_provenance();

    build_frontend_if_missing();

    // Re-run this script when the frontend's inputs change. That recompiles the
    // crate against whatever `dist/` is on disk. It does NOT rebuild `dist/`,
    // which this script builds only when `dist/index.html` is missing, so after
    // a frontend edit run `npm run build` yourself. Emitted whatever the
    // frontend build did. While `dist` does not exist the path is always stale,
    // so every cargo build re-runs this script, retries npm and recompiles the
    // crate until a `dist/` appears. That is the price of picking up a `dist/`
    // built by hand without a `cargo clean`.
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=dist");
}

/// The npm launchers to try, in order. A standard Node.js install on Windows
/// puts npm on PATH as a batch script, `npm.cmd`. The `npm` beside it is a
/// shell script for Git Bash, and `std::process::Command` appends only `.exe`
/// when it searches PATH, so a bare `npm` there is "program not found". A
/// version manager whose shim is an `npm.exe` (Volta) has no `npm.cmd`,
/// hence the fallback. `cfg!` in a build script reads the host, which is where
/// npm runs.
const NPM_LAUNCHERS: &[&str] = if cfg!(windows) {
    &["npm.cmd", "npm"]
} else {
    &["npm"]
};

/// Build the SPA into `dist/` when `dist/index.html` is missing. During dev,
/// run `cd frontend && npm run dev` separately for HMR.
///
/// Every failure is a warning, never a build error: the crate compiles without
/// `dist/` (`routes::dashboard::Assets` is `allow_missing`) and serves the
/// legacy `static/dashboard.html` instead.
fn build_frontend_if_missing() {
    if std::path::Path::new("dist/index.html").exists()
        || !std::path::Path::new("frontend/package.json").exists()
    {
        return;
    }
    // `npm ci` installs the tracked lockfile as written and never rewrites it.
    // `npm install` may rewrite it, which dirties the tree and stamps every
    // later build `QONTINUI_SUPERVISOR_GIT_DIRTY=true`.
    let install: &[&str] = if std::path::Path::new("frontend/package-lock.json").exists() {
        &["ci"]
    } else {
        &["install"]
    };
    // npm 7+ writes `node_modules/.package-lock.json` only when an install
    // completes, so a `node_modules` left half-installed by an interrupted
    // install is installed again rather than built against. A `node_modules`
    // that is a link (symlink or junction) is never installed into: `npm ci`
    // first deletes its contents, which would empty the tree it points at.
    let modules = std::path::Path::new("frontend/node_modules");
    let installed = modules.join(".package-lock.json").exists();
    let linked = modules
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    if linked && !installed {
        println!(
            "cargo:warning=frontend/node_modules is a link with no completed install \
             behind it; building against it without installing into it"
        );
    }
    let built = (installed || linked || run_npm(install)) && run_npm(&["run", "build"]);
    if built {
        println!("cargo:warning=Frontend built successfully into dist/");
    } else {
        println!(
            "cargo:warning=SPA will not be embedded; the dashboard will use the legacy HTML \
             fallback. Run `cd frontend && npm ci && npm run build` to embed it."
        );
    }
}

/// Run npm with `args` in `frontend/`, through the first of [`NPM_LAUNCHERS`]
/// that exists. A spawn failure and a non-zero exit both return `false`, each
/// with a warning that says which it was. npm's stdout goes to this script's
/// stderr, because cargo reads this script's stdout for `cargo:` directives.
fn run_npm(args: &[&str]) -> bool {
    let mut not_found = Vec::new();
    for launcher in NPM_LAUNCHERS {
        let what = format!("{launcher} {}", args.join(" "));
        match std::process::Command::new(launcher)
            .args(args)
            .current_dir("frontend")
            .stdout(std::io::stderr())
            .status()
        {
            Ok(status) if status.success() => return true,
            Ok(status) => {
                println!("cargo:warning=`{what}` failed with status: {status}");
                return false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => not_found.push(*launcher),
            Err(e) => {
                println!("cargo:warning=Failed to run `{what}`: {e}");
                return false;
            }
        }
    }
    println!(
        "cargo:warning=npm is not on PATH (tried {})",
        not_found.join(", ")
    );
    false
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

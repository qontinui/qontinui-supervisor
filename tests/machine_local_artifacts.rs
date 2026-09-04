//! CI-enforced hygiene over the runner's machine-local artifact roster.
//!
//! # Why this exists
//!
//! The runner drops machine-local artifacts into every repo it manages
//! (`.mcp.json`, the two `agent-worktrees` container spellings, and
//! `.coord-mcp-status`). None of them is source, and every one of them makes
//! `git status --porcelain` non-empty while it is untracked-and-unignored.
//!
//! That is not cosmetic. `dev-start.ps1`'s `Resolve-PrimaryTreeStaleness`
//! treats a non-empty porcelain as uncommitted WIP and *skips* its
//! `merge --ff-only origin/main`, so a single stray artifact pins the primary
//! checkout behind main indefinitely — this repo sat 26 commits behind for
//! weeks because of two stale `.claude/` command files (#166).
//!
//! The roster was promoted into `.gitignore` in three separate rounds —
//! `.mcp.json`, then `.claude/` (#166), then `.agent-worktrees/` and
//! `.coord-mcp-status` (#167) — and each round left the next one behind,
//! because nothing checked the roster as a whole. Prose in `.gitignore`
//! cannot notice a missing entry. These tests can.
//!
//! # What each test proves
//!
//! 1. every roster entry is ignored **by the tracked `.gitignore`**, not merely
//!    by `.git/info/exclude` (which is local-only and lost on a fresh clone and
//!    in CI — and which the runner writes best-effort, swallowing every IO
//!    error, so it is never the durable statement);
//! 2. no roster entry has reached the index, the way two `__pycache__` `.pyc`
//!    files did before that entry existed;
//! 3. the roster this file hard-codes still matches the roster the runner
//!    actually enumerates, whenever the runner checkout is reachable.

use std::path::PathBuf;
use std::process::Command;

/// The runner's machine-local artifact roster.
///
/// Authoritative definition: qontinui-runner
/// `src-tauri/src/fleet.rs::MANAGED_REPO_EXCLUDES`. It is duplicated here
/// because that repo is not a build dependency of this one and CI clones only
/// `qontinui-schemas` — see the sibling clone step in
/// `.github/workflows/ci.yml`. `roster_matches_the_runner_definition` closes
/// the loop wherever the runner checkout IS reachable, so the copy announces
/// its own drift instead of silently rotting.
const MACHINE_LOCAL_ARTIFACTS: &[&str] = &[
    ".mcp.json",
    "agent-worktrees/",
    ".agent-worktrees/",
    ".coord-mcp-status",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// True when we are running somewhere a green result is load-bearing.
///
/// GitHub Actions sets `CI=true`. A test that cannot perform its check must not
/// quietly pass *there*, or the control degrades to decoration — the exact
/// failure mode this file exists to prevent.
fn is_ci() -> bool {
    std::env::var_os("CI").is_some_and(|v| !v.is_empty())
}

struct GitOutput {
    code: Option<i32>,
    stdout: String,
}

/// Run `git` in the repo root. `None` means git could not be executed at all
/// (absent binary), which is distinct from git running and reporting a verdict.
fn git(args: &[&str]) -> Option<GitOutput> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(args)
        .output()
        .ok()?;
    Some(GitOutput {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    })
}

/// `Some(true)` inside a work tree, `Some(false)` outside one, `None` when git
/// itself is unavailable.
fn inside_work_tree() -> Option<bool> {
    let out = git(&["rev-parse", "--is-inside-work-tree"])?;
    Some(out.stdout.trim() == "true")
}

/// Resolve the git precondition, or explain why the test is being skipped.
///
/// Returns `false` (skip) only outside CI. In CI an unusable git is a hard
/// failure: every CI leg checks the repo out with `actions/checkout`, so git
/// being unusable there means the check silently stopped running.
fn require_git(test: &str) -> bool {
    match inside_work_tree() {
        Some(true) => true,
        other => {
            let why = match other {
                None => "the `git` binary could not be executed",
                Some(false) => "this directory is not a git work tree",
                Some(true) => unreachable!(),
            };
            assert!(
                !is_ci(),
                "{test}: {why}, so the machine-local artifact roster was NOT \
                 verified. In CI this is a failure, not a skip — every leg \
                 checks the repo out with actions/checkout, so an unusable git \
                 means this control stopped running rather than passing."
            );
            println!("SKIP {test}: {why} (result is UNKNOWN, not a pass).");
            false
        }
    }
}

/// Every roster entry must be ignored, and ignored *by `.gitignore`* — the
/// tracked file that survives a fresh clone and reaches CI.
///
/// `.git/info/exclude` also matches these paths on a machine the runner has
/// touched, and `.gitignore` correctly takes precedence over it, so asserting
/// on the reported source is what distinguishes "promoted" from "still only
/// local". That is exactly the check #167 performed by hand.
#[test]
fn every_machine_local_artifact_is_ignored_by_the_tracked_gitignore() {
    if !require_git("every_machine_local_artifact_is_ignored_by_the_tracked_gitignore") {
        return;
    }

    let mut problems = Vec::new();

    for artifact in MACHINE_LOCAL_ARTIFACTS {
        // `--no-index` keeps the answer about the ignore rules alone, so the
        // verdict does not depend on whether the path happens to exist here.
        let Some(out) = git(&["check-ignore", "-v", "--no-index", artifact]) else {
            problems.push(format!("{artifact}: could not execute git"));
            continue;
        };

        // `git check-ignore`: 0 = ignored, 1 = not ignored, anything else is an
        // error. Only 0 carries a `-v` provenance line to parse.
        if out.code != Some(0) {
            problems.push(format!(
                "{artifact}: NOT ignored (git check-ignore exit {:?}). Add it to \
                 .gitignore — while it is unignored, one such file left in a \
                 checkout makes `git status --porcelain` non-empty and blocks \
                 dev-start's `merge --ff-only origin/main`.",
                out.code
            ));
            continue;
        }

        // `-v` output is `<source>:<line>:<pattern>\t<path>`.
        let line = out.stdout.lines().next().unwrap_or_default();
        let source = line.split(':').next().unwrap_or_default();
        if source != ".gitignore" {
            problems.push(format!(
                "{artifact}: ignored by `{source}`, not by the tracked \
                 .gitignore (matched: {}). `.git/info/exclude` is local-only — \
                 lost on a fresh clone and in CI — and the runner writes it \
                 best-effort, swallowing every IO error. Promote the pattern \
                 into .gitignore.",
                line.trim()
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "machine-local artifacts are not fully promoted into .gitignore:\n  - {}",
        problems.join("\n  - ")
    );
}

/// No roster entry may reach the index. An ignore rule does not apply to an
/// already-tracked path, so a committed machine-local artifact keeps dirtying
/// every checkout regardless of `.gitignore` — the way two `.pyc` files reached
/// the index before the `__pycache__` entry existed.
#[test]
fn no_machine_local_artifact_is_tracked() {
    if !require_git("no_machine_local_artifact_is_tracked") {
        return;
    }

    let mut tracked = Vec::new();

    for artifact in MACHINE_LOCAL_ARTIFACTS {
        let Some(out) = git(&["ls-files", "--", artifact]) else {
            panic!("{artifact}: could not execute git");
        };
        for path in out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
            tracked.push(format!("{artifact} -> {path}"));
        }
    }

    assert!(
        tracked.is_empty(),
        "machine-local artifacts are TRACKED, so .gitignore cannot suppress \
         them and every checkout reads dirty. Remove them from the index with \
         `git rm --cached <path>`:\n  - {}",
        tracked.join("\n  - ")
    );
}

/// Locate qontinui-runner's `fleet.rs` relative to this checkout.
///
/// Both worktree layouts in use put sibling repos one level up
/// (`qontinui-worktrees/<uuid>/<repo>` and `.agent-worktrees/<agent_id>/<repo>`),
/// which is also true of the primary checkout under the workspace root — so a
/// single `../qontinui-runner` probe covers all three.
fn runner_fleet_rs() -> Option<PathBuf> {
    let candidate = repo_root()
        .parent()?
        .join("qontinui-runner")
        .join("src-tauri")
        .join("src")
        .join("fleet.rs");
    candidate.is_file().then_some(candidate)
}

/// Extract the string literals of `MANAGED_REPO_EXCLUDES` from runner source.
///
/// Returns `None` when the declaration cannot be located or yields no entries —
/// both are "could not read the roster", which the caller reports as UNKNOWN
/// rather than as a mismatch. An empty parse must never masquerade as a real
/// roster: it would fail loudly against a correct list here, and (worse) pass
/// vacuously if this list were ever emptied too.
///
/// The scan deliberately starts after the `=`. The declaration is
/// `const MANAGED_REPO_EXCLUDES: &[&str] = &[…]`, so the first `[` following the
/// name belongs to the *type* `&[&str]`, not to the array — anchoring on it
/// parses `&str` and yields nothing.
fn parse_runner_roster(source: &str) -> Option<Vec<String>> {
    let start = source.find("const MANAGED_REPO_EXCLUDES")?;
    let eq = source[start..].find('=')? + start;
    let open = source[eq..].find('[')? + eq;
    let close = source[open..].find(']')? + open;
    let body = &source[open + 1..close];

    let mut entries = Vec::new();
    let mut rest = body;
    while let Some(a) = rest.find('"') {
        let after = &rest[a + 1..];
        let b = after.find('"')?;
        entries.push(after[..b].to_string());
        rest = &after[b + 1..];
    }
    (!entries.is_empty()).then_some(entries)
}

/// The hard-coded roster above must still match the runner's own enumeration.
///
/// The runner is not a build dependency of this crate and CI clones only
/// `qontinui-schemas`, so this cross-check runs on developer machines and is
/// reported as UNKNOWN — never as a pass — everywhere the runner checkout is
/// out of reach. A duplicated constant that cannot announce its own drift is
/// how the roster got half-promoted three times.
#[test]
fn roster_matches_the_runner_definition() {
    let Some(fleet_rs) = runner_fleet_rs() else {
        println!(
            "SKIP roster_matches_the_runner_definition: no qontinui-runner \
             checkout beside this one, so drift against \
             MANAGED_REPO_EXCLUDES is UNKNOWN, not absent."
        );
        return;
    };

    let source = std::fs::read_to_string(&fleet_rs)
        .unwrap_or_else(|e| panic!("reading {}: {e}", fleet_rs.display()));

    let Some(runner_roster) = parse_runner_roster(&source) else {
        println!(
            "SKIP roster_matches_the_runner_definition: could not parse \
             MANAGED_REPO_EXCLUDES out of {} — it was probably reshaped. \
             Drift is UNKNOWN, not absent; re-check by hand.",
            fleet_rs.display()
        );
        return;
    };

    let mut expected = runner_roster;
    let mut actual: Vec<String> = MACHINE_LOCAL_ARTIFACTS
        .iter()
        .map(|s| s.to_string())
        .collect();
    expected.sort();
    actual.sort();

    assert_eq!(
        actual,
        expected,
        "MACHINE_LOCAL_ARTIFACTS has drifted from qontinui-runner's \
         MANAGED_REPO_EXCLUDES ({}). Reconcile the roster here AND add any new \
         entry to .gitignore — an artifact the runner writes but this repo does \
         not ignore blocks dev-start's auto-fast-forward.",
        fleet_rs.display()
    );
}

/// The cross-check is only as good as its parser, and its parser runs on
/// developer machines only — so it is pinned here, where CI does exercise it.
///
/// The first case is the declaration's real shape. Anchoring on the first `[`
/// after the name parses the *type* `&[&str]` and silently yields nothing,
/// which is how this test first came up red against a correct roster.
#[test]
fn the_runner_roster_parser_reads_the_real_declaration() {
    let real = r#"
const MANAGED_REPO_EXCLUDES: &[&str] = &[
    ".mcp.json",
    "agent-worktrees/",
    ".agent-worktrees/",
    ".coord-mcp-status",
];
"#;
    assert_eq!(
        parse_runner_roster(real),
        Some(vec![
            ".mcp.json".to_string(),
            "agent-worktrees/".to_string(),
            ".agent-worktrees/".to_string(),
            ".coord-mcp-status".to_string(),
        ]),
        "the parser must read the declaration's actual shape, type sigil included"
    );

    // A reshaped or absent declaration is UNKNOWN, never an empty roster —
    // an empty parse compared against a correct list is a false mismatch, and
    // compared against an empty list is a vacuous pass.
    assert_eq!(parse_runner_roster("no such constant here"), None);
    assert_eq!(
        parse_runner_roster("const MANAGED_REPO_EXCLUDES: &[&str] = &[];"),
        None,
        "an empty array must read as unparseable, not as a roster of zero"
    );
}

/// Guard against the roster being emptied into vacuity. Both roster tests
/// iterate the list, so a zero-length list would make them pass having checked
/// nothing.
#[test]
fn the_roster_is_not_empty() {
    assert!(
        !MACHINE_LOCAL_ARTIFACTS.is_empty(),
        "MACHINE_LOCAL_ARTIFACTS is empty, which makes every roster test \
         vacuous — they iterate it."
    );
    assert!(
        repo_root().join(".gitignore").is_file(),
        ".gitignore is missing from the repo root; the roster cannot be promoted."
    );
}

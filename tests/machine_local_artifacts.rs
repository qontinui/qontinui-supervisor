//! CI-enforced hygiene over the runner's machine-local artifact roster.
//!
//! # Why this exists
//!
//! The runner drops machine-local artifacts into every repo it manages
//! (`.mcp.json`, the two `agent-worktrees` container spellings,
//! `.coord-mcp-status`, and `.claude/worktrees/`). None of them is source, and
//! every one of them makes `git status --porcelain` non-empty while it is
//! untracked-and-unignored.
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
//!    actually enumerates **on `origin/main`** — read either from the copy
//!    `ci.yml` checks out, or from a sibling checkout's `origin/main` ref.
//!    Never from a working tree, and never only where a checkout happens to
//!    sit: as #168 shipped it, check 3 read the local sibling's TREE and did
//!    not run in CI at all, so its verdict was a property of the reader's box
//!    rather than of the roster.

use std::path::PathBuf;
use std::process::Command;

/// The runner's machine-local artifact roster.
///
/// Authoritative definition: qontinui-runner
/// `src-tauri/src/fleet.rs::MANAGED_REPO_EXCLUDES`. It is duplicated here
/// because that repo is not a build dependency of this one — the roster is a
/// handful of string literals, and taking a build dep on the runner to read
/// them would be absurd. `roster_matches_the_runner_definition` is what stops the copy
/// rotting, and `ci.yml` supplies it the runner's `origin/main` `fleet.rs` so
/// it runs on every PR rather than only where a sibling checkout happens to
/// sit.
const MACHINE_LOCAL_ARTIFACTS: &[&str] = &[
    ".mcp.json",
    "agent-worktrees/",
    ".agent-worktrees/",
    ".coord-mcp-status",
    // Added runner-side by `703123d50` ("exclude .claude/worktrees/ so a skill
    // worktree cannot pin a repo"). Already ignored here by the broader
    // `.claude/` entry, so only the roster lagged — which is precisely the
    // drift `roster_matches_the_runner_definition` exists to announce.
    ".claude/worktrees/",
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
    // The evidence this control produces. A green assertion says only that
    // nothing was wrong; the per-artifact provenance says what was actually
    // checked, and an unauditable control is indistinguishable from an absent
    // one. `ci.yml` re-runs this binary with --nocapture so the log keeps it.
    let mut verdicts = Vec::new();

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
        // `-v` output is `<source>:<line>:<pattern>\t<path>`, and the SOURCE is
        // what distinguishes "promoted" from "still only local". Test it by
        // prefix rather than by splitting on `:`, because a colon appears on
        // BOTH sides of the field: an absolute `core.excludesFile` source
        // starts `C:` on Windows (a left split truncates it to a drive
        // letter), and a gitignore pattern may contain one (a right split
        // then swallows a field). A prefix test is exact for the only
        // question asked — is this the tracked .gitignore at the repo root.
        let line = out.stdout.lines().next().unwrap_or_default();
        let before_tab = line.split('\t').next().unwrap_or_default();
        if !before_tab.starts_with(".gitignore:") {
            problems.push(format!(
                "{artifact}: not ignored by the tracked .gitignore — git \
                 check-ignore reports `{}`. `.git/info/exclude` is \
                 local-only — lost on a fresh clone and in CI — and the \
                 runner writes it best-effort, swallowing every IO error. \
                 Promote the pattern into .gitignore.",
                line.trim()
            ));
        } else {
            verdicts.push(format!("{artifact} <- {}", line.trim()));
        }
    }

    assert!(
        problems.is_empty(),
        "machine-local artifacts are not fully promoted into .gitignore:\n  - {}",
        problems.join("\n  - ")
    );

    println!(
        "every_machine_local_artifact_is_ignored_by_the_tracked_gitignore:\n  {}",
        verdicts.join("\n  ")
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

/// Where the runner's roster was read from, so a verdict can name its source.
///
/// The distinction is the whole point: a roster read from a checkout's WORKING
/// TREE is evidence about that checkout and nothing else, and the primary
/// sibling checkout is routinely the stalest thing in the workspace — nobody's
/// active worktree, so nobody pulls it.
struct RosterSource {
    /// Human-readable provenance, printed in every verdict.
    label: String,
    source: String,
}

/// Resolve the runner's `fleet.rs` **as it stands on `origin/main`**.
///
/// Two sources, in order, and neither is a working tree:
///
/// 1. `$QONTINUI_RUNNER_FLEET_RS` — a file the caller has already fetched from
///    the runner's default branch. This is the CI arm: `ci.yml` sparse-checks
///    out `qontinui-runner` and points this at it, which is what makes the
///    cross-check load-bearing in the one place a green result is load-bearing.
/// 2. A sibling `../qontinui-runner` checkout, read with
///    `git show origin/main:src-tauri/src/fleet.rs` — the checkout's REMOTE-
///    TRACKING ref, never its working tree. A checkout whose `origin/main` will
///    not resolve yields UNKNOWN; it does NOT fall back to the tree, because
///    the tree is exactly what produced the false pass this replaces.
///
/// `None` is UNKNOWN — reported, never counted as a pass.
fn runner_roster_source() -> Option<RosterSource> {
    if let Some(path) = std::env::var_os("QONTINUI_RUNNER_FLEET_RS") {
        let path = PathBuf::from(path);
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "QONTINUI_RUNNER_FLEET_RS points at {} which could not be read: \
                 {e}. It is set explicitly, so an unreadable file is a broken \
                 configuration, not an absent source.",
                path.display()
            )
        });
        return Some(RosterSource {
            label: format!("$QONTINUI_RUNNER_FLEET_RS ({})", path.display()),
            source,
        });
    }

    // Both worktree layouts in use put sibling repos one level up
    // (`qontinui-worktrees/<uuid>/<repo>` and `.agent-worktrees/<agent_id>/<repo>`),
    // which is also true of the primary checkout under the workspace root.
    let runner = repo_root().parent()?.join("qontinui-runner");
    if !runner.join(".git").exists() {
        return None;
    }

    let show = Command::new("git")
        .arg("-C")
        .arg(&runner)
        .args(["show", "origin/main:src-tauri/src/fleet.rs"])
        .output()
        .ok()?;
    if !show.status.success() {
        return None;
    }

    // Name the commit, so a verdict is auditable against a specific tree
    // rather than against "whatever that checkout had fetched".
    let sha = Command::new("git")
        .arg("-C")
        .arg(&runner)
        .args(["rev-parse", "--short", "origin/main"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    Some(RosterSource {
        label: format!("{}@origin/main ({sha})", runner.display()),
        source: String::from_utf8_lossy(&show.stdout).into_owned(),
    })
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

/// The hard-coded roster above must still match the runner's own enumeration
/// **on `origin/main`**.
///
/// This is the control that is supposed to stop the roster rotting, and as #168
/// shipped it its answer depended on the READER'S BOX, because it read the
/// sibling checkout's WORKING TREE. Both halves of that were observed on
/// 2026-09-05, on the same day, against the same `origin/main`:
///
/// - on a box whose runner checkout was 337 commits behind, it PASSED against
///   a stale four-entry roster and certified a drift that had already
///   happened;
/// - on a box whose checkout was current, it FAILED — correctly — which is
///   what produced `ca98051`, the commit that reconciled the roster.
///
/// A control that answers from a local cache gives a confident wrong answer on
/// half the fleet, which is worse than an absent one. And CI, where a green is
/// load-bearing, was the half that never ran it at all: every CI run on `main`
/// between #168 and `ca98051` was GREEN while this check was skipping, which is
/// why `ca98051` had to describe a failure "on main" that no CI run ever saw.
///
/// In CI an unresolvable source is a HARD FAILURE, exactly as [`require_git`]
/// treats an unusable git: CI is where a green result is load-bearing, and
/// `ci.yml` is now what supplies the source, so nothing there can be missing
/// by accident. Everywhere else it is reported as UNKNOWN — never as a pass.
#[test]
fn roster_matches_the_runner_definition() {
    let Some(runner) = runner_roster_source() else {
        assert!(
            !is_ci(),
            "roster_matches_the_runner_definition: the runner's roster could \
             not be resolved, so drift against MANAGED_REPO_EXCLUDES was NOT \
             checked. In CI this is a failure, not a skip — `ci.yml` checks \
             qontinui-runner out and sets $QONTINUI_RUNNER_FLEET_RS, so an \
             unresolved source there means that step stopped working and this \
             control silently stopped running."
        );
        println!(
            "SKIP roster_matches_the_runner_definition: neither \
             $QONTINUI_RUNNER_FLEET_RS nor a sibling qontinui-runner checkout \
             with a resolvable origin/main. Drift is UNKNOWN, not absent. (A \
             sibling checkout's WORKING TREE is deliberately not a source — \
             reading one is what let a 337-commit-stale tree certify a roster \
             that had already drifted.)"
        );
        return;
    };

    let Some(runner_roster) = parse_runner_roster(&runner.source) else {
        assert!(
            !is_ci(),
            "roster_matches_the_runner_definition: MANAGED_REPO_EXCLUDES could \
             not be parsed out of {} — it was probably reshaped. In CI that is \
             a failure: the source was supplied and the parser could not read \
             it, so the control stopped running rather than passing.",
            runner.label
        );
        println!(
            "SKIP roster_matches_the_runner_definition: could not parse \
             MANAGED_REPO_EXCLUDES out of {} — it was probably reshaped. Drift \
             is UNKNOWN, not absent; re-check by hand.",
            runner.label
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
        actual, expected,
        "MACHINE_LOCAL_ARTIFACTS has drifted from qontinui-runner's \
         MANAGED_REPO_EXCLUDES, read from {}. Reconcile the roster here AND \
         make sure .gitignore covers every entry — an artifact the runner \
         writes but this repo does not ignore makes `git status --porcelain` \
         non-empty and blocks dev-start's `merge --ff-only origin/main`.",
        runner.label
    );

    println!(
        "roster_matches_the_runner_definition: {} entries verified against {}.",
        actual.len(),
        runner.label
    );
}

/// The cross-check is only as good as its parser, so the parser is pinned
/// against fixed inputs here rather than only against whatever the runner's
/// declaration happens to look like today.
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
    ".claude/worktrees/",
];
"#;
    assert_eq!(
        parse_runner_roster(real),
        Some(vec![
            ".mcp.json".to_string(),
            "agent-worktrees/".to_string(),
            ".agent-worktrees/".to_string(),
            ".coord-mcp-status".to_string(),
            ".claude/worktrees/".to_string(),
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

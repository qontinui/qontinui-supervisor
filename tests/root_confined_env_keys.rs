//! The supervisor's copy of qontinui-runner's `ROOT_CONFINED_ENV_KEYS` must
//! match the runner's own declaration on its default branch.
//!
//! # Why this exists
//!
//! Under an instance root the runner refuses to start (exit 78) when any
//! root-confined location variable is set non-blank OUTSIDE the root
//! (qontinui-runner `src-tauri/src/instance_env.rs`, `ROOT_CONFINED_ENV_KEYS`).
//! The supervisor keeps its own copy of that list in
//! `src/process/instance_root.rs`, and `InstanceRoot::apply_env` REMOVES every
//! key on it that it does not export. So a key the copy lacks is not removed:
//! the subject child inherits the supervisor's value, and refuses to start when
//! that value lies outside its root. The `extra_env` override warning is blind
//! to it as well.
//!
//! Coord finding 7a74f9f1 (2026-10-24): the runner had gained
//! `QONTINUI_TOUCH_LEDGER_DIR` (one of its `QONTINUI_HOME_CHILD_DIRS`, D5) and
//! the copy here had not. Nothing compared the two lists.
//!
//! # Why a parity test, and not a shared definition
//!
//! The list cannot be derived at build time without a crate both repos build
//! against. The supervisor cannot depend on the runner crate (a Tauri
//! application). The one crate both do depend on is `qontinui-types`
//! (qontinui-schemas), and every repo pins that sibling to an exact commit
//! (`.github/sibling-pins.conf` here). Moving the list there would turn each
//! change to an eleven-entry list into three ordered PRs (schemas, then a pin
//! bump in each consumer). It would also put a runner launch-contract constant
//! in a types crate, and between the schemas land and the runner's pin bump
//! the two copies would still disagree. A test that reads the runner's own
//! declaration catches the drift on the next supervisor CI run with no
//! cross-repo ordering. That is the mechanism `tests/machine_local_artifacts.rs`
//! already uses for `MANAGED_REPO_EXCLUDES`, and this file reuses its rules:
//!
//! - the runner's source is read from its DEFAULT BRANCH, never from a sibling
//!   checkout's working tree (a stale tree certifies a drift that has already
//!   happened; see that file's `roster_matches_the_runner_definition`);
//! - in CI the source is REQUIRED. `ci.yml` checks the runner's `instance_env.rs`
//!   out and names it in `$QONTINUI_RUNNER_INSTANCE_ENV_RS`, so a CI run that
//!   cannot read it fails rather than passing on a check it did not make;
//! - elsewhere an unresolvable source is UNKNOWN: reported, never a pass.

use std::path::PathBuf;
use std::process::Command;

use qontinui_supervisor::process::instance_root::ROOT_CONFINED_ENV_KEYS;

/// Where the runner's list was read from, so a verdict can name its source.
struct RunnerSource {
    label: String,
    source: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// GitHub Actions sets `CI=true`.
fn is_ci() -> bool {
    std::env::var_os("CI").is_some_and(|v| !v.is_empty())
}

const SOURCE_ENV: &str = "QONTINUI_RUNNER_INSTANCE_ENV_RS";
const RUNNER_PATH: &str = "src-tauri/src/instance_env.rs";

/// Resolve the runner's `instance_env.rs` as it stands on its default branch.
///
/// 1. `$QONTINUI_RUNNER_INSTANCE_ENV_RS`: a file the caller fetched from that
///    branch. This is the CI arm, and in CI the only one accepted.
/// 2. Outside CI, a sibling `../qontinui-runner` checkout, read with
///    `git show origin/main:<path>`: its REMOTE-TRACKING ref, never its tree.
///
/// `None` is UNKNOWN.
fn runner_source() -> Option<RunnerSource> {
    if let Some(path) = std::env::var_os(SOURCE_ENV) {
        let path = PathBuf::from(path);
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{SOURCE_ENV} points at {} which could not be read: {e}. It is set \
                 explicitly, so an unreadable file is a broken configuration, not an \
                 absent source.",
                path.display()
            )
        });
        return Some(RunnerSource {
            label: format!("${SOURCE_ENV} ({})", path.display()),
            source,
        });
    }
    if is_ci() {
        return None;
    }

    // Every worktree layout in use puts sibling repos one level up.
    let runner = repo_root().parent()?.join("qontinui-runner");
    if !runner.join(".git").exists() {
        return None;
    }
    let show = Command::new("git")
        .arg("-C")
        .arg(&runner)
        .args(["show", &format!("origin/main:{RUNNER_PATH}")])
        .output()
        .ok()?;
    if !show.status.success() {
        return None;
    }
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
    Some(RunnerSource {
        label: format!("{}@origin/main ({sha})", runner.display()),
        source: String::from_utf8_lossy(&show.stdout).into_owned(),
    })
}

/// The string literals of the runner's `const ROOT_CONFINED_ENV_KEYS` array.
///
/// Anchored on the DECLARATION (`const ROOT_CONFINED_ENV_KEYS`), not the bare
/// name, which the runner's doc comments also mention; the scan starts after
/// the `=`, because the first `[` after the name is the type `&[&str]`, and
/// line comments are dropped before the literals are collected. `None` (the
/// declaration not found, or no entries) is "could not read the list", reported
/// as UNKNOWN rather than as a mismatch, so an empty parse can never pass
/// against an emptied copy.
fn parse_runner_keys(source: &str) -> Option<Vec<String>> {
    let start = source.find("const ROOT_CONFINED_ENV_KEYS")?;
    let eq = source[start..].find('=')? + start;
    let open = source[eq..].find('[')? + eq;
    let close = source[open..].find(']')? + open;
    let mut entries = Vec::new();
    for line in source[open + 1..close].lines() {
        let line = line.split("//").next().unwrap_or("");
        let mut rest = line;
        while let Some(a) = rest.find('"') {
            let after = &rest[a + 1..];
            let b = after.find('"')?;
            entries.push(after[..b].to_string());
            rest = &after[b + 1..];
        }
    }
    (!entries.is_empty()).then_some(entries)
}

#[test]
fn root_confined_env_keys_match_the_runner_definition() {
    let Some(runner) = runner_source() else {
        assert!(
            !is_ci(),
            "the runner's ROOT_CONFINED_ENV_KEYS could not be read, so drift against it was \
             NOT checked. In CI that is a failure: ci.yml checks qontinui-runner's \
             {RUNNER_PATH} out and sets ${SOURCE_ENV}, so an unset or missing source there \
             means that step stopped working and this control stopped running."
        );
        println!(
            "SKIP root_confined_env_keys_match_the_runner_definition: neither ${SOURCE_ENV} \
             nor a sibling qontinui-runner checkout with a resolvable origin/main. Drift is \
             UNKNOWN, not absent."
        );
        return;
    };
    let Some(mut expected) = parse_runner_keys(&runner.source) else {
        assert!(
            !is_ci(),
            "ROOT_CONFINED_ENV_KEYS could not be parsed out of {} (reshaped?). In CI that is \
             a failure: the source was supplied and could not be read.",
            runner.label
        );
        println!(
            "SKIP root_confined_env_keys_match_the_runner_definition: could not parse \
             ROOT_CONFINED_ENV_KEYS out of {}. Drift is UNKNOWN, not absent.",
            runner.label
        );
        return;
    };

    let mut actual: Vec<String> = ROOT_CONFINED_ENV_KEYS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let copy_len = actual.len();
    expected.sort();
    actual.sort();
    actual.dedup();
    assert_eq!(
        actual.len(),
        copy_len,
        "the supervisor's ROOT_CONFINED_ENV_KEYS repeats an entry"
    );
    assert_eq!(
        actual, expected,
        "src/process/instance_root.rs ROOT_CONFINED_ENV_KEYS has drifted from \
         qontinui-runner's instance_env::ROOT_CONFINED_ENV_KEYS, read from {}. A key the \
         supervisor lacks is INHERITED by every subject child (apply_env removes only the \
         keys it knows), and the child refuses to start (exit 78) when the inherited value \
         lies outside its root. Reconcile the copy.",
        runner.label
    );
    println!(
        "root_confined_env_keys_match_the_runner_definition: {} keys verified against {}.",
        actual.len(),
        runner.label
    );
}

/// The cross-check is only as good as its parser, so the parser is pinned
/// against the declaration's real shape, including the doc-comment mention of
/// the name that precedes it, and a line comment inside the list.
#[test]
fn the_runner_key_parser_reads_the_real_declaration() {
    let real = r#"
/// [`QONTINUI_HOME_CHILD_DIRS`] (D5). See also [`ROOT_CONFINED_ENV_KEYS`].
pub const ROOT_CONFINED_ENV_KEYS: &[&str] = &[
    "QONTINUI_CAPABILITY_STATE_DIR",
    // "NOT_A_KEY" in a comment
    "QONTINUI_TOUCH_LEDGER_DIR",
    "WEBVIEW2_USER_DATA_FOLDER",
];
"#;
    assert_eq!(
        parse_runner_keys(real),
        Some(vec![
            "QONTINUI_CAPABILITY_STATE_DIR".to_string(),
            "QONTINUI_TOUCH_LEDGER_DIR".to_string(),
            "WEBVIEW2_USER_DATA_FOLDER".to_string(),
        ])
    );
    assert_eq!(
        parse_runner_keys("pub const OTHER: &[&str] = &[\"A\"];"),
        None
    );
    assert_eq!(
        parse_runner_keys("pub const ROOT_CONFINED_ENV_KEYS: &[&str] = &[];"),
        None,
        "an empty list is UNKNOWN, never a list to compare against"
    );
}

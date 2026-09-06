//! The orphan-kill suite's CI contract, pinned as an ordinary `cargo test` test.
//!
//! `tests/orphan_kill_unix.rs` announces one machine-readable line per D7 kill
//! primitive it actually exercised, and `.github/workflows/ci.yml` gates the
//! job on those lines. That makes two files agree by **string matching across a
//! language boundary**, which is the shape this repo already refuses to leave
//! unpinned: the vacuity parser carries a digest its own CI step re-computes,
//! and `verify-scoped-cleanup.ps1`'s literals are pinned by Rust tests because
//! the harness itself can never run on the `ubuntu-latest` gate.
//!
//! The same hole exists here in both directions:
//!
//! * **Rename** — change `EXERCISED_MARKER`, or the primitive name a test
//!   announces, and the workflow's census silently stops finding it. That is
//!   fail-CLOSED (the census reads short and the job reds), but it reds with a
//!   message blaming the *runner* — *"On ubuntu-latest all of them should run:
//!   python3, lsof and ports 9896-9898 are all expected to be available"* —
//!   for what is really a one-word edit in a Rust file. A red that names the
//!   wrong cause costs the same investigation as no red at all.
//! * **Addition** — add a fourth primitive and nothing requires the workflow to
//!   know about it. The roster is what the job asserts, so an unlisted
//!   primitive is ungated: it may skip in every CI run forever and the step
//!   stays green. Deletion fails closed; addition did not.
//!
//! This file closes both, and closes them **earlier** — it is a plain
//! integration test with no `cfg` gate and no OS dependency, so it runs in the
//! `Run tests` step (and on a Windows dev box, where the suite it guards
//! compiles to nothing at all) and names the drift in its own assertion
//! message.
//!
//! It deliberately reads both files as TEXT rather than importing anything.
//! `tests/orphan_kill_unix.rs` is `#![cfg(not(target_os = "windows"))]` and each
//! integration test is its own crate, so there is nothing to import; and the
//! workflow is YAML. Text is what the coupling actually is.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

const SUITE: &str = "tests/orphan_kill_unix.rs";
const WORKFLOW: &str = ".github/workflows/ci.yml";

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| {
            panic!(
                "{} must be readable to pin the contract: {e}",
                path.display()
            )
        })
        // Both files are authored on a Windows box; normalise so every
        // assertion below compares content rather than line endings.
        .replace("\r\n", "\n")
}

/// The value of a `const <name>: &str = "...";` declaration in `src`.
fn const_str(src: &str, name: &str) -> String {
    let needle = format!("const {name}: &str = \"");
    let after = src
        .split_once(&needle)
        .unwrap_or_else(|| {
            panic!("`{name}` must be declared in {SUITE} — did it get renamed or deleted?")
        })
        .1;
    after
        .split_once('"')
        .unwrap_or_else(|| panic!("`{name}` in {SUITE} has an unterminated string literal"))
        .0
        .to_string()
}

/// Every primitive name the suite announces, from its `exercised("…")` call
/// sites.
///
/// Matching on `exercised("` and not on `exercised(` is what keeps the helper's
/// own definition (`fn exercised(primitive: &str)`) and every prose mention of
/// it out of the set — only a call with a literal name is a claim of coverage.
fn primitives_announced_by_the_suite(src: &str) -> BTreeSet<String> {
    let needle = "exercised(\"";
    let mut found = BTreeSet::new();
    let mut rest = src;
    while let Some((_, after)) = rest.split_once(needle) {
        let (name, tail) = after
            .split_once('"')
            .unwrap_or_else(|| panic!("unterminated `exercised(\"…\")` literal in {SUITE}"));
        found.insert(name.to_string());
        rest = tail;
    }
    found
}

/// The roster the workflow gates on: `EXPECTED_PRIMITIVES="a b c"`.
fn roster_required_by_the_workflow(src: &str) -> BTreeSet<String> {
    let needle = "EXPECTED_PRIMITIVES=\"";
    let after = src
        .split_once(needle)
        .unwrap_or_else(|| {
            panic!(
                "{WORKFLOW} must declare `EXPECTED_PRIMITIVES=\"…\"` — that single declaration \
                 is what drives both the orphan-kill floor and the exercised census. If the \
                 census was rewritten, rewrite this pin with it rather than deleting it."
            )
        })
        .1;
    after
        .split_once('"')
        .unwrap_or_else(|| panic!("unterminated `EXPECTED_PRIMITIVES` list in {WORKFLOW}"))
        .0
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// The set the workflow gates on and the set the suite can produce must be the
/// SAME set — not one a superset of the other.
///
/// A missing name is an ungated primitive (the addition hole). An extra name is
/// a roster entry nothing can ever satisfy, which reds the job on every run
/// with no code defect behind it — the "fix the runner" red this file exists to
/// prevent someone chasing.
#[test]
fn the_workflow_roster_is_exactly_the_set_of_primitives_the_suite_announces() {
    let suite = read(SUITE);
    let workflow = read(WORKFLOW);

    let announced = primitives_announced_by_the_suite(&suite);
    let required = roster_required_by_the_workflow(&workflow);

    assert!(
        !announced.is_empty(),
        "{SUITE} announces no primitives at all. Every test there must end with an \
         `exercised(\"<primitive>\")` call — that announcement is the only positive evidence \
         the CI census has that the primitive ran rather than skipped."
    );

    let ungated: Vec<_> = announced.difference(&required).collect();
    let unsatisfiable: Vec<_> = required.difference(&announced).collect();

    assert!(
        ungated.is_empty(),
        "{SUITE} announces {ungated:?}, which {WORKFLOW}'s EXPECTED_PRIMITIVES does not \
         require. That primitive is UNGATED: it may skip on every CI run — no listener, no \
         python3, an lsof that cannot run — and the job stays green, because the census only \
         asserts the names on the roster. Add it to EXPECTED_PRIMITIVES. Do NOT delete the \
         announcement to make this pass; that removes the coverage signal instead of gating it."
    );
    assert!(
        unsatisfiable.is_empty(),
        "{WORKFLOW} requires {unsatisfiable:?}, which no `exercised(\"…\")` call in {SUITE} can \
         ever print. The job will red on every run and its message will blame the CI runner \
         (\"python3, lsof and ports 9896-9898 are all expected to be available\") for what is \
         really a rename here. Update the roster and the call site together."
    );
}

/// The two marker literals cross a language boundary: Rust prints them, the
/// workflow greps for them. Nothing but this test makes them agree.
#[test]
fn the_markers_the_workflow_greps_for_are_the_ones_the_suite_prints() {
    let suite = read(SUITE);
    let workflow = read(WORKFLOW);

    for name in ["EXERCISED_MARKER", "SKIP_MARKER"] {
        let value = const_str(&suite, name);
        assert!(
            workflow.contains(&value),
            "{SUITE} prints `{value}` (as {name}) but {WORKFLOW} never mentions that string. \
             The census greps for the marker literally, so a rename on this side silently \
             stops the workflow finding it: the exercised census reads zero and reds blaming \
             the runner, and the skip-reason listing the failure message promises prints \
             nothing at all. Update both together."
        );
    }
}

/// A test that announces nothing is invisible to the census: it can skip on
/// every run while its target still reports "N passed".
///
/// This is the addition hole at test granularity rather than roster
/// granularity — a fourth `#[tokio::test]` added without an announcement is
/// counted by the floor (which only counts tests) and ignored by the census
/// (which only counts announcements).
#[test]
fn every_test_in_the_suite_announces_at_least_one_primitive() {
    let suite = read(SUITE);
    let attribute = "#[tokio::test]";

    let mut silent = Vec::new();
    let mut tests = 0usize;
    // Bound each test at the NEXT test attribute, so an announcement belonging
    // to a later test can never satisfy an earlier one — the same bounding
    // technique `fn_source` uses in src/routes/runners.rs.
    for chunk in suite.split(attribute).skip(1) {
        tests += 1;
        let name = chunk
            .split_once('(')
            .and_then(|(head, _)| head.rsplit_once("fn ").map(|(_, n)| n.trim().to_string()))
            .unwrap_or_else(|| format!("<unnamed test #{tests}>"));
        if !chunk.contains("exercised(\"") {
            silent.push(name);
        }
    }

    assert!(
        tests > 0,
        "{SUITE} contains no `{attribute}` at all. The target would still compile and still \
         report a summary line, which is the vacuous green this whole contract exists to \
         refuse."
    );
    assert!(
        silent.is_empty(),
        "these tests in {SUITE} never call `exercised(\"…\")`: {silent:?}. Such a test is \
         invisible to the CI census — it can skip on every single run while the target \
         reports \"N passed\" and the step prints green. Announce the primitive as the test's \
         last statement, after every assertion, and add its name to EXPECTED_PRIMITIVES in \
         {WORKFLOW}."
    );
}

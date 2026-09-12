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
//! Further edges belong here for the same reason. Each is a red that would
//! otherwise land in CI naming the wrong cause, or a green that means nothing:
//!
//! * A primitive name the workflow's own extraction cannot carry. The census
//!   truncates an announcement at the first character outside `[A-Za-z0-9_]`
//!   and iterates the roster UNQUOTED, so a name with a dot or a glob character
//!   in it reds while looking like a missing primitive.
//! * A floor derived from the roster. `MIN_TESTS` is `wc -w` of the roster, so
//!   it counts PRIMITIVES and gates on TESTS; merging two tests into one that
//!   announces both, or suppressing one with `#[ignore]` or a per-test
//!   `#[cfg(…)]`, reds that floor with every primitive genuinely exercised.
//! * A second declaration of a gated constant. Every reader here takes the
//!   FIRST one, so a copy-pasted sibling step would leave this file validating
//!   a value the failing step never used.
//! * **This file's own deletion.** It rides in the `Run tests` step, whose
//!   floor is 1200 — losing this target costs a handful of tests and nothing
//!   notices, leaving the roster and the Rust names free to drift apart again.
//!   That is the fail-open-on-deletion shape the exercised census exists to
//!   refuse, one level up, so the workflow gates on this target having run and
//!   [`the_workflow_requires_this_pin_to_have_run`] pins the two together — by
//!   the census's load-bearing spellings, not by a mention the prose satisfies.
//!
//! It deliberately reads both files as TEXT rather than importing anything.
//! `tests/orphan_kill_unix.rs` is `#![cfg(not(target_os = "windows"))]` and each
//! integration test is its own crate, so there is nothing to import; and the
//! workflow is YAML. Text is what the coupling actually is.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SUITE: &str = "tests/orphan_kill_unix.rs";
const WORKFLOW: &str = ".github/workflows/ci.yml";
/// This file. Read as text like the other two, because the workflow gates on
/// how many tests it holds and that number has to come from somewhere that
/// cannot go stale.
const SELF: &str = "tests/orphan_kill_contract.rs";
/// The shell variable in which the workflow commits this file's test count.
const CONTRACT_FLOOR: &str = "MIN_CONTRACT_TESTS";

/// The attributes that make a function a test cargo will run.
///
/// `#[tokio::test` is matched as a PREFIX so an attribute carrying arguments —
/// `#[tokio::test(flavor = "multi_thread")]` — is still seen; the closing
/// bracket is not part of the claim. The plain attribute is listed because the
/// suite is async TODAY and nothing keeps it that way: a synchronous test added
/// there is counted by the workflow's floor, ignored by its census, and was
/// invisible to this file, which is the addition hole in its third spelling.
const TEST_ATTRIBUTES: [&str; 2] = ["#[tokio::test", "#[test]"];

/// The cargo test target this file compiles to — the name the workflow's
/// census must spell.
///
/// Derived from [`SELF`] rather than declared beside it, because cargo names an
/// integration-test target after its file stem and nothing else: a constant
/// spelling the same name a second time is a copy that a rename of this file
/// updates or does not, and [`the_workflow_requires_this_pin_to_have_run`]
/// would go on pinning the OLD name against a workflow that still carried it —
/// green on the developer's box, red in CI with a message about deletion.
fn contract_target() -> String {
    Path::new(SELF)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_else(|| panic!("`SELF` ({SELF}) must name a `tests/<target>.rs` file"))
        .to_string()
}

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
/// Matching on `exercised("` and not on `exercised(` keeps the helper's own
/// definition (`fn exercised(primitive: &str)`) out of the set, and stripping
/// whole-line comments first keeps PROSE out of it — a doc comment quoting a
/// call is a mention, not a claim of coverage, and one naming a primitive the
/// roster does not list would otherwise red as an ungated primitive.
fn primitives_announced_by_the_suite(src: &str) -> BTreeSet<String> {
    let src = without_rust_comment_lines(src);
    let needle = "exercised(\"";
    let mut found = BTreeSet::new();
    let mut rest = src.as_str();
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

/// The value the workflow assigns to a shell variable, as an integer.
fn workflow_int(src: &str, name: &str) -> usize {
    let needle = format!("{name}=");
    let digits: String = src
        .split_once(&needle)
        .unwrap_or_else(|| {
            panic!(
                "{WORKFLOW} must assign `{name}=<n>` — that is where the floor for this file's \
                 own target is committed, and a floor nothing declares cannot be checked."
            )
        })
        .1
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("`{name}=` in {WORKFLOW} is not followed by a decimal integer"))
}

/// `src` with every whole-line comment removed, Rust syntax.
///
/// Prose is not code, and every check in this file is about code. A chunk in
/// [`tests_in`] runs to the NEXT test's attribute, so it carries that test's doc
/// comment: without this, a silent test whose comment merely QUOTES
/// `exercised("kill_by_port")` satisfies the announcement check belonging to the
/// test above it, and a comment naming a primitive that is not on the roster is
/// collected by [`primitives_announced_by_the_suite`] as an UNGATED
/// announcement — a red about a name nothing can ever print.
///
/// Block comments are matched in their `rustfmt` shape: an opener, a closer, or
/// a continuation line of `*` followed by space or nothing. A continuation is
/// NOT matched on a bare leading `*`, because `*self.count += 1;` is code.
///
/// Two residuals, stated rather than implied. A TRAILING comment survives with
/// its line, so `} // exercised("kill_by_port")` still satisfies a check — the
/// code before it is real and dropping the line would lose it. And a block
/// comment whose middle lines are shaped like neither of the above survives
/// too. Both are narrower than what this closes; widen it with a real scanner
/// if either ever bites.
fn without_rust_comment_lines(src: &str) -> String {
    src.lines()
        .filter(|line| {
            let body = line.trim_start();
            !(body.starts_with("//")
                || body.starts_with("/*")
                || body.starts_with("*/")
                || body == "*"
                || body.starts_with("* "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `src` with every whole-line comment removed, YAML/shell syntax — which is
/// also awk's.
fn without_shell_comment_lines(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The workflow as CODE — the form every assertion here reads.
///
/// Two live cases, both of which were green while the thing they name was
/// broken. [`the_markers_the_workflow_greps_for_are_the_ones_the_suite_prints`]
/// asserts a marker string appears, and it appears in a comment as well as in
/// the census's own `tag = "…"` assignment: rename the tag, leave the comment,
/// test stays green. And [`declaration_count`] / [`workflow_int`] locate
/// `NAME=`, so writing `MIN_CONTRACT_TESTS=7` instead of `MIN_CONTRACT_TESTS` in
/// the comment that DOCUMENTS that knob reds the single-declaration pin with a
/// message about copy-pasted sibling steps — and a different number there would
/// make the floor read from prose.
fn read_workflow_code() -> String {
    without_shell_comment_lines(&read(WORKFLOW))
}

/// How many times `src` DECLARES `name` (`NAME=`).
///
/// Every reader of the workflow here takes the FIRST declaration, so a second
/// one leaves this contract validating a value the failing step never used —
/// silently, and in the direction that reports green.
fn declaration_count(src: &str, name: &str) -> usize {
    src.matches(&format!("{name}=")).count()
}

/// Every test-attribute occurrence in `src`, in source order, each reported at
/// the start of its whole ATTRIBUTE BLOCK rather than at the test attribute.
///
/// Only an occurrence at the start of a line counts. An attribute is written
/// above the `fn` it applies to, in column zero or indented; a mention inside a
/// doc comment or a string literal always has something else before it on the
/// line. Without that rule this file could not scan ITSELF — its own needles in
/// [`TEST_ATTRIBUTES`] and its own prose would be counted as tests, and the
/// floor it pins would be wrong in the direction that hides a deletion.
///
/// The backwards walk is what makes the suppressor check in
/// [`the_suite_holds_at_least_one_running_test_per_primitive_on_the_roster`]
/// honest: Rust accepts `#[ignore]` ABOVE `#[tokio::test]` as readily as below,
/// and an attribute left outside the chunk lands in the PREVIOUS test's chunk,
/// where nothing examines it. A file-level `#![…]` is not `#[` and is never
/// walked into.
///
/// Walking lines rather than searching per needle states the line-start rule
/// directly, arrives in order without a sort, and cannot double-count one
/// attribute by construction.
fn test_attribute_offsets(src: &str) -> Vec<usize> {
    // (offset of the first non-space character, is an attribute, is a TEST
    // attribute) for every line, in order.
    let mut lines: Vec<(usize, bool, bool)> = Vec::new();
    let mut pos = 0;
    for line in src.split_inclusive('\n') {
        let body = line.trim_start();
        lines.push((
            pos + (line.len() - body.len()),
            body.starts_with("#["),
            TEST_ATTRIBUTES.iter().any(|a| body.starts_with(a)),
        ));
        pos += line.len();
    }

    let mut offsets = Vec::new();
    for (i, (start, _, is_test)) in lines.iter().enumerate() {
        if !is_test {
            continue;
        }
        let mut block_start = *start;
        let mut j = i;
        while j > 0 && lines[j - 1].1 {
            j -= 1;
            block_start = lines[j].0;
        }
        offsets.push(block_start);
    }
    offsets.dedup();
    offsets
}

/// The tests in `src`: each one's name, and its CODE from the start of its
/// attribute block up to the next test's, so an announcement belonging to a
/// later test can never satisfy an earlier one — the same bounding `fn_source`
/// uses in `src/routes/runners.rs`, with comments stripped per
/// [`without_rust_comment_lines`].
fn tests_in(src: &str) -> Vec<(String, String)> {
    let offsets = test_attribute_offsets(src);
    offsets
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let end = offsets.get(i + 1).copied().unwrap_or(src.len());
            let code = without_rust_comment_lines(&src[*start..end]);
            // Split on `fn ` rather than on `(`: an attribute may carry its own
            // parentheses (`#[tokio::test(flavor = "multi_thread")]`), and the
            // first of those would otherwise be mistaken for the signature's.
            let name = code
                .split("fn ")
                .nth(1)
                .and_then(|after| {
                    after
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                })
                .filter(|n| !n.is_empty())
                .map_or_else(|| format!("<unnamed test #{}>", i + 1), str::to_string);
            (name, code)
        })
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
    let workflow = read_workflow_code();

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
    let workflow = read_workflow_code();

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
/// granularity — a fourth test added without an announcement is counted by the
/// floor (which only counts tests) and ignored by the census (which only counts
/// announcements). [`TEST_ATTRIBUTES`] is what decides which functions are
/// tests, and it deliberately covers the SYNCHRONOUS attribute too: the suite
/// is async today and nothing keeps it that way, so a plain test added there
/// would have been the same hole in a spelling this scan could not see.
#[test]
fn every_test_in_the_suite_announces_at_least_one_primitive() {
    let suite = read(SUITE);
    let tests = tests_in(&suite);

    let silent: Vec<_> = tests
        .iter()
        .filter(|(_, chunk)| !chunk.contains("exercised(\""))
        .map(|(name, _)| name.clone())
        .collect();

    assert!(
        !tests.is_empty(),
        "{SUITE} contains no test attribute ({TEST_ATTRIBUTES:?}) at all. The target would \
         still compile and still report a summary line, which is the vacuous green this whole \
         contract exists to refuse."
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

/// A primitive name has to survive the workflow's OWN handling of it, which is
/// narrower than what Rust will accept in a string literal.
///
/// Two places clip it, and both were written for the names that exist today:
///
/// * the census truncates an announcement at the first character outside
///   `[A-Za-z0-9_]` (`sub(/[^A-Za-z0-9_].*$/, "", name)`), so `find.pid` is
///   collected as `find`;
/// * the roster is iterated UNQUOTED (`for p in ${EXPECTED_PRIMITIVES}`), so a
///   name carrying `*`, `?` or `[` is pathname-expanded against the runner's
///   working directory before any comparison happens.
///
/// Either one reds the job — reporting the name as MISSING and, on the
/// truncated side, its own prefix as UNGATED — with a message telling an
/// operator to fix the CI runner. That is the wrong-cause red this file exists
/// to prevent, so the character class is pinned rather than assumed.
#[test]
fn every_primitive_name_survives_the_workflows_own_extraction() {
    let suite = read(SUITE);
    let workflow = read_workflow_code();

    let mut rejected: Vec<String> = primitives_announced_by_the_suite(&suite)
        .into_iter()
        .chain(roster_required_by_the_workflow(&workflow))
        .filter(|name| {
            name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .collect();
    rejected.sort();
    rejected.dedup();

    assert!(
        rejected.is_empty(),
        "these primitive names cannot survive the census in {WORKFLOW}: {rejected:?}. Its awk \
         truncates an announcement at the first character outside [A-Za-z0-9_], and its shell \
         loop iterates the roster unquoted, so such a name is reported MISSING (and its \
         surviving prefix UNGATED) while the primitive really was exercised — and the error \
         text blames the CI runner for it. Rename the primitive to [A-Za-z0-9_]+ on both \
         sides; do not widen the awk without widening the quoting with it."
    );
}
/// `MIN_TESTS` is derived from the roster (`wc -w`), so it counts PRIMITIVES
/// and gates on TESTS. The two agree only while the suite holds at least one
/// test per primitive, and only while every one of those tests actually RUNS.
///
/// Merge two tests into one that announces both and the roster still says three
/// while two tests run: CI reds with *"Only 2 tests ran (floor 3)"* — a floor
/// failure — even though all three primitives were exercised against a live
/// listener and the census passed. `#[ignore]` or a per-test `#[cfg(…)]` gets
/// there by a different road: this file counts ATTRIBUTES in the text, while
/// the floor counts tests that reported a result, so a suppressed test is a
/// green here and the same wrong-cause red there. Both are refused.
#[test]
fn the_suite_holds_at_least_one_running_test_per_primitive_on_the_roster() {
    let suite = read(SUITE);
    let workflow = read_workflow_code();

    let required = roster_required_by_the_workflow(&workflow);
    let tests = tests_in(&suite);

    assert!(
        tests.len() >= required.len(),
        "{SUITE} holds {} test(s) but {WORKFLOW}'s roster names {} primitive(s) ({required:?}). \
         MIN_TESTS is `wc -w` of that roster, so the job's floor is {} and this suite cannot \
         reach it: CI will red as a COLLAPSED TARGET (\"Do NOT lower the floor\") while every \
         primitive was exercised. Give each primitive its own test, or stop deriving the floor \
         from the roster — do not shorten the roster, which would ungate a primitive.",
        tests.len(),
        required.len(),
        required.len()
    );

    // The whole attribute block up to `fn ` is where a suppressor sits; the
    // body is free to mention either token. The block is what `tests_in`
    // delivers — it walks BACK over contiguous attribute lines — because Rust
    // takes `#[ignore]` above the test attribute as readily as below, and an
    // attribute above would otherwise be examined as part of the PREVIOUS test.
    //
    // PREFIXES, not the closed spellings: `#[ignore = "flaky"]` is the more
    // common form of the first, and `#[cfg_attr(ci, ignore)]` is the standard
    // conditional spelling of the second. Matching `#[ignore]` and `#[cfg(`
    // exactly saw neither.
    let suppressed: Vec<_> = tests
        .iter()
        .filter(|(_, code)| {
            let header = code.split("fn ").next().unwrap_or("");
            header.contains("#[ignore") || header.contains("#[cfg")
        })
        .map(|(name, _)| name.clone())
        .collect();
    assert!(
        suppressed.is_empty(),
        "these tests in {SUITE} are suppressed by an #[ignore…] or a per-test #[cfg…]: \
         {suppressed:?}. This file counts ATTRIBUTES, so it stays green; the CI floor counts \
         tests that reported a result, so it reds as a collapsed target and blames a \
         target-selection change. The suite is already #![cfg(not(target_os = \"windows\"))] at \
         FILE level and every test SKIPS rather than fails on an environmental unknown — a \
         per-test suppressor on top of that removes the primitive's only live-process \
         coverage while the census still requires its name."
    );
}

/// Each gated constant in the workflow must be declared exactly ONCE.
///
/// Both readers here — [`roster_required_by_the_workflow`] and
/// [`workflow_int`] — take the FIRST declaration in the file. A second one, a
/// sibling step written by copy-paste say, would leave this contract checking a
/// value the failing step never used, silently, and in the direction that
/// reports green.
#[test]
fn the_workflow_declares_each_gated_constant_exactly_once() {
    let workflow = read_workflow_code();

    for name in ["EXPECTED_PRIMITIVES", CONTRACT_FLOOR] {
        let declarations = declaration_count(&workflow, name);
        assert_eq!(
            declarations, 1,
            "{WORKFLOW} declares `{name}=` {declarations} time(s); this contract reads the \
             FIRST one. With more than one, the value this file checks and the value the \
             failing step actually gated on can differ and nothing would say so. With none, \
             there is no gate to check at all. Keep exactly one declaration."
        );
    }
}

/// **This file's own deletion must not be silent.**
///
/// Everything above is the only thing making the roster in the workflow and the
/// `exercised("…")` names in the suite the same set — and it rides in the
/// `Run tests` step, whose floor is 1200. Deleting this target costs a handful
/// of tests out of that, far inside the floor, so the pin could be removed and
/// the two files left free to drift apart again with nothing left to notice.
/// Fail-open on deletion is what the exercised census was written to refuse;
/// this is the same refusal applied one level up, to the refusal itself.
///
/// So the workflow censuses this target by name and gates on a committed count,
/// and this test is the other half. It asserts the census's LOAD-BEARING
/// SPELLINGS, not merely that the target is mentioned: the target name appears
/// in this file's own prose in the workflow too, so a `contains(name)` check
/// would be satisfied by the comments alone — green while the awk and both
/// guards had been deleted. And the floor it commits must equal the number of
/// tests actually here, so adding a test without raising it reds on a
/// developer's box, naming the number, rather than leaving a floor that quietly
/// stops covering the tests added after it was written.
#[test]
fn the_workflow_requires_this_pin_to_have_run() {
    let workflow = read_workflow_code();
    let me = read(SELF);
    let here = tests_in(&me).len();
    let target = contract_target();

    // The three load-bearing spellings: the awk needle that sets `found`, the
    // comparison that spends the floor, and the guard that refuses an unusable
    // reading rather than letting `[ "" -eq 0 ]` fall through to green. Each is
    // deletable on its own, and the third is the newest instance of this
    // lineage — a control nothing names can be removed in silence.
    //
    // `[ "${CONTRACT_FOUND}" -eq 0 ]` is deliberately NOT pinned: `ran` only
    // accumulates while `armed`, and `armed` is only set where `found` is, so
    // `ran > 0` implies `found = 1` and the floor already reds a target that
    // never ran. That guard improves the message; it is not a gate.
    //
    // The first spelling carries the target NAME, and that name comes from
    // [`contract_target`] — this file's own stem — not from a literal. A
    // literal here would survive a rename of this file unchanged, so the pin
    // would keep matching the workflow's stale needle and stay green while CI
    // censused a target that no longer exists.
    for spelling in [
        format!("/{target}/) {{ found = 1"),
        format!("\"${{CONTRACT_RAN}}\" -lt \"${{{CONTRACT_FLOOR}}}\""),
        "''|*[!0-9]*)".to_string(),
    ] {
        assert!(
            workflow.contains(&spelling),
            "{WORKFLOW} no longer contains `{spelling}`, so the census that requires the \
             `{target}` target to have run is gone or rewritten. Without it nothing in CI \
             notices when {SELF} is deleted or renamed: it rides in `cargo test` among >1200 \
             tests and the floor there cannot see a handful go missing. Restore the census \
             in the `Run tests` step — and if it was deliberately rewritten, rewrite this \
             pin with it rather than deleting it. If THIS FILE was renamed, the census still \
             spells the OLD name: change the workflow's `/<old-name>/` awk needle to \
             `/{target}/`."
        );
    }

    let declared = workflow_int(&workflow, CONTRACT_FLOOR);
    assert_eq!(
        declared, here,
        "{WORKFLOW} commits {CONTRACT_FLOOR}={declared} but {SELF} holds {here} test(s). That \
         floor is what proves this pin RAN rather than merely compiled, so a floor below the \
         real count stops covering exactly the tests added after it was last touched. Set \
         {CONTRACT_FLOOR}={here}."
    );
}

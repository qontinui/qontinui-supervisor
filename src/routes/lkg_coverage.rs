//! GET /lkg/coverage — Tell agents whether the current LKG covers a set of files.
//!
//! Collapses the manual procedure documented in this crate's CLAUDE.md
//! ("Last-known-good (LKG) fallback for agents") into a single HTTP call.
//! Per the comparison rule, an agent's changes are considered "covered" by
//! the LKG when `max(file_mtime) <= lkg.built_at` — i.e. the LKG was built
//! AFTER every file the agent has touched, so those changes are already
//! compiled into the LKG binary and the agent can spawn with
//! `{rebuild: false, use_lkg: true}` without running stale code.
//!
//! ## Field-name conventions (READ THIS — this used to be ambiguous)
//!
//! `covered: true` means **the file's content is in the LKG binary** (file
//! is at least as old as the LKG, so the LKG was built AFTER the file's
//! current contents were written, so a `use_lkg: true` spawn runs your
//! changes). `covered: false` means the file is newer than the LKG — the
//! LKG predates your edits and would silently run stale code.
//!
//! `file_newer_than_lkg_secs` is signed and self-documenting:
//!   * **positive** ⇒ file mtime is N seconds AFTER the LKG built_at; the
//!     edit is NOT in the LKG (`covered: false`). N is "how stale the LKG
//!     is relative to this file."
//!   * **zero** ⇒ file mtime equals LKG built_at exactly (still covered;
//!     the rule is `<=`).
//!   * **negative** ⇒ file mtime is |N| seconds BEFORE the LKG built_at;
//!     the LKG was built after this file's last edit, so the file is in the
//!     LKG (`covered: true`). |N| is "how old the file is relative to the
//!     LKG."
//!
//! In short: `file_newer_than_lkg_secs > 0` ⇔ `covered: false`. The two
//! fields are redundant by design — callers gate decisions on `covered`,
//! and the signed delta is for human inspection.
//!
//! ## MTIME vs COMMITS (read this before trusting `covered`)
//!
//! Everything named `covered` / `*_mtime` / `*_secs` below is **mtime-based**.
//! An mtime cannot tell you which *commits* a binary contains: `git checkout`
//! rewrites mtimes wholesale, so a binary built from a branch parked 72 commits
//! behind `origin/main` looks perfectly fresh while missing all 72. That is
//! exactly how a landed fix came to read as a regression.
//!
//! The `commit_provenance` block answers the commit question exactly:
//! `lkg_sha`, `lkg_source`, `behind_origin_main`, `is_ancestor_of_origin_main`.
//! To settle "is MY fix in this binary?", pass `?contains=<sha>` and read
//! `commit_provenance.contains` — it is `git merge-base --is-ancestor <sha>
//! <lkg_sha>`, computed server-side. `null` means *not computable*, and must
//! never be read as `false`.
//!
//! ## Query
//!
//! `path` is repeatable: `?path=foo.rs&path=bar.rs` checks both files in one
//! request. `contains=<sha>` is single-valued (last wins) and adds the
//! commit-containment answer described above. Single-file callers (the common case) use it once. Paths may be
//! absolute, or relative to the supervisor's `--project-dir`
//! (`qontinui-runner/src-tauri/`). Relative paths are resolved against
//! `state.config.project_dir`.
//!
//! ## Response shape
//!
//! ```text
//! {
//!   success: true,
//!   data: {
//!     description: "covered=true means the file's content is in the LKG binary. \
//!                   file_newer_than_lkg_secs > 0 ⇔ file mtime > LKG built_at ⇔ \
//!                   covered: false (LKG is stale relative to this file).",
//!     lkg_built_at: <RFC3339 string | null>,
//!     lkg_age_secs: <i64 | null>,
//!         // = (now - lkg_built_at).num_seconds(). `null` when there is no
//!         // LKG yet (mirrors `lkg_built_at: null`). Agents use this to
//!         // detect a stale-but-covering LKG without parsing the RFC3339
//!         // string themselves.
//!     lkg_stale: <bool>,
//!         // true iff `lkg_age_secs.is_some() && lkg_age_secs > 86400`
//!         // (LKG older than 24h). `false` when no LKG OR when the LKG is
//!         // fresh enough to be trusted. Independent of per-file `covered`:
//!         // the LKG may be stale-as-a-fallback (lkg_stale=true) yet still
//!         // contain a specific file's edits (covered=true).
//!     files: [
//!       {
//!         path: <echoed back, normalized>,
//!         exists: <bool>,
//!         file_mtime: <RFC3339 string | null>,
//!         covered: <bool>,
//!         file_newer_than_lkg_secs: <i64 | null>,
//!             // = (file_mtime - lkg_built_at).num_seconds()
//!             // positive  => file is newer than LKG (NOT covered, rebuild needed)
//!             // zero      => file mtime equals LKG built_at (covered, by `<=` rule)
//!             // negative  => file is older than LKG (covered, in the binary)
//!         reason: <string>,
//!             // Human-readable explanation of the `covered` decision.
//!             // One of: "covered_file_at_or_before_lkg",
//!             //         "covered_but_lkg_stale",
//!             //         "not_covered_file_newer_than_lkg",
//!             //         "no_lkg_yet",
//!             //         "file_not_found".
//!             // `covered_but_lkg_stale` fires when the file's content IS in
//!             // the LKG binary (covered=true) but the LKG itself is older
//!             // than 24h. Distinguishes "covered, ship it" from "covered
//!             // but you should rebuild anyway."
//!       },
//!       ...
//!     ],
//!     all_covered: <bool>,           // true iff every requested file is covered
//!   },
//!   timestamp: <millis>,
//! }
//! ```
//!
//! ## Edge cases
//!
//! - `path` missing or invalid: returned as `{exists: false, covered: false,
//!   file_mtime: null, file_newer_than_lkg_secs: null, reason:
//!   "file_not_found"}` so a script passing a list of files doesn't bail on
//!   the first bad entry. This includes path-traversal attempts (e.g.
//!   `../../etc/passwd`) — those resolve outside `project_dir` and are
//!   reported as `exists: false` rather than 400, on the same "graceful
//!   per-file failure" principle.
//! - `state.build_pool.last_known_good == None` (no successful build yet):
//!   `lkg_built_at: null`, every file `covered: false,
//!   file_newer_than_lkg_secs: null, reason: "no_lkg_yet"`,
//!   `all_covered: false`.
//! - File doesn't exist on disk: `exists: false, file_mtime: null,
//!   covered: false, file_newer_than_lkg_secs: null, reason:
//!   "file_not_found"`.

use axum::extract::{RawQuery, State};
use axum::response::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use std::path::{Component, Path, PathBuf};

use crate::state::SharedState;

/// One file's coverage decision. The serialized shape is fixed by the API
/// contract — every field is always present, with `null` for "not
/// applicable" rather than absent, so JSON consumers don't have to branch on
/// presence vs falsy.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileCoverage {
    /// Path the caller submitted, echoed back (after normalization).
    pub path: String,
    pub exists: bool,
    /// File's last-modified time as RFC3339 UTC. `None` when `exists: false`.
    #[serde(serialize_with = "ser_opt_dt")]
    pub file_mtime: Option<DateTime<Utc>>,
    /// True iff the LKG is at least as new as this file (i.e. the file's
    /// content is in the LKG binary).
    pub covered: bool,
    /// `(file_mtime - lkg_built_at).num_seconds()`. Self-documenting sign
    /// convention — see the module doc-comment for the full table:
    ///
    /// * **positive** ⇒ file is newer than LKG ⇒ NOT covered (rebuild needed).
    /// * **zero** ⇒ file mtime equals LKG built_at exactly ⇒ covered.
    /// * **negative** ⇒ file is older than LKG ⇒ covered (in the binary).
    ///
    /// `None` when either timestamp is missing (no LKG, or file not on disk).
    pub file_newer_than_lkg_secs: Option<i64>,
    /// Human-readable explanation of the `covered` decision. Stable enum
    /// values that callers can branch on if they want machine-readable
    /// reasons in addition to the boolean. See module-level doc for the
    /// fixed set of values.
    pub reason: &'static str,
}

/// Top-level response payload for `GET /lkg/coverage`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoverageResponse {
    /// One-line semantics summary. Embedded in every response so a future
    /// caller doesn't have to read the source to understand what
    /// `covered: false` and `file_newer_than_lkg_secs > 0` mean.
    pub description: &'static str,
    /// LKG `built_at` as RFC3339 UTC. `None` when no successful build has
    /// produced an LKG yet on this checkout.
    #[serde(serialize_with = "ser_opt_dt")]
    pub lkg_built_at: Option<DateTime<Utc>>,
    /// `(now - lkg_built_at).num_seconds()`. `None` when there is no LKG
    /// yet (i.e. when `lkg_built_at` is `None`). Exposed so agents can
    /// detect a stale-but-still-covering LKG without re-parsing the RFC3339
    /// `built_at` string. See `lkg_stale` for the threshold-applied flag.
    pub lkg_age_secs: Option<i64>,
    /// `true` iff `lkg_age_secs.is_some_and(|s| s > LKG_STALE_THRESHOLD_SECS)`.
    /// `false` when there is no LKG (no signal yet) or the LKG is fresh.
    /// Independent of per-file `covered`: an LKG can be stale-as-a-fallback
    /// while still containing a specific file's content. See module-level
    /// docs and `reason::COVERED_BUT_STALE` for the per-file companion signal.
    pub lkg_stale: bool,
    pub files: Vec<FileCoverage>,
    /// True iff every requested file is `covered: true`. `false` when there
    /// are no files OR when LKG is absent OR when any file is not covered.
    pub all_covered: bool,
    /// COMMIT-based provenance of the LKG binary. Every field above this one
    /// is mtime-based and therefore can only ever *guess*; this block answers
    /// the question that actually matters — "which commit is in this binary?"
    /// — exactly. `None` when there is no LKG yet.
    pub commit_provenance: Option<CommitProvenance>,
}

/// Commit-level provenance of the LKG binary, plus the answer to an optional
/// `?contains=<sha>` question.
///
/// Why this exists: `covered` / `file_newer_than_lkg_secs` compare **mtimes**.
/// An mtime says nothing about which commits a binary contains — a `git
/// checkout` rewrites mtimes wholesale, and a build from a branch parked 72
/// commits behind `origin/main` has a perfectly fresh mtime while missing every
/// one of those commits. That is precisely how a landed fix came to read as a
/// regression. These fields are derived from git history instead, so the answer
/// is exact rather than inferred.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct CommitProvenance {
    /// Full 40-char git SHA the LKG binary was built from. `None` when the
    /// build-time git probe failed or the record predates sha recording.
    pub lkg_sha: Option<String>,
    /// How that tree was classified — `live_tree` / `origin_main` / `override`.
    /// `None` when unrecorded.
    pub lkg_source: Option<String>,
    /// Commits `origin/main` is ahead of `lkg_sha`. `None` when not computable.
    pub behind_origin_main: Option<u32>,
    /// `false` ⇒ the LKG sha is NOT on `origin/main`'s history (built from a
    /// feature branch — the dangerous case). `None` when not computable.
    pub is_ancestor_of_origin_main: Option<bool>,
    /// Echo of the `?contains=<sha>` query, when supplied.
    pub contains_query: Option<String>,
    /// `Some(true)` ⇔ `git merge-base --is-ancestor <contains_query> <lkg_sha>`
    /// succeeds, i.e. **that commit IS in this binary**. `Some(false)` ⇔ it is
    /// provably absent. `None` ⇔ not computable (no `contains` asked, no LKG
    /// sha, or either sha is unknown to the local object database) — `None`
    /// must NEVER be read as `false`.
    pub contains: Option<bool>,
}

/// Number of seconds an LKG can age before `lkg_stale` flips to `true`.
/// Chosen at 24h (`86_400`s): a binary older than a day in an active
/// codebase is likely to predate at least one schema/protocol change that
/// silently breaks `use_lkg: true` spawns, even when the agent's own files
/// are covered.
pub const LKG_STALE_THRESHOLD_SECS: i64 = 86_400;

/// One-line semantics summary embedded in every `/lkg/coverage` response.
/// Kept as a single source of truth so the Rust doc-comments and the
/// runtime payload can't drift.
pub const COVERAGE_DESCRIPTION: &str = "covered=true means the file's content is in the LKG binary (file mtime <= lkg_built_at). file_newer_than_lkg_secs > 0 means the file was edited AFTER the LKG was built (covered=false, rebuild needed). Negative means file is older than LKG (covered=true). NOTE: these are MTIME-based and cannot tell you which COMMITS a binary contains — a checkout rewrites mtimes wholesale. For that, read `commit_provenance` (lkg_sha, behind_origin_main, is_ancestor_of_origin_main) or ask directly with ?contains=<sha>, which answers `git merge-base --is-ancestor <sha> <lkg_sha>`.";

/// Stable string values for `FileCoverage.reason`. Exposed as constants so
/// tests and downstream callers can match on them without hard-coding
/// string literals.
pub mod reason {
    /// LKG was built at or after the file's mtime — file is in the binary.
    pub const COVERED: &str = "covered_file_at_or_before_lkg";
    /// LKG contains the file's content (covered=true) BUT the LKG itself
    /// is older than `LKG_STALE_THRESHOLD_SECS`. Distinguishes "covered, ship
    /// it" from "covered but you should rebuild anyway because the LKG is
    /// likely to be missing other ecosystem changes (schema, protocol, etc.)."
    pub const COVERED_BUT_STALE: &str = "covered_but_lkg_stale";
    /// File was edited after the LKG was built — rebuild required.
    pub const NOT_COVERED: &str = "not_covered_file_newer_than_lkg";
    /// No successful LKG build has been recorded yet.
    pub const NO_LKG: &str = "no_lkg_yet";
    /// File doesn't exist on disk (or path traversal escape).
    pub const FILE_NOT_FOUND: &str = "file_not_found";
}

/// Custom serializer for `Option<DateTime<Utc>>` that emits `null` for `None`
/// and an RFC3339 string for `Some(_)`. `chrono`'s default `Serialize` impl
/// for `DateTime` already emits RFC3339, but it doesn't help us with the
/// `Option` wrapper when we want explicit `null` rather than `Option`'s
/// default behavior — which is also `null`, but only because we don't use
/// `skip_serializing_if`. This wrapper just makes the contract explicit and
/// gives us one place to adjust the format if the API needs to.
fn ser_opt_dt<S: serde::Serializer>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(dt) => s.serialize_str(&dt.to_rfc3339()),
        None => s.serialize_none(),
    }
}

/// Parse a raw query string into the list of `path=` values, in submission
/// order. We don't use `axum::extract::Query<HashMap<...>>` because that
/// collapses repeated keys; the spec is "`?path=a&path=b` checks both".
///
/// Empty / missing query string → empty list (caller will get `all_covered:
/// false` because there are no files to cover, which matches the rule "true
/// iff every requested file is covered").
pub fn parse_path_params(raw: Option<&str>) -> Vec<String> {
    let raw = match raw {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        if k != "path" {
            continue;
        }
        // url-decode the value. We use `percent_encoding` semantics by hand
        // to avoid pulling a new crate; only `+` (space) and `%XX` need to
        // be handled and file paths in practice contain neither in the
        // values we care about, but doing it right keeps the door open for
        // paths with spaces and unicode.
        out.push(percent_decode(v));
    }
    out
}

/// Extract the LAST value of a single-valued query parameter, percent-decoded.
///
/// Shares [`percent_decode`] and the same lenient parsing posture as
/// [`parse_path_params`] (malformed pairs are skipped, never fatal). Last-wins
/// so a duplicated `?contains=` is deterministic rather than arbitrary. Blank
/// values resolve to `None` — `?contains=` with nothing after it is "no
/// question asked", not "ask about the empty sha".
pub fn parse_single_param(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    let mut found = None;
    for pair in raw.split('&') {
        // A pair with no `=` is skipped, not fatal — bailing here would make a
        // trailing `&` silently drop a valid earlier match.
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == key {
            let decoded = percent_decode(v);
            if !decoded.trim().is_empty() {
                found = Some(decoded.trim().to_string());
            }
        }
    }
    found
}

/// Minimal percent-decoder: handles `+` → space and `%XX` → byte. Anything
/// malformed is preserved verbatim (so `path=%QQ` becomes `%QQ` rather than
/// erroring out). Returns a `String`; bytes that don't form valid UTF-8
/// after decoding are replaced with U+FFFD via `from_utf8_lossy` — fine
/// because file paths on Windows are essentially always UTF-8 in practice
/// and the response field is `String` anyway.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Resolve a user-submitted path to an absolute `PathBuf` constrained to live
/// under `project_dir`. Returns `None` when the path escapes `project_dir`
/// after normalization (path-traversal guard) — caller should treat that
/// case as `exists: false`.
///
/// Strategy:
/// 1. If `submitted` is absolute, use it as-is.
/// 2. If relative, join onto `project_dir`.
/// 3. Lexically normalize (collapse `.` and `..`) — we cannot rely on
///    `canonicalize()` because the file may not exist yet (in which case
///    canonicalize errors out), and we still need to give a useful answer
///    for nonexistent files.
/// 4. Containment check: when `submitted` was relative OR begins with
///    `project_dir` after normalization, the resolved path is "inside"
///    project_dir and we accept it. When `submitted` was absolute and lives
///    elsewhere on disk, we accept it too — the spec says "absolute paths
///    AND paths relative to project_dir". The traversal guard targets the
///    *relative* case: a relative path that normalizes outside `project_dir`
///    (because it walks `..` past the root) is rejected.
pub fn resolve_path(submitted: &str, project_dir: &Path) -> Option<PathBuf> {
    let submitted_path = Path::new(submitted);

    if submitted_path.is_absolute() {
        // Absolute paths are accepted as-is; the spec doesn't constrain them
        // to project_dir. Lexically normalize so `D:\qontinui-root\..\foo`
        // doesn't slip past anything downstream that does substring checks.
        return Some(lexical_normalize(submitted_path));
    }

    // Relative path: join onto project_dir, then normalize, then verify the
    // result still starts with project_dir (the traversal guard).
    let joined = project_dir.join(submitted_path);
    let normalized = lexical_normalize(&joined);

    let project_normalized = lexical_normalize(project_dir);
    if normalized.starts_with(&project_normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// Lexically normalize a path by collapsing `.` and `..` components without
/// touching the filesystem. Symlinks are NOT resolved — that's a feature for
/// us, not a bug: the spec wants the comparison rooted in the path the user
/// submitted, not whatever the symlink expands to.
///
/// On Windows, this preserves drive letters and UNC prefixes (those come
/// through as `Component::Prefix` and we pass them straight back). When
/// `..` walks back past the root, the root component stays put — so
/// `D:\foo\..\..\..\bar` normalizes to `D:\bar`, not `D:\..\..\bar`. This
/// matches `path::Path::canonicalize` behavior and is what callers expect.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop only if the last component is a "real" directory (not
                // a root or prefix). This prevents `..` from escaping the
                // root.
                let popped = out.pop();
                if !popped {
                    // Empty path; `..` from nothing stays nothing. (Should
                    // be unreachable in our use because we always start
                    // from an absolute project_dir.)
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// Read a file's mtime as `DateTime<Utc>`. Returns `None` for any error
/// (file missing, permission denied, mtime not supported by the FS).
pub fn read_file_mtime(p: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(p).ok()?;
    let modified = meta.modified().ok()?;
    Some(modified.into())
}

/// Pure decision function: given the LKG timestamp and a file's mtime,
/// produce a `(covered, file_newer_than_lkg_secs, reason)` triple.
///
/// Sign convention is self-documenting from the field name —
/// `file_newer_than_lkg_secs = (file_mtime - lkg_built_at).num_seconds()`.
/// Positive ⇒ file is newer than LKG ⇒ NOT covered. See the module
/// doc-comment for the full table.
///
/// Extracted for unit testing — the file-system parts of the handler are
/// unit-tested separately via `tempfile`-backed real files, but the
/// decision rule itself doesn't need I/O to verify.
pub fn decide_coverage(
    lkg_at: Option<DateTime<Utc>>,
    file_mtime: Option<DateTime<Utc>>,
) -> (bool, Option<i64>, &'static str) {
    match (lkg_at, file_mtime) {
        (Some(lkg), Some(m)) => {
            // file_newer_than_lkg_secs = (file - lkg).num_seconds():
            //   > 0 ⇒ file was edited after LKG built ⇒ NOT covered.
            //   = 0 ⇒ exact equality ⇒ covered (rule is `mtime <= lkg`).
            //   < 0 ⇒ file predates LKG ⇒ covered.
            let file_newer = (m - lkg).num_seconds();
            if file_newer <= 0 {
                (true, Some(file_newer), reason::COVERED)
            } else {
                (false, Some(file_newer), reason::NOT_COVERED)
            }
        }
        (None, _) => (false, None, reason::NO_LKG),
        (Some(_), None) => (false, None, reason::FILE_NOT_FOUND),
    }
}

/// Coverage decision for one file given a normalized absolute path and the
/// LKG built_at. Pure-ish (filesystem read for mtime). Caller is responsible
/// for path resolution; this function just stats and decides.
///
/// `lkg_stale` is the top-level "LKG is older than the staleness threshold"
/// flag computed once per response. When the file is covered AND the LKG is
/// stale, the per-file `reason` is upgraded from `COVERED` to
/// `COVERED_BUT_STALE` so agents can distinguish "covered, ship it" from
/// "covered, but rebuild anyway because the rest of the LKG is stale."
fn coverage_for_resolved(
    echo_path: &str,
    resolved: Option<&Path>,
    lkg_at: Option<DateTime<Utc>>,
    lkg_stale: bool,
) -> FileCoverage {
    let path = resolved;
    let mtime = path.and_then(read_file_mtime);
    let exists = mtime.is_some();
    let (covered, file_newer_than_lkg_secs, mut reason) = decide_coverage(lkg_at, mtime);
    // Promote COVERED → COVERED_BUT_STALE when the LKG itself is past the
    // threshold. Only applies to the `covered=true` case; NOT_COVERED /
    // NO_LKG / FILE_NOT_FOUND already carry the stronger signal.
    if covered && lkg_stale && reason == self::reason::COVERED {
        reason = self::reason::COVERED_BUT_STALE;
    }
    FileCoverage {
        path: echo_path.to_string(),
        exists,
        file_mtime: mtime,
        covered,
        file_newer_than_lkg_secs,
        reason,
    }
}

/// Compute the LKG age + stale flag for a response.
///
/// * `lkg_age_secs` is `Some((now - built_at).num_seconds())` when `lkg_at`
///   is present, else `None`.
/// * `lkg_stale` is `true` iff `lkg_age_secs.is_some_and(|s| s > LKG_STALE_THRESHOLD_SECS)`.
///
/// `now` is taken as a parameter so the rule is unit-testable without
/// mocking the system clock — the HTTP handler passes `Utc::now()`.
pub fn compute_lkg_age(lkg_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> (Option<i64>, bool) {
    match lkg_at {
        Some(built_at) => {
            let age = (now - built_at).num_seconds();
            (Some(age), age > LKG_STALE_THRESHOLD_SECS)
        }
        None => (None, false),
    }
}

/// Compute the full coverage response from already-resolved data. Pulled
/// out as a pure function so the bulk of the logic can be unit-tested
/// without a router or `SharedState`.
///
/// `now` is taken as a parameter for testability — the HTTP handler passes
/// `Utc::now()`. It's used solely to compute `lkg_age_secs` / `lkg_stale`;
/// per-file `covered` decisions don't consult the wall clock.
pub fn build_coverage_response(
    requested: &[String],
    project_dir: &Path,
    lkg_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> CoverageResponse {
    let (lkg_age_secs, lkg_stale) = compute_lkg_age(lkg_at, now);
    let mut files = Vec::with_capacity(requested.len());
    for raw in requested {
        let resolved = resolve_path(raw, project_dir);
        let resolved_ref = resolved.as_deref();
        files.push(coverage_for_resolved(raw, resolved_ref, lkg_at, lkg_stale));
    }
    let all_covered = !files.is_empty() && files.iter().all(|f| f.covered);
    CoverageResponse {
        description: COVERAGE_DESCRIPTION,
        lkg_built_at: lkg_at,
        lkg_age_secs,
        lkg_stale,
        files,
        all_covered,
        commit_provenance: None,
    }
}

/// Axum handler for `GET /lkg/coverage`. Wraps the pure logic above and
/// emits the supervisor-standard `{success, data, timestamp}` envelope used
/// by the supervisor-bridge endpoints.
pub async fn lkg_coverage(
    State(state): State<SharedState>,
    RawQuery(raw): RawQuery,
) -> Json<serde_json::Value> {
    let requested = parse_path_params(raw.as_deref());
    let lkg = state.build_pool.last_known_good.read().await.clone();
    let lkg_at = lkg.as_ref().map(|info| info.built_at);
    let project_dir = state.config.project_dir.clone();
    let mut response = build_coverage_response(&requested, &project_dir, lkg_at, Utc::now());

    // Commit-based provenance — the exact answer the mtime fields can only
    // approximate. Best-effort throughout: any probe failure degrades to
    // `None` ("not computable"), never to a false negative.
    if let Some(info) = lkg.as_ref() {
        let contains_query = parse_single_param(raw.as_deref(), "contains");
        // `project_dir` is `<repo>/src-tauri`; git resolves from the repo root
        // for parity with the other provenance call sites.
        let repo_root = project_dir.parent().unwrap_or(&project_dir).to_path_buf();
        let (behind, is_ancestor) = match info.sha.as_deref() {
            Some(sha) => {
                let drift = crate::git_provenance::origin_main_drift(&repo_root, sha).await;
                if drift.origin_main_sha.is_empty() {
                    (None, None)
                } else {
                    (Some(drift.behind_count), Some(drift.is_ancestor))
                }
            }
            None => (None, None),
        };
        let contains = match (contains_query.as_deref(), info.sha.as_deref()) {
            (Some(q), Some(lkg_sha)) => {
                crate::git_provenance::contains_commit(&repo_root, q, lkg_sha).await
            }
            _ => None,
        };
        response.commit_provenance = Some(CommitProvenance {
            lkg_sha: info.sha.clone(),
            lkg_source: serde_json::to_value(info.source)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string)),
            behind_origin_main: behind,
            is_ancestor_of_origin_main: is_ancestor,
            contains_query,
            contains,
        });
    }

    let data = serde_json::to_value(&response).unwrap_or_else(|_| json!({}));
    Json(json!({
        "success": true,
        "data": data,
        "timestamp": chrono::Utc::now().timestamp_millis(),
    }))
}

/// Result of the spawn-time LKG staleness scan.
///
/// `stale` is true iff the newest runner Rust source under `<root>/src`
/// has an mtime strictly later than the LKG `built_at` — meaning a
/// `use_lkg: true` spawn would run a binary that predates current source.
#[derive(Debug, Clone)]
pub struct LkgStaleScan {
    pub stale: bool,
    pub lkg_built_at: DateTime<Utc>,
    pub newest_src_mtime: DateTime<Utc>,
    /// Path of the newest source file, relative to `root` (forward slashes).
    pub newest_src_file: String,
    pub lag_secs: i64,
}

/// Pure decision: given the LKG `built_at` and the newest `(relpath, mtime)`
/// among the runner sources, decide whether the LKG is stale.
///
/// Extracted from the directory walk so the rule is unit-testable without
/// touching the filesystem. `stale` ⇔ `newest_src_mtime > lkg_built_at`.
pub fn decide_lkg_stale(
    lkg_built_at: DateTime<Utc>,
    newest: Option<(String, DateTime<Utc>)>,
) -> Option<LkgStaleScan> {
    let (relpath, mtime) = newest?;
    let lag_secs = (mtime - lkg_built_at).num_seconds();
    Some(LkgStaleScan {
        stale: lag_secs > 0,
        lkg_built_at,
        newest_src_mtime: mtime,
        newest_src_file: relpath,
        lag_secs,
    })
}

/// Find the newest-mtime `*.rs` file under `<root>/src`, returning its
/// path relative to `root` (forward-slashed) and its mtime.
///
/// Bounded + cheap: a single iterative directory walk that prunes
/// `target*`, `node_modules`, and `.git`. No symlink following. Returns
/// `None` if `<root>/src` doesn't exist or contains no readable `.rs`.
pub fn newest_rs_under_src(root: &Path) -> Option<(String, DateTime<Utc>)> {
    let src_root = root.join("src");
    if !src_root.is_dir() {
        return None;
    }
    let mut stack: Vec<PathBuf> = vec![src_root];
    let mut best: Option<(String, DateTime<Utc>)> = None;
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ftype = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ftype.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == ".git" || name == "node_modules" || name.starts_with("target") {
                    continue;
                }
                stack.push(path);
            } else if ftype.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Some(mtime) = read_file_mtime(&path) {
                    let is_newer = best.as_ref().map(|(_, m)| mtime > *m).unwrap_or(true);
                    if is_newer {
                        let rel = path
                            .strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        best = Some((rel, mtime));
                    }
                }
            }
        }
    }
    best
}

/// Top-level helper for the spawn-test handler: scan `<root>/src` for the
/// newest `*.rs` and decide whether the LKG (`lkg_built_at`) is stale
/// relative to it. `None` ⇒ no sources found / nothing to compare.
pub fn scan_lkg_staleness(root: &Path, lkg_built_at: DateTime<Utc>) -> Option<LkgStaleScan> {
    decide_lkg_stale(lkg_built_at, newest_rs_under_src(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use std::time::{Duration, SystemTime};

    /// Set a file's mtime to a known wall-clock value. We use
    /// `filetime` semantics via the `set_modified` syscall on each
    /// platform; on Windows and Unix `std` exposes this through
    /// `File::set_modified` (stable since 1.75). All tests run on
    /// Windows in this repo so this is straightforward.
    /// `?contains=<sha>` parsing — the entry point to the COMMIT-based answer.
    /// Must coexist with repeated `path` params, tolerate malformed pairs, and
    /// treat a blank value as "no question asked" rather than a query for the
    /// empty sha.
    #[test]
    fn parse_single_param_extracts_contains_alongside_paths() {
        let p = |q: &str| parse_single_param(Some(q), "contains");

        assert_eq!(p("contains=abc123"), Some("abc123".to_string()));
        assert_eq!(
            p("path=a.rs&contains=abc123&path=b.rs"),
            Some("abc123".to_string()),
            "must survive repeated `path` params on either side"
        );
        // Last wins, so a duplicated param is deterministic.
        assert_eq!(p("contains=aaa&contains=bbb"), Some("bbb".to_string()));
        // Percent/plus decoding shares the `path` decoder.
        assert_eq!(p("contains=a%62c"), Some("abc".to_string()));

        // Absent / blank / whitespace-only ⇒ no question asked.
        assert_eq!(p("path=a.rs"), None);
        assert_eq!(p("contains="), None);
        assert_eq!(p("contains=%20%20"), None);
        assert_eq!(parse_single_param(None, "contains"), None);

        // A pair with no `=` must be SKIPPED, not abort the scan — otherwise a
        // stray `&flag&` would silently drop a valid `contains` after it.
        assert_eq!(
            p("flag&contains=abc123"),
            Some("abc123".to_string()),
            "a valueless pair must not swallow later params"
        );
        assert_eq!(p("contains=abc123&trailing"), Some("abc123".to_string()));

        // Key matching is exact — no prefix bleed.
        assert_eq!(p("contains_sha=abc"), None);
    }

    fn touch_file_mtime(path: &Path, when: SystemTime) {
        let f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("open file");
        f.set_modified(when).expect("set_modified");
    }

    fn rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("parse rfc3339")
            .with_timezone(&Utc)
    }

    // --- parse_path_params ---

    #[test]
    fn test_parse_path_params_none() {
        assert!(parse_path_params(None).is_empty());
    }

    #[test]
    fn test_parse_path_params_empty_string() {
        assert!(parse_path_params(Some("")).is_empty());
    }

    #[test]
    fn test_parse_path_params_single() {
        assert_eq!(
            parse_path_params(Some("path=foo.rs")),
            vec!["foo.rs".to_string()]
        );
    }

    #[test]
    fn test_parse_path_params_multiple() {
        assert_eq!(
            parse_path_params(Some("path=foo.rs&path=bar.rs")),
            vec!["foo.rs".to_string(), "bar.rs".to_string()]
        );
    }

    #[test]
    fn test_parse_path_params_ignores_other_keys() {
        assert_eq!(
            parse_path_params(Some("foo=1&path=a.rs&bar=2&path=b.rs")),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
    }

    #[test]
    fn test_parse_path_params_url_decode() {
        assert_eq!(
            parse_path_params(Some("path=src%2Fmain.rs")),
            vec!["src/main.rs".to_string()]
        );
        assert_eq!(
            parse_path_params(Some("path=hello+world.txt")),
            vec!["hello world.txt".to_string()]
        );
    }

    // --- resolve_path ---

    #[test]
    fn test_resolve_path_absolute_passes_through() {
        let project = Path::new("D:/tmp/project");
        // An absolute path outside project_dir is accepted (spec: "absolute
        // paths AND paths relative to project_dir"). Use a platform-appropriate
        // absolute path: `D:/...` is absolute on Windows but relative on Linux,
        // so on Linux pick `/...` instead. Without this, Linux CI joins the
        // (relative-on-Linux) input under project_dir and the is_absolute
        // assertion fires.
        let abs = if cfg!(windows) {
            "D:/somewhere/else/file.rs"
        } else {
            "/somewhere/else/file.rs"
        };
        let got = resolve_path(abs, project).expect("absolute path resolves");
        assert!(got.is_absolute(), "result must be absolute, got {:?}", got);
    }

    #[test]
    fn test_resolve_path_relative_resolves_against_project_dir() {
        let project = Path::new("D:/tmp/project");
        let got = resolve_path("src/main.rs", project).expect("relative path resolves");
        assert!(got.ends_with("src/main.rs") || got.ends_with("src\\main.rs"));
        // Must live under project_dir (the whole point).
        let project_norm = lexical_normalize(project);
        assert!(got.starts_with(&project_norm));
    }

    #[test]
    fn test_resolve_path_relative_traversal_rejected() {
        let project = Path::new("D:/tmp/project");
        // `../../etc/passwd` walks above project_dir, must return None.
        assert!(resolve_path("../../etc/passwd", project).is_none());
        // `../sibling/file.rs` walks one level up — also outside project_dir.
        assert!(resolve_path("../sibling/file.rs", project).is_none());
    }

    #[test]
    fn test_resolve_path_relative_with_inner_dotdot_kept() {
        let project = Path::new("D:/tmp/project");
        // `src/../tests/foo.rs` lexically normalizes to `<project>/tests/foo.rs`,
        // which is still under project_dir, so accept.
        let got = resolve_path("src/../tests/foo.rs", project).expect("inner .. ok");
        assert!(got.ends_with("tests/foo.rs") || got.ends_with("tests\\foo.rs"));
    }

    // --- decide_coverage ---
    //
    // Sign convention reminder (see module doc):
    //   file_newer_than_lkg_secs = (file_mtime - lkg_built_at).num_seconds()
    //   > 0 ⇒ file edited after LKG built ⇒ NOT covered (rebuild needed).
    //   = 0 ⇒ exactly equal ⇒ covered (rule is `mtime <= lkg`).
    //   < 0 ⇒ file older than LKG ⇒ covered (in the binary).

    #[test]
    fn test_decide_coverage_no_lkg() {
        let (covered, delta, reason) = decide_coverage(None, Some(Utc::now()));
        assert!(!covered);
        assert_eq!(delta, None);
        assert_eq!(reason, reason::NO_LKG);
    }

    #[test]
    fn test_decide_coverage_no_file() {
        let (covered, delta, reason) = decide_coverage(Some(Utc::now()), None);
        assert!(!covered);
        assert_eq!(delta, None);
        assert_eq!(reason, reason::FILE_NOT_FOUND);
    }

    #[test]
    fn test_decide_coverage_file_older_than_lkg() {
        // file at T=100, LKG at T=200 → covered=true,
        // file_newer_than_lkg_secs = 100 - 200 = -100 (file older).
        let lkg = Utc.timestamp_opt(200, 0).unwrap();
        let file = Utc.timestamp_opt(100, 0).unwrap();
        let (covered, delta, reason) = decide_coverage(Some(lkg), Some(file));
        assert!(covered);
        assert_eq!(delta, Some(-100));
        assert_eq!(reason, super::reason::COVERED);
    }

    #[test]
    fn test_decide_coverage_file_equal_to_lkg() {
        // Spec: "max(mtime) <= lkg.built_at" — equal counts as covered,
        // file_newer_than_lkg_secs = 0.
        let t = Utc.timestamp_opt(150, 0).unwrap();
        let (covered, delta, reason) = decide_coverage(Some(t), Some(t));
        assert!(covered);
        assert_eq!(delta, Some(0));
        assert_eq!(reason, super::reason::COVERED);
    }

    #[test]
    fn test_decide_coverage_file_newer_than_lkg() {
        // file at T=300, LKG at T=200 → covered=false,
        // file_newer_than_lkg_secs = 300 - 200 = +100 (file newer).
        let lkg = Utc.timestamp_opt(200, 0).unwrap();
        let file = Utc.timestamp_opt(300, 0).unwrap();
        let (covered, delta, reason) = decide_coverage(Some(lkg), Some(file));
        assert!(!covered);
        assert_eq!(delta, Some(100));
        assert_eq!(reason, super::reason::NOT_COVERED);
    }

    /// Pin down the new self-documenting semantics. The reverse-implication
    /// `file_newer_than_lkg_secs > 0  ⇔  covered: false` is the property
    /// callers rely on; if a future refactor breaks it, this test catches
    /// it in CI rather than at a manual-test pre-flight.
    #[test]
    fn test_decide_coverage_self_documenting_sign_convention() {
        // Case 1: freshly-built LKG older than your changes.
        // file_newer_than_lkg_secs > 0 ⇒ covered=false.
        let lkg = Utc.timestamp_opt(1_000, 0).unwrap();
        let file = Utc.timestamp_opt(1_500, 0).unwrap(); // 500s after LKG
        let (covered, delta, reason) = decide_coverage(Some(lkg), Some(file));
        assert_eq!(
            (covered, delta, reason),
            (false, Some(500), super::reason::NOT_COVERED),
            "file_newer_than_lkg_secs > 0 must imply covered=false",
        );

        // Case 2: LKG built after your changes (no fresh edits).
        // file_newer_than_lkg_secs < 0 ⇒ covered=true.
        let lkg = Utc.timestamp_opt(2_000, 0).unwrap();
        let file = Utc.timestamp_opt(1_500, 0).unwrap(); // 500s before LKG
        let (covered, delta, reason) = decide_coverage(Some(lkg), Some(file));
        assert_eq!(
            (covered, delta, reason),
            (true, Some(-500), super::reason::COVERED),
            "file_newer_than_lkg_secs < 0 must imply covered=true",
        );

        // Case 3: edge — exact equality.
        let t = Utc.timestamp_opt(3_000, 0).unwrap();
        let (covered, delta, reason) = decide_coverage(Some(t), Some(t));
        assert_eq!(
            (covered, delta, reason),
            (true, Some(0), super::reason::COVERED),
            "file_newer_than_lkg_secs == 0 must imply covered=true (boundary case)",
        );
    }

    // --- end-to-end build_coverage_response with real files ---

    #[test]
    fn test_lkg_coverage_absolute_path_covered() {
        // File mtime BEFORE LKG → covered.
        // file_newer_than_lkg_secs = (file - lkg) = (100 - 200) = -100.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("hello.rs");
        fs::write(&file, "fn main() {}").expect("write");
        // Force file mtime to T=100s.
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        touch_file_mtime(&file, file_at);
        // LKG at T=200s.
        let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(200));

        let req = vec![file.to_string_lossy().into_owned()];
        // `now` is the LKG built_at — LKG is "fresh" so lkg_stale stays false
        // and we exercise the bare COVERED reason path here.
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        assert_eq!(resp.files.len(), 1);
        let f = &resp.files[0];
        assert!(f.exists, "file should exist: {f:?}");
        assert!(f.covered, "file older than LKG should be covered: {f:?}");
        assert!(
            f.file_newer_than_lkg_secs.unwrap_or(0) < 0,
            "file older than LKG ⇒ file_newer_than_lkg_secs < 0, got {f:?}"
        );
        assert_eq!(f.reason, super::reason::COVERED);
        assert!(resp.all_covered);
        assert!(!resp.lkg_stale, "LKG built_at == now ⇒ not stale");
        assert_eq!(resp.lkg_age_secs, Some(0));
        assert_eq!(resp.description, COVERAGE_DESCRIPTION);
    }

    #[test]
    fn test_lkg_coverage_absolute_path_not_covered() {
        // File mtime AFTER LKG → not covered.
        // file_newer_than_lkg_secs = (file - lkg) = (500 - 200) = +300.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("recent.rs");
        fs::write(&file, "// new").expect("write");
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        touch_file_mtime(&file, file_at);
        let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(200));

        let req = vec![file.to_string_lossy().into_owned()];
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        let f = &resp.files[0];
        assert!(f.exists);
        assert!(!f.covered, "file newer than LKG must NOT be covered");
        assert!(
            f.file_newer_than_lkg_secs.unwrap_or(0) > 0,
            "file newer than LKG ⇒ file_newer_than_lkg_secs > 0, got {f:?}"
        );
        assert_eq!(f.reason, super::reason::NOT_COVERED);
        assert!(!resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_relative_path_resolved_against_project_dir() {
        // Same as the absolute-covered test, but submit the path relative
        // to project_dir.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("relative.rs");
        fs::write(&file, "fn x() {}").expect("write");
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        touch_file_mtime(&file, file_at);
        let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(300));

        let req = vec!["relative.rs".to_string()];
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        let f = &resp.files[0];
        assert_eq!(
            f.path, "relative.rs",
            "echo back the submitted path verbatim"
        );
        assert!(f.exists, "{f:?}");
        assert!(f.covered);
        assert!(resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_path_traversal_returns_not_exists() {
        // `../../etc/passwd` escapes project_dir → exists: false.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let lkg_at = rfc3339("2026-04-26T12:00:00Z");

        let req = vec!["../../etc/passwd".to_string()];
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        let f = &resp.files[0];
        assert!(!f.exists, "traversal must report exists: false: {f:?}");
        assert!(!f.covered);
        assert_eq!(f.file_mtime, None);
        assert_eq!(f.file_newer_than_lkg_secs, None);
        assert_eq!(f.reason, super::reason::FILE_NOT_FOUND);
        assert!(!resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_nonexistent_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let lkg_at = rfc3339("2026-04-26T12:00:00Z");

        let req = vec!["definitely-not-a-real-file.rs".to_string()];
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        let f = &resp.files[0];
        assert!(!f.exists);
        assert!(!f.covered);
        assert_eq!(f.file_mtime, None);
        assert_eq!(f.file_newer_than_lkg_secs, None);
        assert_eq!(f.reason, super::reason::FILE_NOT_FOUND);
        assert!(!resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_no_lkg_means_nothing_covered() {
        // build_pool.last_known_good == None → every file covered: false.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("anything.rs");
        fs::write(&file, "x").expect("write");
        // Even an "ancient" file is uncovered when there's no LKG.
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        touch_file_mtime(&file, file_at);

        let req = vec![file.to_string_lossy().into_owned()];
        let resp = build_coverage_response(&req, project, None, Utc::now());
        let f = &resp.files[0];
        assert!(f.exists, "file should still report exists: {f:?}");
        assert!(!f.covered, "no LKG => not covered");
        assert_eq!(f.file_newer_than_lkg_secs, None);
        assert_eq!(f.reason, super::reason::NO_LKG);
        assert!(!resp.all_covered);
        assert_eq!(resp.lkg_built_at, None);
        assert_eq!(resp.lkg_age_secs, None, "no LKG ⇒ lkg_age_secs is None");
        assert!(!resp.lkg_stale, "no LKG ⇒ lkg_stale is false (no signal)");
    }

    #[test]
    fn test_lkg_coverage_multiple_files_mixed() {
        // Three files: one covered, one newer-than-LKG, one nonexistent.
        // all_covered must be false; the per-file decisions must be
        // independent.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();

        let old = project.join("old.rs");
        fs::write(&old, "x").unwrap();
        touch_file_mtime(&old, SystemTime::UNIX_EPOCH + Duration::from_secs(100));

        let new = project.join("new.rs");
        fs::write(&new, "x").unwrap();
        touch_file_mtime(&new, SystemTime::UNIX_EPOCH + Duration::from_secs(500));

        let lkg_at = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(300));

        let req = vec![
            "old.rs".to_string(),
            "new.rs".to_string(),
            "missing.rs".to_string(),
        ];
        let resp = build_coverage_response(&req, project, Some(lkg_at), lkg_at);
        assert_eq!(resp.files.len(), 3);
        assert!(resp.files[0].covered, "old.rs should be covered");
        assert_eq!(resp.files[0].reason, super::reason::COVERED);
        assert!(
            resp.files[0].file_newer_than_lkg_secs.unwrap_or(0) < 0,
            "old.rs file_newer_than_lkg_secs should be negative (file older than LKG)"
        );
        assert!(!resp.files[1].covered, "new.rs should NOT be covered");
        assert_eq!(resp.files[1].reason, super::reason::NOT_COVERED);
        assert!(
            resp.files[1].file_newer_than_lkg_secs.unwrap_or(0) > 0,
            "new.rs file_newer_than_lkg_secs should be positive (file newer than LKG)"
        );
        assert!(!resp.files[2].exists, "missing.rs should not exist");
        assert!(!resp.files[2].covered);
        assert_eq!(resp.files[2].reason, super::reason::FILE_NOT_FOUND);
        assert!(!resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_empty_request_is_not_all_covered() {
        // Vacuous-truth question: if you ask "are all of {}  covered" we
        // return false — no file means no signal, and a script trying to
        // gate on `all_covered` shouldn't pass when it forgot to specify
        // any files.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let lkg_at = rfc3339("2026-04-26T12:00:00Z");
        let resp = build_coverage_response(&[], project, Some(lkg_at), lkg_at);
        assert!(resp.files.is_empty());
        assert!(!resp.all_covered);
    }

    // --- Serialization ---

    #[test]
    fn test_response_serializes_with_null_for_missing_timestamps() {
        let resp = CoverageResponse {
            description: COVERAGE_DESCRIPTION,
            lkg_built_at: None,
            lkg_age_secs: None,
            lkg_stale: false,
            files: vec![FileCoverage {
                path: "missing.rs".to_string(),
                exists: false,
                file_mtime: None,
                covered: false,
                file_newer_than_lkg_secs: None,
                reason: super::reason::FILE_NOT_FOUND,
            }],
            all_covered: false,
            commit_provenance: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("\"lkg_built_at\":null"),
            "expected null lkg_built_at, got: {json}"
        );
        assert!(
            json.contains("\"lkg_age_secs\":null"),
            "expected null lkg_age_secs when no LKG, got: {json}"
        );
        assert!(
            json.contains("\"lkg_stale\":false"),
            "expected lkg_stale=false when no LKG, got: {json}"
        );
        assert!(
            json.contains("\"file_mtime\":null"),
            "expected null file_mtime, got: {json}"
        );
        assert!(
            json.contains("\"file_newer_than_lkg_secs\":null"),
            "expected null file_newer_than_lkg_secs, got: {json}"
        );
        assert!(
            json.contains("\"reason\":\"file_not_found\""),
            "expected reason=file_not_found, got: {json}"
        );
        assert!(json.contains("\"all_covered\":false"));
        assert!(
            json.contains("\"description\":"),
            "response must carry semantics description, got: {json}"
        );
    }

    #[test]
    fn test_response_serializes_rfc3339_timestamps() {
        let t = rfc3339("2026-04-26T12:00:00Z");
        let resp = CoverageResponse {
            description: COVERAGE_DESCRIPTION,
            lkg_built_at: Some(t),
            lkg_age_secs: Some(0),
            lkg_stale: false,
            files: vec![FileCoverage {
                path: "x.rs".to_string(),
                exists: true,
                file_mtime: Some(t),
                covered: true,
                file_newer_than_lkg_secs: Some(0),
                reason: super::reason::COVERED,
            }],
            all_covered: true,
            commit_provenance: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("2026-04-26T12:00:00+00:00"),
            "expected RFC3339 string, got: {json}"
        );
        assert!(
            json.contains("\"lkg_age_secs\":0"),
            "expected lkg_age_secs serialized as number, got: {json}"
        );
        assert!(
            json.contains("\"lkg_stale\":false"),
            "expected lkg_stale=false, got: {json}"
        );
    }

    /// Pin the no-bug-from-2026-05-07 case end-to-end. With a freshly-built
    /// LKG, a file unmodified since before the build must report:
    ///
    /// - `covered: true`
    /// - `file_newer_than_lkg_secs <= 0` (file at or older than LKG)
    /// - `reason: COVERED`
    ///
    /// This is the regression test for the ambiguity the manual-test agent
    /// hit: `delta_secs: -229250` (negative, file older than LKG, in
    /// today's convention) was paired with `covered: false`. With the
    /// flipped sign + reason field, the agent can't misread it again.
    #[test]
    fn test_unmodified_file_reports_covered_true_and_self_documents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("unchanged_since_lkg.rs");
        fs::write(&file, "fn x() {}").expect("write");
        // File last touched 1 day BEFORE LKG built_at — i.e. unmodified
        // since before the build. Mirrors the user's bug report scenario.
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        touch_file_mtime(&file, file_at);
        let lkg_built = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 + 60);
        let lkg_at = DateTime::<Utc>::from(lkg_built);

        let resp = build_coverage_response(
            &[file.to_string_lossy().into_owned()],
            project,
            Some(lkg_at),
            lkg_at,
        );
        let f = &resp.files[0];

        // Property: a freshly-built LKG covers an unmodified file.
        assert!(
            f.covered,
            "given a freshly-built LKG, an unmodified file MUST report covered=true; got {f:?}",
        );
        // Sign convention: file is older than LKG ⇒ field is non-positive.
        let secs = f
            .file_newer_than_lkg_secs
            .expect("present when both timestamps are set");
        assert!(
            secs <= 0,
            "covered file must have file_newer_than_lkg_secs <= 0, got {secs}",
        );
        // Stable machine-readable reason.
        assert_eq!(f.reason, super::reason::COVERED);
        // The response carries a description so a human caller doesn't
        // have to read the source.
        assert!(
            !resp.description.is_empty(),
            "response.description must be non-empty so callers can self-document",
        );
        assert!(resp.all_covered);
    }

    // ---- top-level LKG age + stale flag (Phase 7) ----
    //
    // Phase 7 of the 2026-05-20 manual-test remediation: expose
    // `lkg_age_secs` and `lkg_stale` on `/lkg/coverage` so agents can detect
    // a stale-but-covering LKG without parsing the RFC3339 `built_at` string
    // themselves. These tests pin the three documented cases (fresh,
    // stale, absent) plus the per-file `covered_but_lkg_stale` promotion.

    /// LKG present and fresh (built 100s ago): age ≈ 100, stale=false.
    #[test]
    fn test_lkg_age_fresh_under_threshold() {
        let now = rfc3339("2026-05-20T12:00:00Z");
        let built_at = now - chrono::Duration::seconds(100);
        let (age, stale) = compute_lkg_age(Some(built_at), now);
        assert_eq!(age, Some(100), "age must be (now - built_at).num_seconds()");
        assert!(
            !stale,
            "100s < {LKG_STALE_THRESHOLD_SECS}s threshold ⇒ not stale"
        );
    }

    /// LKG present and stale (built 90000s ago > 86400s): age ≈ 90000,
    /// stale=true. 90000s is the spec value from the Phase 7 brief.
    #[test]
    fn test_lkg_age_stale_over_threshold() {
        let now = rfc3339("2026-05-20T12:00:00Z");
        let built_at = now - chrono::Duration::seconds(90_000);
        let (age, stale) = compute_lkg_age(Some(built_at), now);
        assert_eq!(age, Some(90_000));
        assert!(
            stale,
            "90000s > {LKG_STALE_THRESHOLD_SECS}s threshold ⇒ stale"
        );
    }

    /// Boundary: age exactly == threshold ⇒ NOT stale (rule is strictly `>`).
    /// Pin this so a future "off-by-one" refactor can't silently flip it.
    #[test]
    fn test_lkg_age_exactly_at_threshold_is_not_stale() {
        let now = rfc3339("2026-05-20T12:00:00Z");
        let built_at = now - chrono::Duration::seconds(LKG_STALE_THRESHOLD_SECS);
        let (age, stale) = compute_lkg_age(Some(built_at), now);
        assert_eq!(age, Some(LKG_STALE_THRESHOLD_SECS));
        assert!(
            !stale,
            "age == threshold must be `not stale` (rule is `age > threshold`)"
        );

        // One second over the boundary: now stale.
        let built_at = now - chrono::Duration::seconds(LKG_STALE_THRESHOLD_SECS + 1);
        let (_, stale) = compute_lkg_age(Some(built_at), now);
        assert!(stale, "age == threshold + 1 must flip to stale");
    }

    /// No LKG: age=None, stale=false. Mirrors the `no_lkg_yet` case so
    /// callers see a single "I have no signal yet" shape rather than
    /// `stale=true` (which would falsely imply "rebuild needed because LKG
    /// is old").
    #[test]
    fn test_lkg_age_none_when_no_lkg() {
        let now = rfc3339("2026-05-20T12:00:00Z");
        let (age, stale) = compute_lkg_age(None, now);
        assert_eq!(age, None, "no LKG ⇒ lkg_age_secs is None");
        assert!(!stale, "no LKG ⇒ lkg_stale is false (no signal, not stale)");
    }

    /// End-to-end: fresh LKG + covered file ⇒ `lkg_stale: false`,
    /// `lkg_age_secs: Some(~100)`, per-file `reason: COVERED` (NOT the
    /// stale variant).
    #[test]
    fn test_build_coverage_response_lkg_present_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("covered.rs");
        fs::write(&file, "x").unwrap();
        // file mtime: 1000s before LKG (covered).
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        touch_file_mtime(&file, file_at);
        // LKG built_at: 1_001_000s since epoch.
        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_001_000),
        ));
        // now: 100s after LKG built_at ⇒ fresh.
        let now = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(1_001_100));

        let req = vec!["covered.rs".to_string()];
        let resp = build_coverage_response(&req, project, lkg_at, now);

        assert_eq!(resp.lkg_age_secs, Some(100));
        assert!(!resp.lkg_stale, "100s LKG ⇒ not stale");
        let f = &resp.files[0];
        assert!(f.covered, "older file ⇒ covered");
        assert_eq!(
            f.reason,
            super::reason::COVERED,
            "fresh LKG ⇒ per-file reason stays at COVERED, not COVERED_BUT_STALE"
        );
        assert!(resp.all_covered);
    }

    /// End-to-end: stale LKG + covered file ⇒ `lkg_stale: true`,
    /// `lkg_age_secs: Some(~90000)`, AND the per-file `reason` is promoted
    /// from `COVERED` to `COVERED_BUT_STALE`. This is the signal the
    /// 2026-05-16 manual-test session was missing.
    #[test]
    fn test_build_coverage_response_lkg_present_stale_promotes_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("ancient.rs");
        fs::write(&file, "x").unwrap();
        // file is much older than LKG (still covered).
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(500_000);
        touch_file_mtime(&file, file_at);
        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        ));
        // now: 90_000s after LKG ⇒ stale (> 86_400).
        let now = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(1_090_000));

        let req = vec!["ancient.rs".to_string()];
        let resp = build_coverage_response(&req, project, lkg_at, now);

        assert_eq!(resp.lkg_age_secs, Some(90_000));
        assert!(resp.lkg_stale, "90000s > 86400s threshold ⇒ stale");
        let f = &resp.files[0];
        assert!(f.covered, "file still older than LKG ⇒ covered=true");
        assert_eq!(
            f.reason,
            super::reason::COVERED_BUT_STALE,
            "covered + stale LKG ⇒ reason promoted to COVERED_BUT_STALE"
        );
        // `all_covered` tracks `covered`, NOT staleness. Agents reading
        // `all_covered: true` plus `lkg_stale: true` can decide for
        // themselves whether to rebuild.
        assert!(
            resp.all_covered,
            "all_covered tracks per-file `covered`, independent of staleness"
        );
    }

    /// End-to-end: no LKG ⇒ age and stale are both the "absent" shape.
    /// Confirms that the per-file `NO_LKG` reason is NOT promoted to
    /// `COVERED_BUT_STALE` (the promotion only fires when `covered=true`).
    #[test]
    fn test_build_coverage_response_no_lkg_age_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("any.rs");
        fs::write(&file, "x").unwrap();
        touch_file_mtime(&file, SystemTime::UNIX_EPOCH + Duration::from_secs(1));

        let resp = build_coverage_response(
            &["any.rs".to_string()],
            project,
            None, // no LKG
            Utc::now(),
        );
        assert_eq!(resp.lkg_age_secs, None);
        assert!(!resp.lkg_stale);
        assert_eq!(
            resp.files[0].reason,
            super::reason::NO_LKG,
            "no-LKG case must keep the NO_LKG reason, not COVERED_BUT_STALE"
        );
    }

    /// `lkg_age_secs` and `lkg_stale` serialize as plain JSON number /
    /// boolean (no string-wrapping). Pins the wire shape so a future
    /// `#[serde(serialize_with)]` refactor can't silently change it.
    #[test]
    fn test_response_serializes_lkg_age_and_stale_as_plain_types() {
        let t = rfc3339("2026-05-20T12:00:00Z");
        let resp = CoverageResponse {
            description: COVERAGE_DESCRIPTION,
            lkg_built_at: Some(t),
            lkg_age_secs: Some(90_000),
            lkg_stale: true,
            files: vec![],
            all_covered: false,
            commit_provenance: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("\"lkg_age_secs\":90000"),
            "lkg_age_secs must serialize as a plain JSON number, got: {json}"
        );
        assert!(
            json.contains("\"lkg_stale\":true"),
            "lkg_stale must serialize as a plain JSON boolean, got: {json}"
        );
    }

    // ---- spawn-test LKG staleness scan ----

    #[test]
    fn decide_lkg_stale_when_src_newer_than_lkg() {
        let lkg = rfc3339("2026-05-17T00:00:00Z");
        let newest = Some(("src/main.rs".to_string(), rfc3339("2026-05-17T00:05:00Z")));
        let s = decide_lkg_stale(lkg, newest).expect("some when newest present");
        assert!(s.stale, "src newer than LKG must be stale");
        assert_eq!(s.lag_secs, 300);
        assert_eq!(s.newest_src_file, "src/main.rs");
    }

    #[test]
    fn decide_lkg_not_stale_when_src_older_or_equal() {
        let lkg = rfc3339("2026-05-17T00:00:00Z");
        // older
        let older = Some(("src/a.rs".to_string(), rfc3339("2026-05-16T23:00:00Z")));
        let s = decide_lkg_stale(lkg, older).expect("some");
        assert!(!s.stale);
        assert_eq!(s.lag_secs, -3600);
        // exact equality ⇒ NOT stale (rule is strictly greater)
        let eq = Some(("src/a.rs".to_string(), rfc3339("2026-05-17T00:00:00Z")));
        let s = decide_lkg_stale(lkg, eq).expect("some");
        assert!(!s.stale);
        assert_eq!(s.lag_secs, 0);
    }

    #[test]
    fn decide_lkg_stale_none_when_no_sources() {
        let lkg = rfc3339("2026-05-17T00:00:00Z");
        assert!(decide_lkg_stale(lkg, None).is_none());
    }

    #[test]
    fn newest_rs_walk_picks_newest_and_prunes() {
        let tmp = std::env::temp_dir().join(format!("qsup-lkg-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        fs::create_dir_all(src.join("nested")).expect("mkdir");
        fs::create_dir_all(src.join("target").join("debug")).expect("mkdir target");

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mid = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_001_000);
        let new = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_002_000);
        let newest_pruned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_009_999);

        touch_file_mtime(&src.join("a.rs"), old);
        touch_file_mtime(&src.join("nested").join("b.rs"), new);
        touch_file_mtime(&src.join("nested").join("c.rs"), mid);
        // A .rs under a pruned `target*` dir must be ignored even though
        // it's the newest file on disk.
        touch_file_mtime(
            &src.join("target").join("debug").join("z.rs"),
            newest_pruned,
        );
        // Non-.rs must be ignored.
        touch_file_mtime(&src.join("d.txt"), newest_pruned);

        let (relpath, mtime) = newest_rs_under_src(&tmp).expect("some — readable .rs present");
        assert_eq!(relpath, "src/nested/b.rs");
        let expected: DateTime<Utc> = new.into();
        assert_eq!(mtime, expected);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn newest_rs_walk_none_when_no_src_dir() {
        let tmp = std::env::temp_dir().join(format!("qsup-lkg-nosrc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("mkdir");
        assert!(newest_rs_under_src(&tmp).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }
}

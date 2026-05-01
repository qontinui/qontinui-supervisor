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
//! ## Query
//!
//! `path` is repeatable: `?path=foo.rs&path=bar.rs` checks both files in one
//! request. Single-file callers (the common case) use it once. Paths may be
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
//!     lkg_built_at: <RFC3339 string | null>,
//!     files: [
//!       {
//!         path: <echoed back, normalized>,
//!         exists: <bool>,
//!         file_mtime: <RFC3339 string | null>,
//!         covered: <bool>,
//!         delta_secs: <i64 | null>,  // (lkg_built_at - file_mtime).as_secs()
//!                                    // positive => LKG is newer (covered)
//!                                    // negative => file is newer (NOT covered)
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
//!   file_mtime: null, delta_secs: null}` so a script passing a list of files
//!   doesn't bail on the first bad entry. This includes path-traversal
//!   attempts (e.g. `../../etc/passwd`) — those resolve outside `project_dir`
//!   and are reported as `exists: false` rather than 400, on the same
//!   "graceful per-file failure" principle.
//! - `state.build_pool.last_known_good == None` (no successful build yet):
//!   `lkg_built_at: null`, every file `covered: false`, `delta_secs: null`,
//!   `all_covered: false`.
//! - File doesn't exist on disk: `exists: false, file_mtime: null,
//!   covered: false, delta_secs: null`.

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
    /// True iff the LKG is at least as new as this file.
    pub covered: bool,
    /// `(lkg_built_at - file_mtime).num_seconds()`. Positive means LKG is
    /// newer (good — file is covered). Negative means file is newer than
    /// LKG (not covered). `None` when either timestamp is missing.
    pub delta_secs: Option<i64>,
}

/// Top-level response payload for `GET /lkg/coverage`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoverageResponse {
    /// LKG `built_at` as RFC3339 UTC. `None` when no successful build has
    /// produced an LKG yet on this checkout.
    #[serde(serialize_with = "ser_opt_dt")]
    pub lkg_built_at: Option<DateTime<Utc>>,
    pub files: Vec<FileCoverage>,
    /// True iff every requested file is `covered: true`. `false` when there
    /// are no files OR when LKG is absent OR when any file is not covered.
    pub all_covered: bool,
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
/// produce a `(covered, delta_secs)` pair. Extracted for unit testing —
/// the file-system parts of the handler are unit-tested separately via
/// `tempfile`-backed real files, but the decision rule itself doesn't need
/// I/O to verify.
pub fn decide_coverage(
    lkg_at: Option<DateTime<Utc>>,
    file_mtime: Option<DateTime<Utc>>,
) -> (bool, Option<i64>) {
    match (lkg_at, file_mtime) {
        (Some(lkg), Some(m)) => {
            let delta = (lkg - m).num_seconds();
            // Covered when LKG is at least as new as the file:
            //   file_mtime <= lkg_built_at  iff  delta >= 0.
            (delta >= 0, Some(delta))
        }
        _ => (false, None),
    }
}

/// Coverage decision for one file given a normalized absolute path and the
/// LKG built_at. Pure-ish (filesystem read for mtime). Caller is responsible
/// for path resolution; this function just stats and decides.
fn coverage_for_resolved(
    echo_path: &str,
    resolved: Option<&Path>,
    lkg_at: Option<DateTime<Utc>>,
) -> FileCoverage {
    let path = resolved;
    let mtime = path.and_then(read_file_mtime);
    let exists = mtime.is_some();
    let (covered, delta_secs) = decide_coverage(lkg_at, mtime);
    FileCoverage {
        path: echo_path.to_string(),
        exists,
        file_mtime: mtime,
        covered,
        delta_secs,
    }
}

/// Compute the full coverage response from already-resolved data. Pulled
/// out as a pure function so the bulk of the logic can be unit-tested
/// without a router or `SharedState`.
pub fn build_coverage_response(
    requested: &[String],
    project_dir: &Path,
    lkg_at: Option<DateTime<Utc>>,
) -> CoverageResponse {
    let mut files = Vec::with_capacity(requested.len());
    for raw in requested {
        let resolved = resolve_path(raw, project_dir);
        let resolved_ref = resolved.as_deref();
        files.push(coverage_for_resolved(raw, resolved_ref, lkg_at));
    }
    let all_covered = !files.is_empty() && files.iter().all(|f| f.covered);
    CoverageResponse {
        lkg_built_at: lkg_at,
        files,
        all_covered,
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
    let lkg_at = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .map(|info| info.built_at);
    let project_dir = state.config.project_dir.clone();
    let response = build_coverage_response(&requested, &project_dir, lkg_at);

    let data = serde_json::to_value(&response).unwrap_or_else(|_| json!({}));
    Json(json!({
        "success": true,
        "data": data,
        "timestamp": chrono::Utc::now().timestamp_millis(),
    }))
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
        // paths AND paths relative to project_dir").
        let abs = "D:/somewhere/else/file.rs";
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

    #[test]
    fn test_decide_coverage_no_lkg() {
        let (covered, delta) = decide_coverage(None, Some(Utc::now()));
        assert!(!covered);
        assert_eq!(delta, None);
    }

    #[test]
    fn test_decide_coverage_no_file() {
        let (covered, delta) = decide_coverage(Some(Utc::now()), None);
        assert!(!covered);
        assert_eq!(delta, None);
    }

    #[test]
    fn test_decide_coverage_file_older_than_lkg() {
        // file at T=100, LKG at T=200 → covered=true, delta=+100s.
        let lkg = Utc.timestamp_opt(200, 0).unwrap();
        let file = Utc.timestamp_opt(100, 0).unwrap();
        let (covered, delta) = decide_coverage(Some(lkg), Some(file));
        assert!(covered);
        assert_eq!(delta, Some(100));
    }

    #[test]
    fn test_decide_coverage_file_equal_to_lkg() {
        // Spec: "max(mtime) <= lkg.built_at" — equal counts as covered.
        let t = Utc.timestamp_opt(150, 0).unwrap();
        let (covered, delta) = decide_coverage(Some(t), Some(t));
        assert!(covered);
        assert_eq!(delta, Some(0));
    }

    #[test]
    fn test_decide_coverage_file_newer_than_lkg() {
        // file at T=300, LKG at T=200 → covered=false, delta=-100s.
        let lkg = Utc.timestamp_opt(200, 0).unwrap();
        let file = Utc.timestamp_opt(300, 0).unwrap();
        let (covered, delta) = decide_coverage(Some(lkg), Some(file));
        assert!(!covered);
        assert_eq!(delta, Some(-100));
    }

    // --- end-to-end build_coverage_response with real files ---

    #[test]
    fn test_lkg_coverage_absolute_path_covered() {
        // File mtime BEFORE LKG → covered.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("hello.rs");
        fs::write(&file, "fn main() {}").expect("write");
        // Force file mtime to T=100s.
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        touch_file_mtime(&file, file_at);
        // LKG at T=200s.
        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(200),
        ));

        let req = vec![file.to_string_lossy().into_owned()];
        let resp = build_coverage_response(&req, project, lkg_at);
        assert_eq!(resp.files.len(), 1);
        let f = &resp.files[0];
        assert!(f.exists, "file should exist: {f:?}");
        assert!(f.covered, "file older than LKG should be covered: {f:?}");
        assert!(f.delta_secs.unwrap_or(0) > 0);
        assert!(resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_absolute_path_not_covered() {
        // File mtime AFTER LKG → not covered.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let file = project.join("recent.rs");
        fs::write(&file, "// new").expect("write");
        let file_at = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        touch_file_mtime(&file, file_at);
        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(200),
        ));

        let req = vec![file.to_string_lossy().into_owned()];
        let resp = build_coverage_response(&req, project, lkg_at);
        let f = &resp.files[0];
        assert!(f.exists);
        assert!(!f.covered, "file newer than LKG must NOT be covered");
        assert!(f.delta_secs.unwrap_or(0) < 0);
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
        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(300),
        ));

        let req = vec!["relative.rs".to_string()];
        let resp = build_coverage_response(&req, project, lkg_at);
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
        let lkg_at = Some(rfc3339("2026-04-26T12:00:00Z"));

        let req = vec!["../../etc/passwd".to_string()];
        let resp = build_coverage_response(&req, project, lkg_at);
        let f = &resp.files[0];
        assert!(!f.exists, "traversal must report exists: false: {f:?}");
        assert!(!f.covered);
        assert_eq!(f.file_mtime, None);
        assert_eq!(f.delta_secs, None);
        assert!(!resp.all_covered);
    }

    #[test]
    fn test_lkg_coverage_nonexistent_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        let lkg_at = Some(rfc3339("2026-04-26T12:00:00Z"));

        let req = vec!["definitely-not-a-real-file.rs".to_string()];
        let resp = build_coverage_response(&req, project, lkg_at);
        let f = &resp.files[0];
        assert!(!f.exists);
        assert!(!f.covered);
        assert_eq!(f.file_mtime, None);
        assert_eq!(f.delta_secs, None);
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
        let resp = build_coverage_response(&req, project, None);
        let f = &resp.files[0];
        assert!(f.exists, "file should still report exists: {f:?}");
        assert!(!f.covered, "no LKG => not covered");
        assert_eq!(f.delta_secs, None);
        assert!(!resp.all_covered);
        assert_eq!(resp.lkg_built_at, None);
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

        let lkg_at = Some(DateTime::<Utc>::from(
            SystemTime::UNIX_EPOCH + Duration::from_secs(300),
        ));

        let req = vec![
            "old.rs".to_string(),
            "new.rs".to_string(),
            "missing.rs".to_string(),
        ];
        let resp = build_coverage_response(&req, project, lkg_at);
        assert_eq!(resp.files.len(), 3);
        assert!(resp.files[0].covered, "old.rs should be covered");
        assert!(!resp.files[1].covered, "new.rs should NOT be covered");
        assert!(!resp.files[2].exists, "missing.rs should not exist");
        assert!(!resp.files[2].covered);
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
        let resp = build_coverage_response(&[], project, Some(rfc3339("2026-04-26T12:00:00Z")));
        assert!(resp.files.is_empty());
        assert!(!resp.all_covered);
    }

    // --- Serialization ---

    #[test]
    fn test_response_serializes_with_null_for_missing_timestamps() {
        let resp = CoverageResponse {
            lkg_built_at: None,
            files: vec![FileCoverage {
                path: "missing.rs".to_string(),
                exists: false,
                file_mtime: None,
                covered: false,
                delta_secs: None,
            }],
            all_covered: false,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("\"lkg_built_at\":null"),
            "expected null lkg_built_at, got: {json}"
        );
        assert!(
            json.contains("\"file_mtime\":null"),
            "expected null file_mtime, got: {json}"
        );
        assert!(
            json.contains("\"delta_secs\":null"),
            "expected null delta_secs, got: {json}"
        );
        assert!(json.contains("\"all_covered\":false"));
    }

    #[test]
    fn test_response_serializes_rfc3339_timestamps() {
        let t = rfc3339("2026-04-26T12:00:00Z");
        let resp = CoverageResponse {
            lkg_built_at: Some(t),
            files: vec![FileCoverage {
                path: "x.rs".to_string(),
                exists: true,
                file_mtime: Some(t),
                covered: true,
                delta_secs: Some(0),
            }],
            all_covered: true,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("2026-04-26T12:00:00+00:00"),
            "expected RFC3339 string, got: {json}"
        );
    }
}

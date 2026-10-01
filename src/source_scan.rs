//! Shared classification for the crate's SOURCE-SCANNING tests: the guards
//! that read `src/**/*.rs` as text and police only its production code.
//!
//! Those guards find test code by an IN-FILE `#[cfg(test)]` span. A test
//! module extracted to its own file (`foo.rs` -> `foo/tests.rs`, by
//! `qontinui-claude-config/scripts/extract-rust-test-modules.py`) has no such
//! span — the `#[cfg(test)]` stays on the parent's `mod tests;` line — so
//! without this predicate every one of them would read the moved test code as
//! production. The codemod opens each extracted file with `#![cfg(test)]`,
//! which is the file-local signal this module keys on: a guard needs no parent
//! lookup and no path convention.
//!
//! Plan `2026-10-01-oversized-source-files-owe-a-decomposition`, Phase 2b.

/// The inner attribute that marks a whole file as test-only.
pub(crate) const TEST_ONLY_FILE_ATTR: &str = "#![cfg(test)]";

/// True when `text`'s first line that is neither blank nor a comment is
/// `#![cfg(test)]` — i.e. the whole file is test code and no part of it is
/// production.
///
/// `//`, `///` and `//!` lines and `/* … */` block comments are skipped —
/// block comments with NESTING, as rustc reads them, so text inside an inner
/// `/* … */` can never surface as the marker. A leading byte-order mark and
/// CRLF line endings are tolerated, and a trailing `//` comment after the
/// attribute is allowed. Anything else first — including another inner
/// attribute — means the file is not classified test-only, which is the
/// conservative direction for a guard policing production: an unrecognised
/// file stays in scope.
pub(crate) fn is_test_only_file(text: &str) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("//") {
            // Line comment (`//`, `///`, `//!`): skip to the end of the line.
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if rest.starts_with("/*") {
            match skip_block_comment(rest) {
                Some(after) => rest = after,
                // Unterminated: rustc would reject the file; keep it in scope.
                None => return false,
            }
        } else {
            break;
        }
    }
    let Some(after) = rest.strip_prefix(TEST_ONLY_FILE_ATTR) else {
        return false;
    };
    let line = after
        .split_once('\n')
        .map_or(after, |(line, _)| line)
        .trim();
    line.is_empty() || line.starts_with("//")
}

/// `text` starts with `/*`; return what follows its matching `*/`, honouring
/// nested block comments. `None` when the comment never closes.
fn skip_block_comment(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(&text[i..]);
                }
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::is_test_only_file;

    #[test]
    fn the_codemod_header_marks_a_file_test_only() {
        // Exactly the header `extract-rust-test-modules.py` writes.
        assert!(is_test_only_file(
            "#![cfg(test)]\n\nuse super::*;\n\n#[test]\nfn t() {}\n"
        ));
        assert!(is_test_only_file("#![cfg(test)]\r\n\r\nuse super::*;\r\n"));
        assert!(is_test_only_file("\u{feff}#![cfg(test)]\n"));
    }

    #[test]
    fn leading_comments_and_blank_lines_are_skipped() {
        assert!(is_test_only_file(
            "\n// licence\n//! module doc\n/// stray\n   \n#![cfg(test)]\nfn x() {}\n"
        ));
        assert!(is_test_only_file(
            "/* one-line */\n/*\n * multi\n * line\n */\n#![cfg(test)] // why\n"
        ));
        assert!(is_test_only_file("/* a */ /* b */ #![cfg(test)]\n"));
    }

    #[test]
    fn a_production_file_is_not_test_only() {
        // An ordinary file, and one with an in-file test module: that is the
        // OUTER attribute, which other guards' in-file spans already handle.
        assert!(!is_test_only_file("use std::fs;\n\npub fn f() {}\n"));
        assert!(!is_test_only_file(
            "pub fn f() {}\n\n#[cfg(test)]\nmod tests {}\n"
        ));
        assert!(!is_test_only_file("#[cfg(test)]\nmod tests;\n"));
        // The marker must be FIRST: anything before it keeps the file in scope.
        assert!(!is_test_only_file("#![allow(dead_code)]\n#![cfg(test)]\n"));
        assert!(!is_test_only_file("pub fn f() {}\n#![cfg(test)]\n"));
        // Not the marker: a different predicate, or trailing code.
        assert!(!is_test_only_file("#![cfg(any(test, feature = \"x\"))]\n"));
        assert!(!is_test_only_file("#![cfg(test)] fn sneaky() {}\n"));
        // A marker hidden in a comment does not count.
        assert!(!is_test_only_file("// #![cfg(test)]\npub fn f() {}\n"));
        assert!(!is_test_only_file("/*\n#![cfg(test)]\n*/\npub fn f() {}\n"));
        // Block comments NEST: the marker on line 2 is still inside the outer
        // comment, which only the final `*/` closes.
        assert!(!is_test_only_file(
            "/* a /* b */\n#![cfg(test)]\n*/\npub fn f() {}\n"
        ));
        assert!(is_test_only_file("/* a /* b */ c */\n#![cfg(test)]\n"));
        // An unterminated comment is not classified.
        assert!(!is_test_only_file("/* never closed\n#![cfg(test)]\n"));
        // Empty / comment-only.
        assert!(!is_test_only_file(""));
        assert!(!is_test_only_file("// only a comment\n"));
    }
}

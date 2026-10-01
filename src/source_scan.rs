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

/// The production part of one LF-normalized source file: everything before
/// its first `#[cfg(test)] mod`, and NOTHING for a file that is test code in
/// its entirety ([`is_test_only_file`]).
///
/// The cut is anchored on the test MODULE, not the bare attribute:
/// `#[cfg(test)]` also decorates test-only helper items, and cutting at the
/// first of those hides every production function after it (see
/// `routes::runners::tests::scan_production_lines`). A module extracted to its
/// own file leaves `#[cfg(test)]\nmod tests;` in the parent, so the cut lands
/// in the same place before and after the extraction — which is what lets a
/// self-scan of the parent read identical production text either way.
pub(crate) fn production_span(text: &str) -> &str {
    if is_test_only_file(text) {
        return "";
    }
    text.split_once("\n#[cfg(test)]\nmod ")
        .map(|(before, _)| before)
        .unwrap_or(text)
}

/// The body of the function whose signature starts with `signature`: the text
/// after the first occurrence of `signature`, up to and including the `}` that
/// closes the function's body. `None` when `signature` does not occur or no
/// body follows it (a `;` before any `{`, or an unbalanced file).
///
/// The end is found by BRACE MATCHING, not by looking for the next item. A
/// "next top-level `fn`" heuristic stops at whichever item-start spellings it
/// lists and runs on past every other one (`pub(crate) fn`, `pub(super) fn`,
/// `const fn`, `unsafe fn`, `extern "C" fn`, an `impl` block, a `static`), so
/// a source assertion could be satisfied by text in a LATER function. Braces
/// inside comments (`//`, nested `/* */`), string literals (`"…"`, `b"…"`,
/// `r#"…"#`, `br"…"`, `c"…"`) and char literals (`'{'`, `'\''`) do not count;
/// a lifetime or label (`'a`) is not mistaken for a char literal. Before the
/// body opens, a brace inside `()`, `[]` or `<>` belongs to the signature (a
/// struct pattern parameter, a const-generic expression), not the body.
/// Known limit, loud rather than silent: a `<` used as an OPERATOR in the
/// signature outside braces (`[u8; 1 << 2]`) leaves the body unrecognised
/// and returns `None`.
pub(crate) fn fn_body<'t>(text: &'t str, signature: &str) -> Option<&'t str> {
    let start = text.find(signature)? + signature.len();
    let after = &text[start..];
    let bytes = after.as_bytes();
    let mut depth = 0usize;
    // Signature-phase nesting (only while `depth == 0`): `(` / `[` and `<` /
    // `>`, so neither a `;` (`[u8; 4]`) nor a brace inside the signature
    // (`Foo<{ N + 1 }>`, `[u8; { N }]`) is taken for the end of a body-less
    // declaration or for the body's opening brace. `sig` counts braces
    // opened inside such a group.
    // Callers pass a signature ending in its opening `(` or `<`, which is
    // already consumed: start from the brackets it leaves open, or a struct
    // pattern parameter (`Foo { a }: Foo`) would read as the body.
    let (mut group, mut angle) = open_brackets(signature);
    let mut sig = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = after[i..].find('\n').map_or(bytes.len(), |n| i + n);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = after.len() - skip_block_comment(&after[i..])?.len();
            }
            b'"' => i = skip_string(bytes, i + 1)?,
            b'r' | b'b' | b'c' if !is_ident_byte(bytes, i.wrapping_sub(1)) => {
                match raw_or_prefixed_string(bytes, i) {
                    Some(end) => i = end?,
                    None => i += 1,
                }
            }
            b'\'' => i = skip_char_or_lifetime(after, i),
            b'(' | b'[' => {
                group += 1;
                i += 1;
            }
            b')' | b']' => {
                group = group.saturating_sub(1);
                i += 1;
            }
            b'<' if depth == 0 && sig == 0 => {
                angle += 1;
                i += 1;
            }
            // `->` is an arrow, not a closing angle bracket.
            b'>' if depth == 0 && sig == 0 && i > 0 && bytes[i - 1] != b'-' => {
                angle = angle.saturating_sub(1);
                i += 1;
            }
            b';' if depth == 0 && group == 0 && angle == 0 && sig == 0 => return None,
            b'{' if depth == 0 && (sig > 0 || angle > 0 || group > 0) => {
                sig += 1;
                i += 1;
            }
            b'}' if depth == 0 && sig > 0 => {
                sig -= 1;
                i += 1;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(&after[..i]);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The `(`/`[` and `<` nesting a signature prefix leaves open (`->` is not a
/// closing `>`).
fn open_brackets(signature: &str) -> (usize, usize) {
    let bytes = signature.as_bytes();
    let (mut group, mut angle) = (0usize, 0usize);
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' => group += 1,
            b')' | b']' => group = group.saturating_sub(1),
            b'<' => angle += 1,
            b'>' if i == 0 || bytes[i - 1] != b'-' => angle = angle.saturating_sub(1),
            _ => {}
        }
    }
    (group, angle)
}

/// Whether the byte at `i` (if any) continues an identifier, so that an `r`,
/// `b` or `c` there is part of a name and not a literal prefix.
fn is_ident_byte(bytes: &[u8], i: usize) -> bool {
    bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80)
}

/// `bytes[i]` is just past an opening `"`; return the index just past the
/// closing one, honouring `\` escapes. `None` when the string never closes.
fn skip_string(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// At an `r`/`b`/`c` that starts no identifier: if it begins a string literal
/// (`r"`, `r#"`, `b"`, `br#"`, `c"`, `cr"`), `Some(end)` with the index just past
/// it (`Some(None)` when it never closes); `None` when it is not a literal
/// (an identifier such as `ready`, or a byte char `b'x'`, which the char arm
/// handles next).
fn raw_or_prefixed_string(bytes: &[u8], i: usize) -> Option<Option<usize>> {
    let mut j = i;
    if matches!(bytes[j], b'b' | b'c') {
        j += 1;
    }
    let raw = bytes.get(j) == Some(&b'r');
    if raw {
        j += 1;
    }
    if !raw {
        // `b"…"` / `c"…"`: an ordinary escaped string after the prefix.
        return (bytes.get(j) == Some(&b'"')).then(|| skip_string(bytes, j + 1));
    }
    let hashes = bytes[j..].iter().take_while(|b| **b == b'#').count();
    j += hashes;
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let mut k = j + 1;
    while k < bytes.len() {
        if bytes[k] == b'"'
            && bytes[k + 1..]
                .iter()
                .take(hashes)
                .filter(|b| **b == b'#')
                .count()
                == hashes
        {
            return Some(Some(k + 1 + hashes));
        }
        k += 1;
    }
    Some(None)
}

/// At a `'`: skip a char literal (`'x'`, `'\n'`, `'\''`, `'\u{7f}'`, `'é'`)
/// whole, or only the `'` of a lifetime or label (`'a`, `'static`).
fn skip_char_or_lifetime(text: &str, i: usize) -> usize {
    let rest = &text[i + 1..];
    if let Some(escaped) = rest.strip_prefix('\\') {
        // An escape: the literal ends at the next `'` after the escaped char.
        let skip = escaped.chars().next().map_or(0, char::len_utf8);
        return match escaped[skip..].find('\'') {
            Some(n) => i + 2 + skip + n + 1,
            None => text.len(),
        };
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), Some('\'')) => i + 1 + c.len_utf8() + 1,
        _ => i + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{fn_body, is_test_only_file, production_span};

    #[test]
    fn production_span_is_the_same_for_inline_and_extracted_tests() {
        let prod = "pub fn f() {}\n";
        let inline =
            format!("{prod}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{}}\n}}\n");
        let parent = format!("{prod}\n#[cfg(test)]\nmod tests;\n");
        assert_eq!(production_span(&inline), production_span(&parent));
        assert_eq!(production_span(&parent), prod);
        assert_eq!(production_span("#![cfg(test)]\n\n#[test]\nfn t() {}\n"), "");
        // A test-only helper ITEM does not end production.
        let helper = "#[cfg(test)]\npub fn helper() {}\npub fn real() {}\n";
        assert_eq!(production_span(helper), helper);
    }

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

    /// The bound ends at the function's own closing brace, whatever item
    /// follows it. The old "next top-level `fn`" heuristic (`\npub async fn `,
    /// `\nasync fn `, `\npub fn `, `\nfn `) ran on into every one of these.
    #[test]
    fn fn_body_stops_at_the_closing_brace_before_any_following_item() {
        for next in [
            "pub(crate) fn later() {",
            "pub(crate) async fn later() {",
            "pub(super) fn later() {",
            "pub(in crate::routes) fn later() {",
            "const fn later() {",
            "pub const fn later() {",
            "unsafe fn later() {",
            "pub unsafe extern \"C\" fn later() {",
            "impl Later {\n    pub fn later() {",
            "static LATER: &str = \"x\"; fn later() {",
            "#[cfg(test)]\nmod tests {\n    fn later() {",
        ] {
            let text = format!(
                "pub async fn target(x: u8) -> u8 {{\n    keep_me();\n    x\n}}\n\n{next}\n    NEEDLE_IN_LATER();\n}}\n"
            );
            let body = fn_body(&text, "pub async fn target(").expect("target has a body");
            assert!(
                body.contains("keep_me();"),
                "{next}: lost the target's own body"
            );
            assert!(
                !body.contains("NEEDLE_IN_LATER"),
                "the bound ran on past `{next}` into a later item: {body:?}"
            );
            assert!(
                body.ends_with("x\n}"),
                "{next}: body must end at its own `}}`"
            );
        }
    }

    /// A METHOD: indented inside an `impl`, its `}` is not at column 0, so the
    /// old `"\n}\n"` cut ran on through every sibling method to the `impl`'s
    /// own closing brace.
    #[test]
    fn fn_body_stops_at_an_indented_method_s_own_brace() {
        let text = concat!(
            "impl Runner {\n",
            "    pub(crate) async fn target(&self) {\n",
            "        if self.ok() {\n",
            "            keep_me();\n",
            "        }\n",
            "    }\n",
            "\n",
            "    fn sibling(&self) {\n",
            "        NEEDLE_IN_SIBLING();\n",
            "    }\n",
            "}\n",
        );
        let body = fn_body(text, "pub(crate) async fn target(").expect("method has a body");
        assert!(body.contains("keep_me();"));
        assert!(
            !body.contains("NEEDLE_IN_SIBLING"),
            "ran on into the sibling method: {body:?}"
        );
        assert!(body.ends_with("        }\n    }"));
    }

    #[test]
    fn fn_body_ignores_braces_in_comments_and_literals() {
        let text = concat!(
            "fn target() {\n",
            "    // a stray } in a line comment\n",
            "    /* nested /* } */ still comment } */\n",
            "    let s = \"}\\\"}\";\n",
            "    let r = r#\"} \" }\"#;\n",
            "    let b = b\"}\";\n",
            "    let br = br##\"}\"#}\"##;\n",
            "    let c = '}';\n",
            "    let q = '\\'';\n",
            "    let u = '\\u{7d}';\n",
            "    let e = 'é';\n",
            "    fn inner<'a>(x: &'a str) -> &'a str { x }\n",
            "    'outer: loop { break 'outer; }\n",
            "    let ready = brace_free(); // identifiers starting r/b/c\n",
            "    INSIDE();\n",
            "}\n",
            "fn later() { AFTER(); }\n",
        );
        let body = fn_body(text, "fn target(").expect("target has a body");
        assert!(body.contains("INSIDE();"), "stopped early: {body:?}");
        assert!(!body.contains("AFTER"), "ran on past the end: {body:?}");
    }

    #[test]
    fn fn_body_is_none_without_a_body() {
        assert_eq!(fn_body("fn a() {}\n", "fn missing("), None);
        // A declaration with no body (trait method, extern block item).
        assert_eq!(
            fn_body("fn decl(x: u8) -> u8;\nfn b() {}\n", "fn decl("),
            None
        );
        // Unbalanced.
        assert_eq!(fn_body("fn open() {\n    {\n", "fn open("), None);
        // A `;` inside the signature is not a declaration's end.
        assert_eq!(
            fn_body("fn arr(x: [u8; 4]) -> [u8; 4] { x }", "fn arr("),
            Some("x: [u8; 4]) -> [u8; 4] { x }")
        );
        // Braces inside the signature (const-generic expressions) are not the
        // body.
        assert_eq!(
            fn_body(
                "fn g<const N: usize>(a: [u8; { N }]) -> Foo<{ N + 1 }> where T: Fn() -> u8 { BODY }\nfn later() {}",
                "fn g<"
            ),
            Some("const N: usize>(a: [u8; { N }]) -> Foo<{ N + 1 }> where T: Fn() -> u8 { BODY }")
        );
        // A struct pattern among the parameters is not the body — the
        // signature's own `(` is already consumed when scanning starts.
        assert_eq!(
            fn_body("fn f(Foo { a, b }: Foo) -> u8 { BODY }", "fn f("),
            Some("Foo { a, b }: Foo) -> u8 { BODY }")
        );
        assert_eq!(
            fn_body("fn m(&self, Foo { a }: Foo) { BODY }", "fn m("),
            Some("&self, Foo { a }: Foo) { BODY }")
        );
        // Signature is excluded; the body begins right after it.
        assert_eq!(fn_body("fn f(a: u8) { a }", "fn f("), Some("a: u8) { a }"));
    }
}

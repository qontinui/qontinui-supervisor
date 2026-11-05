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

/// What opens a test-only module, past the newline before it.
const TEST_MODULE: &str = "#[cfg(test)]\nmod ";

/// The production part of one LF-normalized source file: everything before
/// its test modules, and NOTHING for a file that is test code in its entirety
/// ([`is_test_only_file`]).
///
/// The cut is anchored on the test MODULE, not the bare attribute:
/// `#[cfg(test)]` also decorates test-only helper items, and cutting at the
/// first of those hides every production function after it (see
/// `routes::runners::tests::scan_production_lines`). A module extracted to its
/// own file leaves `#[cfg(test)]\nmod tests;` in the parent, so the cut lands
/// in the same place before and after the extraction — which is what lets a
/// self-scan of the parent read identical production text either way.
///
/// The same goes for a module DECLARATION with production after it: a crate root
/// lists its modules first, `#[cfg(test)]\nmod source_scan;` among them, and
/// cutting there hid all of `main()` from every guard. So a declaration ends
/// production only when nothing but comments and more test modules follow it.
/// Anything this does not recognise after one stays in scope, the conservative
/// direction for a guard policing production.
///
/// An inline test module does end production, so a file with production after
/// one (no file here has) would lose it. So does a declaration followed only by
/// test modules, inline ones included. A `#[cfg(test)]\npub(crate) mod` is not
/// recognised at all and stays in scope.
pub(crate) fn production_span(text: &str) -> &str {
    if is_test_only_file(text) {
        return "";
    }
    let marker = format!("\n{TEST_MODULE}");
    let mut searched = 0;
    while let Some(found) = text[searched..].find(&marker) {
        let at = searched + found;
        let module = at + marker.len();
        match after_declaration(&text[module..]) {
            Some(rest) if !holds_only_test_modules(rest) => searched = module,
            _ => return &text[..at],
        }
    }
    text
}

/// `module` starts at a module's name. What follows it when it is a bodiless
/// `name;` declaration, and `None` when it has a body.
fn after_declaration(module: &str) -> Option<&str> {
    let end = module.find([';', '{'])?;
    module[end..].strip_prefix(';')
}

/// Whether `text`, what follows a test-only module declaration, holds nothing
/// but comments and more test modules.
fn holds_only_test_modules(mut text: &str) -> bool {
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return true;
        }
        if let Some(comment) = text.strip_prefix("//") {
            text = comment.split_once('\n').map_or("", |(_, next)| next);
            continue;
        }
        let Some(module) = text.strip_prefix(TEST_MODULE) else {
            return false;
        };
        match after_declaration(module) {
            Some(rest) => text = rest,
            // An inline test module: `production_span` cuts at one regardless.
            None => return true,
        }
    }
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

/// What a [`Token`] is. Comments are not tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// An identifier or a keyword. A raw identifier's `r#` is dropped.
    Ident,
    /// A lifetime or a loop label: `'a`, `'static`.
    Lifetime,
    /// A string literal in any spelling: `"…"`, `r#"…"#`, `b"…"`, `br"…"`, `c"…"`.
    Str,
    /// A char or byte literal: `'x'`, `'\n'`, `b'x'`.
    Char,
    /// A numeric literal, its suffix included: `0`, `1_000u16`, `0x1F`, `1.5`.
    Number,
    /// Punctuation. A multi-character operator (`::`, `->`, `=>`, `==`, `..`, …)
    /// is one token.
    Punct,
    /// `(`, `[` or `{`.
    Open,
    /// `)`, `]` or `}`.
    Close,
}

/// One lexeme of Rust source and the 1-based line it starts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token<'t> {
    pub(crate) kind: TokenKind,
    pub(crate) text: &'t str,
    pub(crate) line: usize,
}

/// Multi-character operators, longest first so the first match is the longest.
const MULTI_PUNCT: [&str; 24] = [
    "<<=", ">>=", "...", "..=", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "..", "+=",
    "-=", "*=", "/=", "%=", "^=", "&=", "|=", "<<", ">>",
];

/// `text` as tokens, comments dropped, with the same rules [`fn_body`] uses for
/// comments, strings, chars and lifetimes. `None` when a comment or a literal
/// never closes, which rustc would reject too.
pub(crate) fn tokens(text: &str) -> Option<Vec<Token<'_>>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let kind = match bytes[i] {
            b'\n' => {
                line += 1;
                i += 1;
                continue;
            }
            b if b.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = text[i..].find('\n').map_or(bytes.len(), |n| i + n);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = text.len() - skip_block_comment(&text[i..])?.len();
                line += text[start..i].matches('\n').count();
                continue;
            }
            b'"' => {
                i = skip_string(bytes, i + 1)?;
                TokenKind::Str
            }
            b'r' | b'b' | b'c' if !is_ident_byte(bytes, i.wrapping_sub(1)) => {
                if let Some(end) = raw_or_prefixed_string(bytes, i) {
                    i = end?;
                    TokenKind::Str
                } else if bytes[i] == b'b' && bytes.get(i + 1) == Some(&b'\'') {
                    i = skip_char_or_lifetime(text, i + 1);
                    TokenKind::Char
                } else if bytes[i] == b'r'
                    && bytes.get(i + 1) == Some(&b'#')
                    && is_ident_start(bytes.get(i + 2).copied())
                {
                    // A raw identifier: the token is the name after `r#`.
                    let name = i + 2;
                    i = ident_end(bytes, name);
                    out.push(Token {
                        kind: TokenKind::Ident,
                        text: &text[name..i],
                        line,
                    });
                    continue;
                } else {
                    i = ident_end(bytes, i);
                    TokenKind::Ident
                }
            }
            b'\'' => {
                let end = skip_char_or_lifetime(text, i);
                if end == i + 1 {
                    // `'` of a lifetime or label: the token runs over its name.
                    i = ident_end(bytes, i + 1);
                    TokenKind::Lifetime
                } else {
                    i = end;
                    TokenKind::Char
                }
            }
            b if is_ident_start(Some(b)) => {
                i = ident_end(bytes, i);
                TokenKind::Ident
            }
            b if b.is_ascii_digit() => {
                i = ident_end(bytes, i);
                // A fraction, but not the `..` of a range (`0..10`).
                if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    i = ident_end(bytes, i + 1);
                }
                TokenKind::Number
            }
            b'(' | b'[' | b'{' => {
                i += 1;
                TokenKind::Open
            }
            b')' | b']' | b'}' => {
                i += 1;
                TokenKind::Close
            }
            _ => {
                let rest = &text[i..];
                let len = MULTI_PUNCT
                    .iter()
                    .find(|op| rest.starts_with(**op))
                    .map_or_else(
                        || rest.chars().next().map_or(1, char::len_utf8),
                        |op| op.len(),
                    );
                i += len;
                TokenKind::Punct
            }
        };
        out.push(Token {
            kind,
            text: &text[start..i],
            line,
        });
        line += text[start..i].matches('\n').count();
    }
    Some(out)
}

/// Whether `b` can start an identifier (non-ASCII bytes are taken as part of one).
fn is_ident_start(b: Option<u8>) -> bool {
    b.is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b >= 0x80)
}

/// The index just past the identifier-like run starting at `i`.
fn ident_end(bytes: &[u8], mut i: usize) -> usize {
    while is_ident_byte(bytes, i) {
        i += 1;
    }
    i
}

/// A token, or a delimited group of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TokenTree<'t> {
    Leaf(Token<'t>),
    Group {
        open: Token<'t>,
        close: Token<'t>,
        inner: Vec<TokenTree<'t>>,
    },
}

impl<'t> TokenTree<'t> {
    /// The group's opening delimiter (`b'('`, `b'['` or `b'{'`), `None` for a leaf.
    pub(crate) fn delimiter(&self) -> Option<u8> {
        match self {
            TokenTree::Group { open, .. } => open.text.bytes().next(),
            TokenTree::Leaf(_) => None,
        }
    }

    /// The group's contents; empty for a leaf.
    pub(crate) fn inner(&self) -> &[TokenTree<'t>] {
        match self {
            TokenTree::Group { inner, .. } => inner,
            TokenTree::Leaf(_) => &[],
        }
    }

    /// The leaf token, `None` for a group.
    pub(crate) fn leaf(&self) -> Option<&Token<'t>> {
        match self {
            TokenTree::Leaf(token) => Some(token),
            TokenTree::Group { .. } => None,
        }
    }

    /// Whether this is the identifier or keyword `name`.
    pub(crate) fn is_ident(&self, name: &str) -> bool {
        self.leaf()
            .is_some_and(|t| t.kind == TokenKind::Ident && t.text == name)
    }

    /// Whether this is the punctuation `op`.
    pub(crate) fn is_punct(&self, op: &str) -> bool {
        self.leaf()
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == op)
    }

    /// The line the tree starts on.
    pub(crate) fn line(&self) -> usize {
        match self {
            TokenTree::Leaf(token) => token.line,
            TokenTree::Group { open, .. } => open.line,
        }
    }

    /// The line the tree ends on.
    pub(crate) fn end_line(&self) -> usize {
        match self {
            TokenTree::Leaf(token) => token.line,
            TokenTree::Group { close, .. } => close.line,
        }
    }
}

/// `tokens` nested by their delimiters. `None` when they do not balance.
pub(crate) fn token_trees<'t>(tokens: &[Token<'t>]) -> Option<Vec<TokenTree<'t>>> {
    let mut stack: Vec<(Token<'t>, Vec<TokenTree<'t>>)> = Vec::new();
    let mut top: Vec<TokenTree<'t>> = Vec::new();
    for token in tokens {
        match token.kind {
            TokenKind::Open => stack.push((*token, std::mem::take(&mut top))),
            TokenKind::Close => {
                let (open, outer) = stack.pop()?;
                let pair = (open.text.as_bytes()[0], token.text.as_bytes()[0]);
                if !matches!(pair, (b'(', b')') | (b'[', b']') | (b'{', b'}')) {
                    return None;
                }
                let inner = std::mem::replace(&mut top, outer);
                top.push(TokenTree::Group {
                    open,
                    close: *token,
                    inner,
                });
            }
            _ => top.push(TokenTree::Leaf(*token)),
        }
    }
    stack.is_empty().then_some(top)
}

#[cfg(test)]
mod tests {
    use super::{fn_body, is_test_only_file, production_span, token_trees, tokens, TokenKind};

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

    /// A crate root declares test-only modules among the rest, ahead of its
    /// production code. Cutting at the first of those hid all of `main()`.
    #[test]
    fn production_span_keeps_what_follows_a_test_only_module_declaration() {
        let root = "mod a;\n#[cfg(test)]\nmod scan;\nmod b;\n\nfn main() {}\n";
        assert_eq!(production_span(root), root);

        // Its inline tests still end production, and so do trailing declarations
        // with only comments and more test modules after them.
        let inline = format!("{root}\n#[cfg(test)]\nmod tests {{\n    fn t() {{}}\n}}\n");
        assert_eq!(production_span(&inline), root);
        let trailing = format!(
            "{root}\n#[cfg(test)]\nmod one;\n\n// why\n#[cfg(test)]\nmod two;\n#[cfg(test)]\nmod tests {{}}\n"
        );
        assert_eq!(production_span(&trailing), root);

        // What it cannot read after a declaration stays in scope.
        let unread = format!("{root}\n#[cfg(test)]\nmod one;\n/* a */\nfn real() {{}}\n");
        assert_eq!(production_span(&unread), unread);
    }

    /// The crate root this scanner polices: `main()` is production.
    #[test]
    fn the_production_span_of_main_rs_reaches_main() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let span = production_span(&src);
        assert!(span.contains("async fn main("), "main() is out of scope");
        assert!(
            !span.contains("primary_to_boot_start(false"),
            "main.rs's inline tests are in scope"
        );
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

    fn token_texts(src: &str) -> Vec<(TokenKind, &str)> {
        tokens(src)
            .expect("the source lexes")
            .into_iter()
            .map(|t| (t.kind, t.text))
            .collect()
    }

    #[test]
    fn tokens_drop_comments_and_keep_each_literal_whole() {
        use TokenKind::{Char, Close, Ident, Lifetime, Open, Punct, Str};
        assert_eq!(
            token_texts(
                "let s = \"a // b { \"; // c }\n/* d /* e */ { */ let r = r#\"x \" }\"#;\n'a: loop { '}' }"
            ),
            [
                (Ident, "let"),
                (Ident, "s"),
                (Punct, "="),
                (Str, "\"a // b { \""),
                (Punct, ";"),
                (Ident, "let"),
                (Ident, "r"),
                (Punct, "="),
                (Str, "r#\"x \" }\"#"),
                (Punct, ";"),
                (Lifetime, "'a"),
                (Punct, ":"),
                (Ident, "loop"),
                (Open, "{"),
                (Char, "'}'"),
                (Close, "}"),
            ]
        );
    }

    #[test]
    fn tokens_join_operators_and_split_a_range_from_its_ends() {
        use TokenKind::{Ident, Number, Punct};
        assert_eq!(
            token_texts("a::b -> c => d == e != f <= g >= h && i || j ..= k"),
            [
                (Ident, "a"),
                (Punct, "::"),
                (Ident, "b"),
                (Punct, "->"),
                (Ident, "c"),
                (Punct, "=>"),
                (Ident, "d"),
                (Punct, "=="),
                (Ident, "e"),
                (Punct, "!="),
                (Ident, "f"),
                (Punct, "<="),
                (Ident, "g"),
                (Punct, ">="),
                (Ident, "h"),
                (Punct, "&&"),
                (Ident, "i"),
                (Punct, "||"),
                (Ident, "j"),
                (Punct, "..="),
                (Ident, "k"),
            ]
        );
        assert_eq!(
            token_texts("0..1_000u16 1.5 0x1F"),
            [
                (Number, "0"),
                (Punct, ".."),
                (Number, "1_000u16"),
                (Number, "1.5"),
                (Number, "0x1F"),
            ]
        );
    }

    #[test]
    fn tokens_read_raw_identifiers_byte_chars_and_lifetimes() {
        use TokenKind::{Char, Ident, Lifetime, Str};
        assert_eq!(
            token_texts("r#type b'x' b\"y\" br#\"z\"# c\"w\" 'static '\\'' '\\u{7d}' ready"),
            [
                (Ident, "type"),
                (Char, "b'x'"),
                (Str, "b\"y\""),
                (Str, "br#\"z\"#"),
                (Str, "c\"w\""),
                (Lifetime, "'static"),
                (Char, "'\\''"),
                (Char, "'\\u{7d}'"),
                (Ident, "ready"),
            ]
        );
    }

    #[test]
    fn a_token_carries_the_line_it_starts_on() {
        let lines: Vec<(&str, usize)> = tokens("a\n/* x\ny */ b\n\"m\nn\" c\r\nd")
            .expect("lexes")
            .into_iter()
            .map(|t| (t.text, t.line))
            .collect();
        assert_eq!(
            lines,
            [("a", 1), ("b", 3), ("\"m\nn\"", 4), ("c", 5), ("d", 6)]
        );
    }

    #[test]
    fn an_unterminated_literal_or_comment_does_not_lex() {
        assert_eq!(tokens("let s = \"open"), None);
        assert_eq!(tokens("/* open /* nested */"), None);
        assert_eq!(tokens("r#\"open\""), None);
    }

    #[test]
    fn token_trees_nest_by_delimiter_and_refuse_a_mismatch() {
        let toks = tokens("f(a, [b], {c})").expect("lexes");
        let trees = token_trees(&toks).expect("balanced");
        assert_eq!(trees.len(), 2);
        assert!(trees[0].is_ident("f"));
        assert_eq!(trees[1].delimiter(), Some(b'('));
        let inner = trees[1].inner();
        assert_eq!(inner.len(), 5);
        assert!(inner[1].is_punct(","));
        assert_eq!(inner[2].delimiter(), Some(b'['));
        assert!(inner[2].inner()[0].is_ident("b"));
        assert_eq!(inner[4].delimiter(), Some(b'{'));
        // A group spans from its opening line to its closing one.
        let spanning = tokens("f(\n    a,\n)").expect("lexes");
        let trees = token_trees(&spanning).expect("balanced");
        assert_eq!((trees[0].line(), trees[0].end_line()), (1, 1));
        assert_eq!((trees[1].line(), trees[1].end_line()), (1, 3));
        for unbalanced in ["(]", "(", ")", "{[}]"] {
            let toks = tokens(unbalanced).expect("lexes");
            assert_eq!(token_trees(&toks), None, "{unbalanced}");
        }
    }
}

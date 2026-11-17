//! Regression guard: the supervisor never addresses a runner, or itself, as
//! `localhost`.
//!
//! ## Why this test exists
//!
//! The supervisor binds the IPv4 loopback only (`main.rs`, on
//! `loopback::LOOPBACK_IP`, through `socket2::Domain::IPV4`). So does every
//! runner it dials: the primary and every temp or named runner bind
//! qontinui-runner's `mcp::types::RUNNER_API_BIND_IP` the same way. Windows
//! resolves `localhost` to `::1` first, so a client that dials
//! `http://localhost:<port>` first tries a socket nobody listens on.
//! qontinui-runner measured the cost per NEW connection on 2026-10-25
//! (`src-tauri/tests/runner_self_urls_use_ipv4_loopback.rs`):
//!
//! | client | penalty per new connection |
//! |---|---|
//! | reqwest, fresh `Client` | ~+305 ms |
//! | curl | ~+205 ms |
//! | Python `urllib`, PowerShell 5.1 | ~+2.0 s |
//!
//! The supervisor is also where agents get their runner URLs. `/ui-bridge`
//! tells them to drive the `ui_bridge_url` that `POST /runners/spawn-test`
//! returns, and the spawn-named and `GET /runners/by-unit/{unit_id}` responses
//! carry the same field. Plan
//! `2026-11-11-supervisor-and-temp-runner-probes-still-dial-localhost`, Phase 2,
//! routed every such URL through `src/loopback.rs`. This test keeps a NEW call
//! site from spelling `localhost` again. The helpers are the control; this is
//! the guard.
//!
//! ## What it flags
//!
//! `localhost:` in any case, after any scheme or none, followed by EITHER a
//! port 9875–9899 that no further digit follows, OR an interpolation opener
//! (`{`, `$`, `%`), whose port a lexical check cannot know. 9875 is this
//! supervisor, 9876 the primary runner, and 9877–9899 the primary's bind
//! fallbacks and the temp and named runners. This is qontinui-runner's
//! `forbidden_at` with its range widened from 9876 down to 9875, which is the
//! port set qontinui-claude-config's check #14 is being widened to (plan
//! 2026-11-11-supervisor-and-temp-runner-probes-still-dial-localhost Phase 1).
//!
//! It reads every `.rs` file under `src/`, and production code only:
//!
//! - Comments are not scanned. A small lexer separates code and string text
//!   from comment text.
//! - Whatever `#[cfg(test)]` (or `#[cfg(all(…, test, …))]`) gates is skipped,
//!   bounded to its own end, so a test-only helper, field, variant or match arm
//!   does not hide the production code after it. An item (`fn`, `mod`, `impl`,
//!   `use`, …) ends at a `;` or at the `}` that closes its body. A field, a
//!   variant, a match arm or a parameter also ends at its `,`. Those separators
//!   count only outside `(…)`, `[…]` and `{…}`, so the `;` in `[u8; 4]` or the
//!   `,` in `(a, b)` ends nothing. `union` and `macro_rules` are also legal
//!   field names, so they start an item only when a name follows (`union U`)
//!   or a `!` does (`macro_rules! m`). A gated `union: 1,` is a field.
//! - A whole file is skipped when it opens with `#![cfg(test)]`, or when EVERY
//!   `mod name;` declaration that resolves to it is gated that way, and the
//!   second case FAILS the test unless the file also carries the header. The
//!   rule is "every" because this crate has two roots. `main.rs` gates
//!   `mod test_clock;` while `lib.rs` declares `pub mod test_clock;`, so that
//!   file is compiled into production and stays in scope.
//!
//! ## Why it does not reuse `src/source_scan.rs`
//!
//! The crate already decides what is production for its in-crate source scans,
//! in `src/source_scan.rs`. This test cannot call it. Both crate roots declare
//! that module `#[cfg(test)] mod source_scan;` and its items are `pub(crate)`,
//! while a file under `tests/` links the library built without `cfg(test)`, so
//! the module does not exist for it. The rules here are this test's own, and
//! they differ in three ways:
//!
//! - **Test code inside a file.** `source_scan::production_span` cuts at an
//!   inline test module, or at a bodiless `#[cfg(test)] mod x;` that only
//!   comments and test modules follow, so a gated helper above the cut reads
//!   as production and everything below it as test. This test bounds each gated item, field, variant or arm on its own
//!   and scans everything outside them, below a test module included.
//! - **Test-only files.** The file's own `#![cfg(test)]` header is the one
//!   rule both classifiers decide on: `source_scan::is_test_only_file` keys on
//!   it, and so does this test. This test also derives the files whose every
//!   `mod name;` declaration is gated, as qontinui-runner's guard does, but
//!   only to hold them to that rule: such a file without the header is a
//!   failure here ([`headerless_test_only_files`]), naming the file and the fix,
//!   because `source_scan` would read it as production while this test skipped
//!   it. It is still skipped for the localhost scan. `src/test_env.rs` was the
//!   last such file and gained its header on 2026-11-17; `source_scan.rs`,
//!   `test_ports.rs` and `dev_action/test_support.rs` gained theirs on
//!   2026-11-15. qontinui-claude-config's `scripts/file-size-census.py` lists
//!   such files as `missing-header`.
//! - **A byte-order mark.** `source_scan` strips a leading one. This test does
//!   not, so a `#![cfg(test)]` after a BOM marks nothing and the file stays in
//!   scope, the conservative direction for this guard.
//!
//! So the two can disagree about which lines are production. Moving this guard
//! in-crate, as a `#[cfg(test)]` module under `src/`, would let both share one
//! classifier.
//!
//! ## The one exemption
//!
//! A deliberate `localhost:` form is exempt when a COMMENT carries
//! `runner-localhost-ok:` followed by a reason, which is any non-blank text
//! inside that same comment. That is a URL that targets neither the supervisor
//! nor a runner, or text that nothing dials, such as the retired prompt text
//! `src/evaluation/db.rs` matches in order to replace it. The comment must be
//! on the same line or on a comment-only line immediately above. A bare
//! `runner-localhost-ok`, or one with nothing after its colon, exempts
//! nothing, and neither does the token inside a string literal.
//!
//! qontinui-claude-config check #14 and qontinui-runner use the same marker,
//! so the fleet has one spelling for "this localhost is deliberate". Neither
//! checked for the reason when this test began to (2026-11-15), so a bare
//! marker that they accept fails here.
//!
//! ## What it does NOT claim
//!
//! It is lexical. A URL assembled by concatenation (`"http://localhost:" + p`)
//! or split by a string continuation escapes it, and its comment and string
//! tracking is a small hand lexer, not a parser. It reads `src/` only:
//! `frontend/` (the Vite dev proxy), `scripts/` and `CLAUDE.md` are outside it.
//! A green run proves only that no line it SCANNED carries a flagged form. The
//! `scanned=` count it prints, the floor on it, and the refusal to bound a
//! gated item that never closes are what tell "scanned nothing" apart from
//! "found nothing".
//!
//! Its test regions are whole lines, bounded without a parser:
//!
//! - Production code that shares a line with the end of a gated item is
//!   skipped with it, a layout rustfmt does not produce for an attribute that
//!   starts its line.
//! - A gated item whose signature opens a `{` outside `(…)` and `[…]` (a const
//!   generic `-> Foo<{ N }>`) ends there, and so does a gated arm whose first
//!   line closes a `{…}` before its body (`Foo { a } =>`, body on the next
//!   line). The rest is scanned as production: the noisy direction, never the
//!   silent one.
//!
//! ## Fixing a failure
//!
//! For a runner, use `loopback::runner_loopback_base(port)` or
//! `loopback::runner_ui_bridge_base(port)` (`crate::loopback::…` inside the
//! crate). For the supervisor itself, use
//! `loopback::loopback_base(state.config.port)`. For a bind, use
//! `loopback::LOOPBACK_IP`. In prompt or message text, spell
//! `127.0.0.1:<port>`. For a URL that is not a supervisor or a runner at all,
//! add `// runner-localhost-ok: <what it targets>`. The reason after the colon
//! is required.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The exemption token, qontinui-claude-config check #14's marker. It exempts
/// only with a reason after its colon ([`exempts`]).
const MARKER: &str = "runner-localhost-ok";

/// The ports this guard flags: the supervisor (9875) through the last temp
/// runner port (9899).
const FLAGGED_PORTS: std::ops::RangeInclusive<u32> = 9875..=9899;

/// A floor on production files scanned, so that a moved `src/` or a broken walk
/// cannot pass vacuously. On 2026-11-11, 124 of the 131 `.rs` files under
/// `src/` were scanned; the other 7 are test-only.
const MIN_FILES_SCANNED: usize = 100;

/// Is there a flagged `localhost:` form in `text`? Returns the byte column of
/// the first one.
fn forbidden_at(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let needle = "localhost:";
    let mut from = 0;
    while let Some(rel) = lower[from..].find(needle) {
        let at = from + rel;
        let after = &lower.as_bytes()[at + needle.len()..];
        let flagged = match after.first() {
            Some(b'{' | b'$' | b'%') => true,
            Some(b) if b.is_ascii_digit() => {
                let digits = after.iter().take_while(|d| d.is_ascii_digit()).count();
                digits == 4
                    && std::str::from_utf8(&after[..4])
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .is_some_and(|port| FLAGGED_PORTS.contains(&port))
            }
            _ => false,
        };
        if flagged {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// One source line, split by [`lex`].
struct LexedLine {
    /// Code and string contents. This is what the matcher reads, since URLs
    /// live in strings.
    scanned: String,
    /// Comment text only. The exemption marker must sit here.
    comment: String,
    /// Code outside strings, chars and comments. Attributes, braces and `mod`
    /// declarations are read from this.
    code: String,
}

#[derive(Clone, Copy)]
enum State {
    Code,
    /// Inside `/* … */`, nested this deep (Rust block comments nest).
    Block(usize),
    /// Inside a string literal: `Some(n)` for a raw string that `"` plus `n`
    /// `#`s closes, `None` for one with escapes.
    Str {
        raw_hashes: Option<usize>,
    },
}

/// The bytes of one line, sorted by [`lex`] as it reads them.
#[derive(Default)]
struct LineParts {
    scanned: Vec<u8>,
    comment: Vec<u8>,
    code: Vec<u8>,
}

impl LineParts {
    fn into_line(self) -> LexedLine {
        let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
        LexedLine {
            scanned: text(self.scanned),
            comment: text(self.comment),
            code: text(self.code),
        }
    }
}

/// Split Rust source into [`LexedLine`]s. Strings and block comments carry
/// across lines. Each step reads one token, or one byte of one, in the current
/// [`State`] and returns how many bytes it took and the state after them.
///
/// Also returns whether the text ENDS in code. Valid Rust cannot end inside a
/// string or a block comment, so a file that does means the lexer lost sync,
/// and every line after the slip would read as unscanned comment or string.
fn lex(text: &str) -> (Vec<LexedLine>, bool) {
    let mut state = State::Code;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = LineParts::default();
        let mut i = 0;
        while i < line.len() {
            let rest = &line.as_bytes()[i..];
            let (taken, next) = match state {
                State::Code => code_step(line, i, &mut parts),
                State::Block(depth) => block_step(rest, depth, &mut parts),
                State::Str { raw_hashes } => string_step(rest, raw_hashes, &mut parts),
            };
            i += taken;
            state = next;
        }
        out.push(parts.into_line());
    }
    (out, matches!(state, State::Code))
}

/// One step in code, where a comment, a string or a char literal can open.
fn code_step(line: &str, i: usize, parts: &mut LineParts) -> (usize, State) {
    let b = line.as_bytes();
    let rest = &b[i..];
    if rest.starts_with(b"//") {
        parts.comment.extend_from_slice(rest);
        return (rest.len(), State::Code);
    }
    if rest.starts_with(b"/*") {
        parts.comment.extend_from_slice(b"/*");
        return (2, State::Block(1));
    }
    if let Some((len, hashes)) = raw_string_open(b, i) {
        parts.scanned.extend_from_slice(&rest[..len]);
        let raw_hashes = Some(hashes);
        return (len, State::Str { raw_hashes });
    }
    match b[i] {
        b'"' => {
            parts.scanned.push(b'"');
            (1, State::Str { raw_hashes: None })
        }
        // A char literal is not code, so its `{` or `"` must not count.
        b'\'' => match char_literal_end(line, i) {
            Some(end) => {
                parts.scanned.extend_from_slice(&b[i..end]);
                (end - i, State::Code)
            }
            // A lifetime or a label.
            None => {
                parts.scanned.push(b'\'');
                parts.code.push(b'\'');
                (1, State::Code)
            }
        },
        c => {
            parts.scanned.push(c);
            parts.code.push(c);
            (1, State::Code)
        }
    }
}

/// One step inside a block comment nested `depth` deep.
fn block_step(rest: &[u8], depth: usize, parts: &mut LineParts) -> (usize, State) {
    let (taken, next) = if rest.starts_with(b"/*") {
        (2, State::Block(depth + 1))
    } else if rest.starts_with(b"*/") {
        let outer = if depth == 1 {
            State::Code
        } else {
            State::Block(depth - 1)
        };
        (2, outer)
    } else {
        (1, State::Block(depth))
    };
    parts.comment.extend_from_slice(&rest[..taken]);
    (taken, next)
}

/// One step inside a string literal ([`State::Str`]).
fn string_step(rest: &[u8], raw_hashes: Option<usize>, parts: &mut LineParts) -> (usize, State) {
    let open = State::Str { raw_hashes };
    let (taken, next) = match raw_hashes {
        // An escape: keep it with the byte it escapes, so `\"` does not close
        // the string.
        None if rest[0] == b'\\' => (rest.len().min(2), open),
        None if rest[0] == b'"' => (1, State::Code),
        Some(n) if closes_raw_string(rest, n) => (1 + n, State::Code),
        _ => (1, open),
    };
    parts.scanned.extend_from_slice(&rest[..taken]);
    (taken, next)
}

/// Whether `rest` opens with the `"` and `n` `#`s that close a raw string, so
/// that a `"` inside one (`r#"say "hi"#`) does not.
fn closes_raw_string(rest: &[u8], n: usize) -> bool {
    rest[0] == b'"'
        && rest
            .get(1..=n)
            .is_some_and(|hashes| hashes.iter().all(|c| *c == b'#'))
}

/// Whether `c` can continue an identifier, so that an `r`, `b` or `c` after it
/// is part of a name and not a literal prefix.
fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// At `b[i]`: when a raw string literal opens here (`r"`, `r#"`, `br"`,
/// `cr#"`, …), the length of its opener (the quote included) and its `#`
/// count. A raw identifier (`r#type`) is not one.
fn raw_string_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && is_ident(b[i - 1]) {
        return None;
    }
    let mut j = i;
    if matches!(b.get(j), Some(b'b' | b'c')) {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = b[j..].iter().take_while(|c| **c == b'#').count();
    j += hashes;
    (b.get(j) == Some(&b'"')).then_some((j + 1 - i, hashes))
}

/// At a `'` at byte `i`: the end (exclusive) of a char literal that starts here
/// (`'x'`, `'\n'`, `'\''`, `'\u{7f}'`, `'é'`), or `None` for a lifetime or a
/// label (`'a`, `'static`).
fn char_literal_end(line: &str, i: usize) -> Option<usize> {
    let rest = &line[i + 1..];
    if let Some(escaped) = rest.strip_prefix('\\') {
        // An escape: the literal ends at the next `'` after the escaped char.
        let first = escaped.chars().next()?;
        let close = escaped[first.len_utf8()..].find('\'')?;
        return Some(i + 2 + first.len_utf8() + close + 1);
    }
    let mut chars = rest.chars();
    let c = chars.next()?;
    (chars.next() == Some('\'')).then_some(i + 1 + c.len_utf8() + 1)
}

/// The predicate of an outer `#[cfg(…)]` attribute at the start of `code`, and
/// the code after the attribute's closing `]`.
fn cfg_predicate(code: &str) -> Option<(&str, &str)> {
    let inner = code.strip_prefix("#[cfg(")?;
    let mut depth = 1usize;
    for (k, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let rest = inner[k + 1..].trim_start().strip_prefix(']')?;
                    return Some((&inner[..k], rest));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether an item gated by `cfg(pred)` exists only in test builds: `test`
/// itself, or an `all(…)` with a test-only member. Anything else
/// (`not(test)`, `any(test, unix)`, a feature) is compiled into production
/// too. That is the conservative direction for a guard, so it stays in scope.
fn cfg_requires_test(pred: &str) -> bool {
    let pred = pred.trim();
    if pred == "test" {
        return true;
    }
    let Some(args) = pred.strip_prefix("all(").and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    let mut depth = 0usize;
    let mut start = 0;
    for (k, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if cfg_requires_test(&args[start..k]) {
                    return true;
                }
                start = k + 1;
            }
            _ => {}
        }
    }
    cfg_requires_test(&args[start..])
}

/// Whether a file is test code in its entirety: its first code line is
/// `#![cfg(test)]`. Any other first item, another inner attribute included,
/// keeps the file in scope. That is the conservative direction for a guard.
fn is_test_only_file(lines: &[LexedLine]) -> bool {
    lines.iter().map(|l| l.code.trim()).find(|s| !s.is_empty()) == Some("#![cfg(test)]")
}

/// Line ranges (0-based, inclusive) of the test-only code in a file. Whatever
/// an outer test-only `#[cfg(…)]` gates (an item, a field, a variant, a match
/// arm, a parameter, a statement) is one range, bounded by [`gated_item_end`].
///
/// `Err` carries the 1-based line of a gated item whose end cannot be found.
/// Bounding it at the end of the file would silently skip the production code
/// after it, so the caller fails instead.
fn test_regions(lines: &[LexedLine]) -> Result<Vec<(usize, usize)>, usize> {
    let mut regions = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let gated = cfg_predicate(lines[i].code.trim()).is_some_and(|(p, _)| cfg_requires_test(p));
        if !gated {
            i += 1;
            continue;
        }
        let end = gated_item_end(lines, i).ok_or(i + 1)?;
        regions.push((i, end));
        i = end + 1;
    }
    Ok(regions)
}

/// The last line of what the test-only `#[cfg(…)]` on line `attr` gates, or
/// `None` when no end is found.
///
/// It counts `(`, `[` and `{` together over CODE text only, starting just after
/// the attribute. The lexer has already removed strings, chars and comments,
/// so a bracket inside a literal cannot unbalance the count. Outside every
/// bracket, the gated code ends:
///
/// - at a `;` (a `use`, a `const`, a bodiless `mod x;`, a statement);
/// - at a `,`, unless it is an item ([`gates_an_item`]). A field, a variant, a
///   match arm and a parameter end at their comma, so the siblings after them
///   stay production;
/// - at the `}` that closes a brace group it opened (a body, a block arm);
/// - on the line BEFORE a closer it did not open, which closes the enclosing
///   scope (a gated last field or arm with no trailing comma).
///
/// A `;` or `,` inside brackets ends nothing, so a gated `fn t() -> [u8; 4] {`
/// runs to its closing `}` instead of ending at its signature.
fn gated_item_end(lines: &[LexedLine], attr: usize) -> Option<usize> {
    let item = gates_an_item(lines, attr);
    let mut depth = 0usize;
    for k in attr..lines.len() {
        for c in gated_code(lines, attr, k).bytes() {
            match c {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' if depth == 0 => {
                    return Some(k.saturating_sub(1).max(attr));
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(k);
                    }
                }
                b')' | b']' => depth -= 1,
                b';' if depth == 0 => return Some(k),
                b',' if depth == 0 && !item => return Some(k),
                _ => {}
            }
        }
    }
    None
}

/// Line `k`'s code as the gate on line `attr` sees it: on the attribute's own
/// line, only what follows the attribute.
fn gated_code(lines: &[LexedLine], attr: usize, k: usize) -> &str {
    if k == attr {
        cfg_predicate(lines[k].code.trim()).map_or("", |(_, rest)| rest)
    } else {
        &lines[k].code
    }
}

/// The words that start an item, or a `let` statement, rather than a
/// comma-separated element (a field, a variant, a match arm, a parameter).
/// `const` is not one: `const N: usize` can also be a generic parameter, which
/// a `,` ends. A `const fn` is recognised by its second word. Nor are the weak
/// keywords `union` and `macro_rules`, which are also legal field and binding
/// names ([`weak_keyword_starts_an_item`]).
const ITEM_KEYWORDS: &[&str] = &[
    "async", "enum", "extern", "fn", "impl", "let", "mod", "static", "struct", "trait", "type",
    "unsafe", "use",
];

/// Whether the test-only `#[cfg(…)]` on line `attr` gates an item or a `let`
/// statement, which only a `;` or its own closing `}` ends. An item's
/// signature can hold a `,` outside every bracket (`fn f<A, B>()`, a `where`
/// clause), so a `,` must not end it the way it ends a field or a match arm.
fn gates_an_item(lines: &[LexedLine], attr: usize) -> bool {
    // The gated code can start lines below the attribute, past doc comments
    // and further attributes, so lines are joined until its first word shows.
    let mut text = String::new();
    for k in attr..lines.len() {
        text.push_str(gated_code(lines, attr, k));
        text.push('\n');
        if let Some((first, after)) = leading_word(&text) {
            let after = after.trim_start();
            return ITEM_KEYWORDS.contains(&first)
                || (first == "const" && ITEM_KEYWORDS.contains(&word(after)))
                || weak_keyword_starts_an_item(first, after);
        }
    }
    false
}

/// Whether the weak keyword `first`, with `after` following it, starts an
/// item. `union` and `macro_rules` are also legal field and binding names, and
/// a gated field `union: 1,` ends at its `,` like any other field; read as an
/// item it would run on to the enclosing `}` and hide the fields after it. As
/// keywords they are followed by a name (`union U {`) or by a `!`
/// (`macro_rules! m {`).
fn weak_keyword_starts_an_item(first: &str, after: &str) -> bool {
    match first {
        "union" => !word(after).is_empty(),
        "macro_rules" => after.starts_with('!'),
        _ => false,
    }
}

/// The first word of `code`, past any leading outer attributes (`#[…]`) and a
/// visibility (`pub`, `pub(…)`), and the code after that word. `None` while
/// `code` holds nothing past those yet. A word is a run of identifier
/// characters, so code that opens with punctuation or a literal gives an empty
/// word.
fn leading_word(code: &str) -> Option<(&str, &str)> {
    let mut rest = code.trim_start();
    loop {
        if let Some(inner) = rest.strip_prefix("#[") {
            rest = after_close(inner, b'[', b']')?.trim_start();
        } else if let Some(after) = strip_word(rest, "pub") {
            rest = after.trim_start();
            if let Some(scope) = rest.strip_prefix('(') {
                rest = after_close(scope, b'(', b')')?.trim_start();
            }
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }
    let first = word(rest);
    Some((first, &rest[first.len()..]))
}

/// The run of identifier characters `s` starts with.
fn word(s: &str) -> &str {
    let end = s.bytes().position(|c| !is_ident(c)).unwrap_or(s.len());
    &s[..end]
}

/// `s` past the whole word `w`, when `s` starts with it.
fn strip_word<'a>(s: &'a str, w: &str) -> Option<&'a str> {
    s.strip_prefix(w)
        .filter(|rest| !rest.bytes().next().is_some_and(is_ident))
}

/// `s` follows an opening `open`: what follows its matching `close`, or `None`
/// when that is not in `s`.
fn after_close(s: &str, open: u8, close: u8) -> Option<&str> {
    let mut depth = 1usize;
    for (i, c) in s.bytes().enumerate() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(&s[i + 1..]);
            }
        }
    }
    None
}

/// A `mod NAME;` declaration: the module's name, the `#[path = "…"]` it
/// carries, and whether a test-only `#[cfg(…)]` gates it.
struct ModDecl {
    name: String,
    path: Option<String>,
    gated: bool,
}

/// Every bodiless `mod NAME;` declaration in a file, with the attributes on the
/// lines (or the same line) before it. A declaration inside a string is not
/// one, since the lexer has removed it from the code text.
fn mod_declarations(lines: &[LexedLine]) -> Vec<ModDecl> {
    let mut out = Vec::new();
    let (mut gated, mut path) = (false, None);
    for line in lines {
        let mut code = line.code.trim();
        // Leading attributes apply to the next item, whichever line it starts on.
        while code.starts_with("#[") {
            if let Some((pred, rest)) = cfg_predicate(code) {
                gated |= cfg_requires_test(pred);
                code = rest.trim();
                continue;
            }
            if code.starts_with("#[path") {
                path = line.scanned.split('"').nth(1).map(str::to_string);
            }
            code = code.find(']').map_or("", |k| code[k + 1..].trim());
        }
        if code.is_empty() {
            // A blank, comment-only or attribute-only line: the run goes on.
            continue;
        }
        if let Some(name) = bodiless_mod_name(code) {
            out.push(ModDecl {
                name,
                path: path.take(),
                gated,
            });
        }
        gated = false;
        path = None;
    }
    out
}

/// The name in a `[pub[(…)]] mod NAME;` item, or `None` for any other item
/// (an inline `mod NAME { … }` included).
fn bodiless_mod_name(code: &str) -> Option<String> {
    let mut rest = code;
    if let Some(after) = rest.strip_prefix("pub") {
        rest = after.trim_start();
        if rest.starts_with('(') {
            rest = rest[rest.find(')')? + 1..].trim_start();
        }
    }
    let rest = rest.strip_prefix("mod ")?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let bodiless = rest[name.len()..].trim_start().starts_with(';');
    (!name.is_empty() && bodiless).then_some(name)
}

/// The files a `mod` declaration in `file` can resolve to.
fn declaration_targets(file: &Path, decl: &ModDecl) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    // `#[path = "…"]` on a module declared in a file is relative to that
    // file's directory.
    if let Some(rel) = &decl.path {
        return vec![dir.join(rel)];
    }
    // A crate root (`lib.rs`, `main.rs`, a `src/bin/*.rs` binary) and a
    // `mod.rs` declare their children beside themselves. Any other file
    // declares them in a directory named after itself.
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let beside = matches!(stem, "mod" | "lib" | "main") || dir.ends_with("src/bin");
    let own_dir = if beside {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    vec![
        own_dir.join(format!("{}.rs", decl.name)),
        own_dir.join(&decl.name).join("mod.rs"),
    ]
}

/// The files some `#[cfg(test)] mod NAME;` declares, minus any file that a
/// declaration WITHOUT the gate also resolves to. Such a file is compiled into
/// production through that declaration.
///
/// Derived from the declarations rather than kept as a hand list, so a new
/// test-module file needs no edit here, and no production file can be waved
/// through by its name.
fn test_only_files(sources: &[(PathBuf, String)]) -> HashSet<PathBuf> {
    let (mut gated, mut ungated) = (HashSet::new(), HashSet::new());
    for (file, text) in sources {
        for decl in mod_declarations(&lex(text).0) {
            let set = if decl.gated { &mut gated } else { &mut ungated };
            set.extend(declaration_targets(file, &decl));
        }
    }
    gated.difference(&ungated).cloned().collect()
}

/// Each of `declared` (the files [`test_only_files`] derived) that is in
/// `sources` and does NOT open with `#![cfg(test)]`, in `sources` order.
///
/// Such a file is test-only only by its declarations, so this test would skip
/// it while `source_scan::is_test_only_file`, which reads the header alone,
/// would read it as production. Returning it lets the caller fail on it, so
/// the header stays the one rule both classifiers decide on.
fn headerless_test_only_files(
    sources: &[(PathBuf, String)],
    declared: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    sources
        .iter()
        .filter(|(path, text)| declared.contains(path) && !is_test_only_file(&lex(text).0))
        .map(|(path, _)| path.clone())
        .collect()
}

/// One flagged line.
struct Hit {
    rel: String,
    line: usize,
    text: String,
}

/// What [`scan_text`] read of one file.
enum Scanned {
    /// The file is test code in its entirety ([`is_test_only_file`]).
    TestOnly,
    /// This many production lines were scanned.
    Lines(usize),
}

/// Scan one file's production lines, pushing each flagged line onto `hits`.
///
/// Panics on a `#[cfg(test)]` item whose end the region detector cannot find,
/// rather than skip everything after it.
fn scan_text(rel: &str, text: &str, hits: &mut Vec<Hit>) -> Scanned {
    let (lines, ends_in_code) = lex(text);
    assert!(
        ends_in_code,
        "{rel}: the scanner ends this file inside a string or a block comment, which valid \
         Rust cannot do, so it lost sync and would read every line after the slip as \
         unscanned. Fix the lexer in this test, not the source."
    );
    if is_test_only_file(&lines) {
        return Scanned::TestOnly;
    }
    let regions = test_regions(&lines).unwrap_or_else(|line| {
        panic!(
            "{rel}:{line}: this #[cfg(test)] item never closes as the scanner reads it, so \
             it cannot be bounded. Skipping to the end of the file would hide every production \
             line after it. Fix the lexer in this test, not the source."
        )
    });
    let mut scanned = 0;
    for (idx, ll) in lines.iter().enumerate() {
        if regions.iter().any(|(a, b)| (*a..=*b).contains(&idx)) {
            continue;
        }
        scanned += 1;
        if forbidden_at(&ll.scanned).is_none() {
            continue;
        }
        let exempt_here = exempts(&ll.comment);
        // The line above exempts only when it is a COMMENT-ONLY line.
        let exempt_above =
            idx > 0 && exempts(&lines[idx - 1].comment) && lines[idx - 1].scanned.trim().is_empty();
        if exempt_here || exempt_above {
            continue;
        }
        hits.push(Hit {
            rel: rel.to_string(),
            line: idx + 1,
            text: text.lines().nth(idx).unwrap_or("").trim().to_string(),
        });
    }
    Scanned::Lines(scanned)
}

/// Whether one line's comment text carries the exemption: [`MARKER`], a `:`,
/// and then a reason, which is any non-blank text inside the same comment. A
/// bare marker, or one with nothing after its colon, gives no reason.
fn exempts(comment: &str) -> bool {
    let tagged = format!("{MARKER}:");
    comment.match_indices(tagged.as_str()).any(|(at, _)| {
        let after = &comment[at + tagged.len()..];
        // A block comment's reason ends at its `*/`.
        let reason = after.split_once("*/").map_or(after, |(reason, _)| reason);
        !reason.trim().is_empty()
    })
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    // A directory that cannot be read fails the guard, rather than shrinking
    // it into a no-op. That holds for each entry too: `.flatten()` here, or
    // `Path::is_dir`, would drop an entry or a subdirectory that errors.
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => panic!("read_dir {}: an unreadable entry: {e}", dir.display()),
        };
        let meta = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("metadata {} (in {}): {e}", path.display(), dir.display()));
        if meta.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_supervisor_url_spells_localhost() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&base.join("src"), &mut files);
    files.sort();
    let sources: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", p.display()));
            (p, text)
        })
        .collect();
    let declared_test_only = test_only_files(&sources);
    let headerless = headerless_test_only_files(&sources, &declared_test_only);

    let mut hits = Vec::new();
    let mut header_failures = Vec::new();
    let (mut files_scanned, mut lines_scanned, mut skipped_test) = (0usize, 0usize, 0usize);
    for (path, text) in &sources {
        let rel = path
            .strip_prefix(&base)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.display().to_string());
        if declared_test_only.contains(path) {
            if headerless.contains(path) {
                header_failures.push(format!(
                    "  {rel}: reached only through `#[cfg(test)] mod` declarations, but has no \
                     `#![cfg(test)]` header, so `source_scan` reads it as production. Fix: add \
                     `#![cfg(test)]` as its first item."
                ));
            }
            skipped_test += 1;
            continue;
        }
        match scan_text(&rel, text, &mut hits) {
            Scanned::TestOnly => skipped_test += 1,
            Scanned::Lines(n) => {
                files_scanned += 1;
                lines_scanned += n;
            }
        }
    }
    println!(
        "root=src scanned={files_scanned} lines={lines_scanned} skipped_test={skipped_test} \
         min_files={MIN_FILES_SCANNED}"
    );

    assert!(
        files_scanned >= MIN_FILES_SCANNED,
        "scanned only {files_scanned} production files under src/ (floor {MIN_FILES_SCANNED}): \
         the directory moved or the walk broke, so a green run here would prove nothing"
    );

    assert!(
        header_failures.is_empty(),
        "{} test-only file(s) lack the `#![cfg(test)]` header, the one rule this test and \
         `src/source_scan.rs` both decide on:\n{}",
        header_failures.len(),
        header_failures.join("\n")
    );

    if !hits.is_empty() {
        let list: Vec<String> = hits
            .iter()
            .map(|h| format!("  {}:{}  {}", h.rel, h.line, h.text))
            .collect();
        panic!(
            "{} line(s) address the supervisor or a runner as `localhost`. Both bind 127.0.0.1 \
             only, and on Windows `localhost` tries ::1 first (+~0.3 s per new reqwest \
             connection, +~2 s for Python/PowerShell), in the supervisor and in every agent \
             that follows a URL it hands out.\n\
             Fix: `crate::loopback::runner_loopback_base(port)` / `runner_ui_bridge_base(port)` \
             for a runner, `crate::loopback::loopback_base(state.config.port)` for the \
             supervisor itself, `crate::loopback::LOOPBACK_IP` for a bind; in prompt or message \
             text spell `127.0.0.1:<port>`. A URL that is NOT the supervisor or a runner: add a \
             `// {MARKER}: <what it targets>` comment on the line or the line above; a bare \
             marker, with no reason after its colon, exempts nothing.\n{}",
            hits.len(),
            list.join("\n")
        );
    }
}

/// Whether `scan_text` flags anything in `src`.
fn flags(src: &str) -> bool {
    let mut hits = Vec::new();
    scan_text("fixture.rs", src, &mut hits);
    !hits.is_empty()
}

/// The matcher, the lexer and the test-region detector, shown failing on
/// known-bad input and passing on known-good input. A guard that can only
/// answer "fine" proves nothing.
#[test]
fn the_guard_flags_every_forbidden_form_and_only_those() {
    let bad: &[&str] = &[
        // Interpolated ports: format args, a shell variable, a Windows one.
        r#"let u = format!("http://localhost:{}", port);"#,
        r#"let u = format!("http://localhost:{port}/health");"#,
        r#"let c = "curl -s http://localhost:$PORT/health";"#,
        r#"let c = "curl -s http://localhost:%PORT%/health";"#,
        // Literal ports across the range, in any case, after any scheme or none.
        r#"let s = "http://localhost:9875/health";"#,
        r#"client.get("http://localhost:9876/ui-bridge/sdk/elements")"#,
        r#"let s = "http://localhost:9899/x";"#,
        r#"let w = "ws://localhost:9877/events";"#,
        r#"let p = "curl -s LocalHost:9878/health";"#,
        r#"let s = "LOCALHOST:9877";"#,
        // Raw, raw byte and multi-line strings.
        "let s = r#\"curl http://localhost:9876/x\"#;",
        "let s = br\"http://localhost:9876\";",
        "let s = \"multi\nline http://localhost:9876/x\";",
        // An escaped `"` does not close a string, and neither does a `"`
        // inside a raw string whose `#`s are still open. Either mistake would
        // read the URL's `//` as a comment.
        r##"let b = "{\"url\": \"http://localhost:9876\"}";"##,
        r##"let s = r#"say "hi http://localhost:9876"#;"##,
        // The marker inside a STRING does not exempt, even with a reason.
        r#"let u = "http://localhost:9876"; let n = "runner-localhost-ok: a reason";"#,
        // A `\` in a raw string is not an escape, with or without a `b`
        // prefix: read as an escape it would swallow the next string's quote.
        r##"let p = r"C:\"; let u = "http://localhost:9876";"##,
        r##"let p = br"C:\"; let u = "http://localhost:9876";"##,
        r##"let p = cr"C:\"; let u = "http://localhost:9876";"##,
        // A marker on a line above that is not comment-only does not exempt.
        "let a = 1; // runner-localhost-ok: not for the next line\nlet u = \"http://localhost:9876\";",
        // Nor does a marker with no reason: bare, with nothing after its
        // colon, or with its block comment closed right after the colon.
        r#"let u = "http://localhost:9876"; // runner-localhost-ok"#,
        "// runner-localhost-ok\nlet u = \"http://localhost:9876\";",
        r#"let u = "http://localhost:9876"; // runner-localhost-ok:"#,
        r#"let u = "http://localhost:9876"; // runner-localhost-ok:   "#,
        r#"/* runner-localhost-ok: */ let u = "http://localhost:9876";"#,
        // The lexer resumes code after a char literal (an escaped one
        // included), a lifetime and a nested comment.
        "let c = '\"'; let u = \"http://localhost:9876\";",
        r#"let c = '\"'; let u = "http://localhost:9876";"#,
        "fn f<'a>(x: &'a str) -> String { format!(\"http://localhost:{}\", x) }",
        // A SINGLE lifetime: a lexer that pairs it with the next `'` would open
        // a string at the `\"` and hide the URL inside it.
        "fn f(s: &'static str) -> String { let q = '\"'; format!(\"http://localhost:{}\", s) }",
        "/* a /* b */ c */ let u = \"http://localhost:9876\";",
        // Production after a test-only item is still production.
        "#[cfg(test)]\nfn helper() {}\nfn real() { let u = \"http://localhost:9876\"; }",
        "#[cfg(test)]\nuse std::fs;\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(test)] mod tests {}\nconst U: &str = \"http://localhost:9876\";",
        "struct S {\n    #[cfg(test)]\n    f: u8,\n}\nconst U: &str = \"http://localhost:9876\";",
        // A gated field, match arm or parameter ends at its own `,`, or where
        // the enclosing list closes, so the siblings after it stay in scope.
        "S {\n    #[cfg(test)]\n    t: 1,\n    url: format!(\"http://localhost:{}\", port),\n}",
        "match x {\n    #[cfg(test)]\n    0 => String::new(),\n    _ => format!(\"http://localhost:{}\", p),\n}",
        "match x {\n    #[cfg(test)]\n    0 => {\n        String::new()\n    }\n    _ => format!(\"http://localhost:{}\", p),\n}",
        "fn f(\n    #[cfg(test)] a: u8,\n    b: u8,\n) {\n    let u = \"http://localhost:9876\";\n}",
        "fn f(\n    a: u8,\n    #[cfg(test)] b: u8\n) {\n    let u = \"http://localhost:9876\";\n}",
        // `union` and `macro_rules` are legal field names, so a gated field
        // spelled that way is not an item and still ends at its `,`.
        "S {\n    #[cfg(test)]\n    union: 1,\n    url: format!(\"http://localhost:{}\", p),\n}",
        "S {\n    #[cfg(test)]\n    macro_rules: 1,\n    url: format!(\"http://localhost:{}\", p),\n}",
        // A gated item bounded at its real end does not run on past it.
        "#[cfg(test)]\nfn t() -> [u8; 4] {\n    [0; 4]\n}\nconst U: &str = \"http://localhost:9876\";",
        // Gates that are not test-only.
        "#[cfg(not(test))]\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(any(test, unix))]\nconst U: &str = \"http://localhost:9876\";",
        // `#![cfg(test)]` marks a file test-only only when it comes first.
        "pub fn f() {}\n#![cfg(test)]\nconst U: &str = \"http://localhost:9876\";",
    ];

    let good: &[&str] = &[
        // The IPv4 loopback, ports outside the range, and other services.
        r#"let u = format!("http://127.0.0.1:{}", port);"#,
        r#"let u = "http://127.0.0.1:9877/ui-bridge";"#,
        r#"let u = "http://localhost:3001/build";"#,
        r#"let u = "http://localhost:9874/x";"#,
        r#"let u = "http://localhost:9900/x";"#,
        r#"let u = "http://localhost:98765";"#,
        // Comments of every kind, nested ones included.
        "// e.g. http://localhost:9876/health",
        "/// doc: http://localhost:{port}",
        "//! http://localhost:9875/",
        "/* http://localhost:9876 */ let a = 1;",
        // Block-comment state carries across lines: read as code, line 2's
        // `"` would open a string and hide nothing, so the URL is flagged.
        "/*\n   let u = \"http://localhost:9876\";\n*/",
        "/* a /* b */ \"http://localhost:9876\" */ let a = 1;",
        "let a = 1; // http://localhost:9876",
        "let c = '\"'; // http://localhost:9876",
        // An unpaired quote (an escaped char literal, a single lifetime) before
        // a commented URL: read to the end of the line, the comment would
        // become scanned text and be flagged.
        "let c = '\\''; // http://localhost:9876",
        "fn f(x: &'static str) {} // http://localhost:9876",
        // The marker, on the line or on a comment-only line above.
        r#"let u = format!("http://localhost:{}", vite_port); // runner-localhost-ok: Vite dev server"#,
        "// runner-localhost-ok: a target app's dev server\nlet u = format!(\"http://localhost:{}\", p);",
        r#"let u = "http://localhost:9876"; /* runner-localhost-ok: a target app */"#,
        // Test-only items, however the gate is spelled.
        "#[cfg(test)]\nmod tests {\n    const U: &str = \"http://localhost:9876\";\n}",
        "#[cfg(test)] mod tests {\n    const U: &str = \"http://localhost:9876\";\n}",
        "#[cfg(test)]\n/// doc\n#[allow(dead_code)]\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(all(unix, test))]\nconst U: &str = \"http://localhost:9876\";",
        // Braces inside literals do not end a gated item early.
        "#[cfg(test)]\nfn t() {\n    let s = r#\"a\"}\"#;\n    let c = '}';\n    let u = \"http://localhost:9876\";\n}",
        // Nor does a `;` inside brackets, or a `,` in an item's generics or
        // `where` clause: the gated item runs to its closing `}`.
        "#[cfg(test)]\nfn t() -> [u8; 4] {\n    let u = \"http://localhost:9876\";\n    [0; 4]\n}",
        "#[cfg(test)]\nfn t<A, B>(a: A, b: B) {\n    let u = \"http://localhost:9876\";\n}",
        // A second attribute between the gate and the item is skipped, so the
        // item is still read as an item and does not end at its signature's `,`.
        "#[cfg(test)]\n#[allow(dead_code)]\nfn t<A, B>(a: A, b: B) {\n    let u = \"http://localhost:9876\";\n}",
        "#[cfg(test)]\nfn t<A, B>(a: A, b: B)\nwhere\n    A: Clone,\n    B: Clone,\n{\n    let u = \"http://localhost:9876\";\n}",
        "#[cfg(test)]\npub(crate) const fn t<A, B>() -> u16 {\n    let u = \"http://localhost:9876\";\n    9876\n}",
        // A `union` followed by its name is still an item, so the `,` in its
        // generics does not end it.
        "#[cfg(test)]\nunion U<A: Copy, B: Copy> {\n    #[doc = \"http://localhost:9876\"]\n    a: A,\n    b: B,\n}",
        // A file that is test code in its entirety.
        "#![cfg(test)]\n\nconst U: &str = \"http://localhost:9876\";",
        "// licence\n//! doc\n#![cfg(test)]\nconst U: &str = \"http://localhost:9876\";",
    ];

    // Every fixture is checked before anything fails, so one run names every
    // miss rather than the first.
    let missed: Vec<&str> = bad.iter().copied().filter(|src| !flags(src)).collect();
    let wrongly_flagged: Vec<&str> = good.iter().copied().filter(|src| flags(src)).collect();
    assert!(
        missed.is_empty() && wrongly_flagged.is_empty(),
        "the guard MISSED {} forbidden form(s):\n{}\n\nand flagged {} allowed form(s):\n{}",
        missed.len(),
        missed.join("\n---\n"),
        wrongly_flagged.len(),
        wrongly_flagged.join("\n---\n")
    );
}

/// The boundary of the port range, enumerated rather than sampled.
#[test]
fn the_flagged_ports_are_exactly_9875_through_9899() {
    for port in 9860..=9910u32 {
        let src = format!("let u = \"http://localhost:{port}/health\";");
        assert_eq!(
            flags(&src),
            (9875..=9899).contains(&port),
            "port {port} is flagged wrongly"
        );
    }
    // A fifth digit makes it another port.
    assert_eq!(forbidden_at("localhost:98765"), None);
    assert_eq!(forbidden_at("localhost:98750/x"), None);
    assert_eq!(forbidden_at("see localhost:9875/x"), Some(4));
}

/// A gated item the region detector cannot close fails the scan by name,
/// rather than hiding the rest of the file.
#[test]
#[should_panic(expected = "fixture.rs:2: this #[cfg(test)] item never closes")]
fn an_unclosable_test_item_fails_the_scan() {
    flags("pub fn f() {}\n#[cfg(test)]\nfn t() {\n    let x = 1;\n");
}

#[test]
#[should_panic(
    expected = "fixture.rs: the scanner ends this file inside a string or a block comment"
)]
fn a_file_that_ends_inside_a_comment_fails_the_scan() {
    flags("pub fn f() {}\n/* never closed\nlet u = \"http://localhost:9876\";\n");
}

/// A file a parent declares `#[cfg(test)] mod x;` is test-only. A file that
/// some declaration WITHOUT the gate also resolves to is production, and so is
/// a declaration that sits inside a string, even one whose lines open as a
/// gated declaration would.
#[test]
fn test_only_files_come_from_cfg_test_declarations() {
    let src = |p: &str, t: &str| (PathBuf::from(p), t.to_string());
    let sources = vec![
        src(
            "src/routes/runners.rs",
            "pub fn f() {}\n\n#[cfg(test)]\nmod tests;\n",
        ),
        src(
            "src/dev_action/mod.rs",
            "pub mod run_result;\n#[cfg(test)]\npub(crate) mod test_support;\n",
        ),
        src(
            "src/peer/mod.rs",
            "pub fn f() {}\n#[cfg(test)] mod one_line_tests;\n",
        ),
        src(
            "src/a/b.rs",
            "#[cfg(test)]\n#[path = \"corpus_tests.rs\"]\nmod corpus;\n",
        ),
        src("src/c.rs", "#[cfg(test)]\nfn helper() {}\nmod real;\n"),
        // The string's lines open with the gate and the declaration, which is
        // what a reader of raw lines would take for a real one.
        src(
            "src/d.rs",
            "const S: &str = \"\n#[cfg(test)]\nmod fake_tests;\n\";\n",
        ),
        src("src/bin/tool.rs", "#[cfg(test)]\nmod tool_tests;\n"),
        // Two crate roots: `main.rs` gates both modules, `lib.rs` only one.
        src(
            "src/main.rs",
            "#[cfg(test)]\nmod test_clock;\n#[cfg(test)]\nmod test_ports;\n",
        ),
        src(
            "src/lib.rs",
            "pub mod test_clock;\n#[cfg(test)]\nmod test_ports;\n",
        ),
    ];
    let set = test_only_files(&sources);
    for p in [
        "src/routes/runners/tests.rs",
        "src/dev_action/test_support.rs",
        "src/peer/one_line_tests.rs",
        "src/a/corpus_tests.rs",
        "src/bin/tool_tests.rs",
        "src/test_ports.rs",
    ] {
        assert!(set.contains(Path::new(p)), "{p} is test-only: {set:?}");
    }
    for p in [
        "src/dev_action/run_result.rs",
        "src/c/real.rs",
        "src/d/fake_tests.rs",
        "src/test_clock.rs",
    ] {
        assert!(!set.contains(Path::new(p)), "{p} is production: {set:?}");
    }
}

/// A file reached only through gated declarations must carry the header: one
/// without it is returned, so the guard fails on it, and its header-bearing
/// twin is not.
#[test]
fn a_gated_only_file_without_the_header_is_returned() {
    let src = |p: &str, t: &str| (PathBuf::from(p), t.to_string());
    let sources = vec![
        src(
            "src/lib.rs",
            "#[cfg(test)]\nmod headerless;\n#[cfg(test)]\nmod headed;\npub mod prod;\n",
        ),
        src(
            "src/headerless.rs",
            "//! Test-only, but says so only in prose.\n\nuse crate::prod::X;\n",
        ),
        src(
            "src/headed.rs",
            "#![cfg(test)]\n\n//! Test-only.\n\nuse crate::prod::X;\n",
        ),
        src("src/prod.rs", "pub struct X;\n"),
    ];
    let declared = test_only_files(&sources);
    assert!(
        declared.contains(Path::new("src/headerless.rs")),
        "{declared:?}"
    );
    assert!(
        declared.contains(Path::new("src/headed.rs")),
        "{declared:?}"
    );
    assert_eq!(
        headerless_test_only_files(&sources, &declared),
        vec![PathBuf::from("src/headerless.rs")]
    );
}

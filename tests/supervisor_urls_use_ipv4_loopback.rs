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
//! The supervisor is also where agents get their runner URLs. The `api_url`
//! and `ui_bridge_url` of the spawn-test, spawn-named and runner-list responses
//! are what `/manual-test` and `/ui-bridge` tell them to follow. Plan
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
//! port set qontinui-claude-config check #14 reads.
//!
//! It reads every `.rs` file under `src/`, and production code only:
//!
//! - Comments are not scanned. A small lexer separates code and string text
//!   from comment text.
//! - An item gated `#[cfg(test)]` (or `#[cfg(all(…, test, …))]`) is skipped,
//!   brace-matched to its own end. So a test-only HELPER does not hide the
//!   production code after it.
//! - A whole file is skipped when it opens with `#![cfg(test)]`, or when EVERY
//!   `mod name;` declaration that resolves to it is gated that way. The rule is
//!   "every" because this crate has two roots. `main.rs` gates
//!   `mod test_clock;` while `lib.rs` declares `pub mod test_clock;`, so that
//!   file is compiled into production and stays in scope.
//!
//! ## The one exemption
//!
//! A `localhost:` URL that targets neither the supervisor nor a runner is
//! exempt when `runner-localhost-ok` appears IN A COMMENT, followed by a
//! reason. The comment must be on the same line or on a comment-only line
//! immediately above. qontinui-claude-config check #14 and qontinui-runner use
//! the same marker, so the fleet has one spelling for "this localhost is
//! deliberate". The token inside a string literal does not exempt.
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
//! ## Fixing a failure
//!
//! For a runner, use `loopback::runner_loopback_base(port)` or
//! `loopback::runner_ui_bridge_base(port)` (`crate::loopback::…` inside the
//! crate). For the supervisor itself, use
//! `loopback::loopback_base(state.config.port)`. For a bind, use
//! `loopback::LOOPBACK_IP`. In prompt or message text, spell
//! `127.0.0.1:<port>`. For a URL that is not a supervisor or a runner at all,
//! add `// runner-localhost-ok: <what it targets>`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The exemption token: qontinui-claude-config check #14's marker.
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

/// Split Rust source into [`LexedLine`]s. Strings and block comments carry
/// across lines.
fn lex(text: &str) -> Vec<LexedLine> {
    let mut out = Vec::new();
    let mut state = State::Code;
    for line in text.lines() {
        let b = line.as_bytes();
        let (mut scanned, mut comment, mut code) = (Vec::new(), Vec::new(), Vec::new());
        let mut i = 0;
        while i < b.len() {
            let rest = &b[i..];
            match state {
                State::Block(depth) => {
                    if rest.starts_with(b"/*") {
                        comment.extend_from_slice(b"/*");
                        state = State::Block(depth + 1);
                        i += 2;
                    } else if rest.starts_with(b"*/") {
                        comment.extend_from_slice(b"*/");
                        state = if depth == 1 {
                            State::Code
                        } else {
                            State::Block(depth - 1)
                        };
                        i += 2;
                    } else {
                        comment.push(b[i]);
                        i += 1;
                    }
                }
                State::Str { raw_hashes: None } => {
                    let take = match b[i] {
                        // An escape: keep it with the byte it escapes.
                        b'\\' => rest.len().min(2),
                        b'"' => {
                            state = State::Code;
                            1
                        }
                        _ => 1,
                    };
                    scanned.extend_from_slice(&rest[..take]);
                    i += take;
                }
                State::Str {
                    raw_hashes: Some(n),
                } => {
                    let closes = b[i] == b'"'
                        && rest
                            .get(1..=n)
                            .is_some_and(|hashes| hashes.iter().all(|c| *c == b'#'));
                    let take = if closes {
                        state = State::Code;
                        1 + n
                    } else {
                        1
                    };
                    scanned.extend_from_slice(&rest[..take]);
                    i += take;
                }
                State::Code => {
                    if rest.starts_with(b"//") {
                        comment.extend_from_slice(rest);
                        break;
                    }
                    if rest.starts_with(b"/*") {
                        comment.extend_from_slice(b"/*");
                        state = State::Block(1);
                        i += 2;
                        continue;
                    }
                    if let Some((len, hashes)) = raw_string_open(b, i) {
                        scanned.extend_from_slice(&rest[..len]);
                        state = State::Str {
                            raw_hashes: Some(hashes),
                        };
                        i += len;
                        continue;
                    }
                    match b[i] {
                        b'"' => {
                            scanned.push(b'"');
                            state = State::Str { raw_hashes: None };
                            i += 1;
                        }
                        b'\'' => match char_literal_end(line, i) {
                            // A char literal is not code, so its `{` or `"`
                            // must not count.
                            Some(end) => {
                                scanned.extend_from_slice(&b[i..end]);
                                i = end;
                            }
                            // A lifetime or a label.
                            None => {
                                scanned.push(b'\'');
                                code.push(b'\'');
                                i += 1;
                            }
                        },
                        c => {
                            scanned.push(c);
                            code.push(c);
                            i += 1;
                        }
                    }
                }
            }
        }
        out.push(LexedLine {
            scanned: String::from_utf8_lossy(&scanned).into_owned(),
            comment: String::from_utf8_lossy(&comment).into_owned(),
            code: String::from_utf8_lossy(&code).into_owned(),
        });
    }
    out
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

/// Line ranges (0-based, inclusive) of the test-only items in a file. Each item
/// an outer test-only `#[cfg(…)]` gates is one range, brace-matched to its own
/// end.
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

/// The last line of the item that the `#[cfg(…)]` attribute on line `attr`
/// gates, or `None` when no end is found.
///
/// Braces are counted over CODE text only, starting just after the attribute.
/// The lexer has already removed strings, chars and comments, so a `{` inside
/// a literal cannot unbalance the count. The item ends at the `}` that closes
/// its first `{`, or at a `;` before any `{` (a `use`, a `const`, a bodiless
/// `mod x;`). A `}` that closes an ENCLOSING scope before the item opened one
/// (a gated struct field) means the item ended on the line before.
fn gated_item_end(lines: &[LexedLine], attr: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, line) in lines.iter().enumerate().skip(attr) {
        let code = if k == attr {
            cfg_predicate(line.code.trim()).map_or("", |(_, rest)| rest)
        } else {
            line.code.as_str()
        };
        for c in code.bytes() {
            match c {
                b'{' => depth += 1,
                b'}' if depth == 0 => return Some(k.saturating_sub(1).max(attr)),
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(k);
                    }
                }
                b';' if depth == 0 => return Some(k),
                _ => {}
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
        for decl in mod_declarations(&lex(text)) {
            let set = if decl.gated { &mut gated } else { &mut ungated };
            set.extend(declaration_targets(file, &decl));
        }
    }
    gated.difference(&ungated).cloned().collect()
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
    let lines = lex(text);
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
        let exempt_here = ll.comment.contains(MARKER);
        // The line above exempts only when it is a COMMENT-ONLY line.
        let exempt_above = idx > 0
            && lines[idx - 1].comment.contains(MARKER)
            && lines[idx - 1].scanned.trim().is_empty();
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

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    // A directory that cannot be read fails the guard, rather than shrinking
    // it into a no-op.
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
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

    let mut hits = Vec::new();
    let (mut files_scanned, mut lines_scanned, mut skipped_test) = (0usize, 0usize, 0usize);
    for (path, text) in &sources {
        let rel = path
            .strip_prefix(&base)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.display().to_string());
        if declared_test_only.contains(path) {
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
             `// {MARKER}: <what it targets>` comment on the line or the line above.\n{}",
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
        // The marker inside a STRING does not exempt.
        r#"let u = "http://localhost:9876"; let n = "runner-localhost-ok";"#,
        // A marker on a line above that is not comment-only does not exempt.
        "let a = 1; // runner-localhost-ok: not for the next line\nlet u = \"http://localhost:9876\";",
        // The lexer resumes code after a char literal, a lifetime and a nested comment.
        "let c = '\"'; let u = \"http://localhost:9876\";",
        "fn f<'a>(x: &'a str) -> String { format!(\"http://localhost:{}\", x) }",
        "/* a /* b */ c */ let u = \"http://localhost:9876\";",
        // Production after a test-only item is still production.
        "#[cfg(test)]\nfn helper() {}\nfn real() { let u = \"http://localhost:9876\"; }",
        "#[cfg(test)]\nuse std::fs;\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(test)] mod tests {}\nconst U: &str = \"http://localhost:9876\";",
        "struct S {\n    #[cfg(test)]\n    f: u8,\n}\nconst U: &str = \"http://localhost:9876\";",
        // Gates that are not test-only.
        "#[cfg(not(test))]\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(any(test, unix))]\nconst U: &str = \"http://localhost:9876\";",
        // `#![cfg(test)]` marks a file test-only only when it comes first.
        "pub fn f() {}\n#![cfg(test)]\nconst U: &str = \"http://localhost:9876\";",
    ];
    for src in bad {
        assert!(flags(src), "the guard MISSED a forbidden form: {src}");
    }

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
        "/*\n   http://localhost:9876\n*/",
        "/* a /* b */ \"http://localhost:9876\" */ let a = 1;",
        "let a = 1; // http://localhost:9876",
        "let c = '\"'; // http://localhost:9876",
        "let c = '\\''; // http://localhost:9876",
        // The marker, on the line or on a comment-only line above.
        r#"let u = format!("http://localhost:{}", vite_port); // runner-localhost-ok: Vite dev server"#,
        "// runner-localhost-ok: a target app's dev server\nlet u = format!(\"http://localhost:{}\", p);",
        // Test-only items, however the gate is spelled.
        "#[cfg(test)]\nmod tests {\n    const U: &str = \"http://localhost:9876\";\n}",
        "#[cfg(test)] mod tests {\n    const U: &str = \"http://localhost:9876\";\n}",
        "#[cfg(test)]\n/// doc\n#[allow(dead_code)]\nconst U: &str = \"http://localhost:9876\";",
        "#[cfg(all(unix, test))]\nconst U: &str = \"http://localhost:9876\";",
        // Braces inside literals do not end a gated item early.
        "#[cfg(test)]\nfn t() {\n    let s = r#\"}\"#;\n    let c = '}';\n    let u = \"http://localhost:9876\";\n}",
        // A file that is test code in its entirety.
        "#![cfg(test)]\n\nconst U: &str = \"http://localhost:9876\";",
        "// licence\n//! doc\n#![cfg(test)]\nconst U: &str = \"http://localhost:9876\";",
    ];
    for src in good {
        assert!(!flags(src), "the guard flagged an allowed form: {src}");
    }
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

/// A file a parent declares `#[cfg(test)] mod x;` is test-only. A file that
/// some declaration WITHOUT the gate also resolves to is production, and so is
/// a declaration that sits inside a string.
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
        src(
            "src/d.rs",
            "const S: &str = \"#[cfg(test)] mod fake_tests;\";\n",
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

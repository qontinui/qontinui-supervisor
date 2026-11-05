//! The scan behind `test_ports::tests::no_test_code_binds_port_0_outside_test_ports`:
//! every `bind(…)` in test code whose argument is an address with port 0.
//!
//! **What a token rule holds, and what it cannot see.** A port the kernel picked
//! belongs to the test only while the socket that holds it lives. `test_ports`
//! hands one out only together with that socket ([`super::held_ephemeral_listener`],
//! [`super::held_ephemeral_tokio_listener`]), and its dead-port fixtures
//! ([`super::unheld_port`], [`super::reserve_refused_port`]) never release one. So
//! if test code binds port 0 in `test_ports` and nowhere else, no fixture outside it
//! can bind a kernel-picked port, let the socket go and keep using the port: that
//! takes a bind this rule flags. A test can still release a socket `test_ports`
//! handed it, and no bind marks either way it does so: dropping the listener, or
//! giving it to a server that shuts down, since a server drops its listener when it
//! stops (axum's graceful shutdown drops it as soon as the shutdown is signalled).
//! The helpers' docs carry that rule: a test whose code under test still probes or
//! kills the port after its server stops listens on [`super::unheld_port`] instead,
//! and only a test whose subject is a released port drops a listener on purpose.
//!
//! **A port-0 bind** is a call of `bind` (`TcpListener::bind(…)`,
//! `sock.bind(…)`) whose parenthesised argument holds, at any depth:
//!
//! * a string literal that is a host and port 0: `"127.0.0.1:0"`, `"[::1]:0"`,
//!   `"localhost:0"`;
//! * a tuple whose last element is the literal `0`: `("127.0.0.1", 0)`,
//!   `([127, 0, 0, 1], 0)` (so `SocketAddr::from(([127, 0, 0, 1], 0))`),
//!   `(Ipv4Addr::LOCALHOST, 0u16)`;
//! * `SocketAddr::new`, `SocketAddrV4::new` or `SocketAddrV6::new` whose second
//!   argument, the port, is the literal `0`.
//!
//! What it does not see, so it can miss a bind but never invents one: an
//! address built before the call and passed in by name
//! (`let a = "127.0.0.1:0"; bind(a)`), and a port computed at run time.
//!
//! **Test code** is a file under `tests/`, a file that opens with
//! `#![cfg(test)]` ([`is_test_only_file`]), a file declared by a
//! `#[cfg(test)] mod NAME;` (or declared at all inside such a file), and, in
//! every other file, the items under `#[test]`, `#[tokio::test]` or a
//! `#[cfg(…)]` that requires `test`, with everything inside them.

use crate::source_scan::{is_test_only_file, token_trees, tokens, TokenKind, TokenTree};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A port-0 bind in test code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bind {
    /// The line the `bind` call is on.
    pub(crate) line: usize,
    /// The test item it is in: a function's name, or the name of the other item
    /// (a `static`, a `const`) that holds it.
    pub(crate) function: String,
}

/// A `mod NAME;` declaration, so a caller can find the module's file. `path` is
/// the inline modules around it and then its own name; `test` is whether it is
/// compiled only for tests (its own `#[cfg(test)]`, or an enclosing one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModDecl {
    pub(crate) path: Vec<String>,
    pub(crate) test: bool,
}

/// What a scan of one file found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileScan {
    pub(crate) binds: Vec<Bind>,
    pub(crate) mod_decls: Vec<ModDecl>,
}

/// Scan one source file. With `whole_file_is_test` every item is test code (an
/// integration test, a module compiled only for tests); otherwise only test
/// items are. `None` when the file does not lex or its delimiters do not
/// balance.
pub(crate) fn scan(text: &str, whole_file_is_test: bool) -> Option<FileScan> {
    let tokens = tokens(text)?;
    let trees = token_trees(&tokens)?;
    let mut scan = FileScan::default();
    walk_items(&trees, whole_file_is_test, &[], &mut scan);
    scan.binds.sort_by_key(|b| b.line);
    Some(scan)
}

/// Walk an item list (a file, a module, an `impl` or a `trait` body): record
/// its module declarations, and the port-0 binds in each of its test items.
/// `test` is whether the whole list is test code.
fn walk_items(trees: &[TokenTree<'_>], mut test: bool, path: &[String], scan: &mut FileScan) {
    let mut i = 0;
    let mut attribute_test = false;
    while i < trees.len() {
        if let Some((next, inner, attr_test)) = attribute(trees, i) {
            if inner {
                test |= attr_test;
            } else {
                attribute_test |= attr_test;
            }
            i = next;
            continue;
        }
        let item_test = test || attribute_test;
        attribute_test = false;
        let keyword = skip_qualifiers(trees, i);
        let Some(head) = trees.get(keyword) else {
            break;
        };
        let name = ident_text(trees.get(keyword + 1)).unwrap_or_default();
        if head.is_ident("mod") {
            match trees.get(keyword + 2) {
                Some(group) if group.delimiter() == Some(b'{') => {
                    let mut inner_path = path.to_vec();
                    inner_path.push(name);
                    walk_items(group.inner(), item_test, &inner_path, scan);
                    i = keyword + 3;
                }
                _ => {
                    let mut decl_path = path.to_vec();
                    decl_path.push(name);
                    scan.mod_decls.push(ModDecl {
                        path: decl_path,
                        test: item_test,
                    });
                    i = item_end(trees, keyword);
                }
            }
            continue;
        }
        if head.is_ident("impl") || head.is_ident("trait") {
            match first_brace(trees, keyword + 1) {
                Some(body) => {
                    walk_items(trees[body].inner(), item_test, path, scan);
                    i = body + 1;
                }
                None => i = item_end(trees, keyword),
            }
            continue;
        }
        let end = if head.is_ident("fn") {
            fn_body_index(trees, keyword + 2).map_or_else(|| item_end(trees, keyword), |b| b + 1)
        } else {
            item_end(trees, keyword)
        };
        if item_test {
            port_zero_binds(&trees[keyword..end], &name, &mut scan.binds);
        }
        i = end.max(i + 1);
    }
}

/// Every port-0 bind in `trees`, at any depth, as found in `item`.
fn port_zero_binds(trees: &[TokenTree<'_>], item: &str, out: &mut Vec<Bind>) {
    for (i, tree) in trees.iter().enumerate() {
        let is_call = tree.is_ident("bind")
            && !(i > 0 && trees[i - 1].is_ident("fn"))
            && trees
                .get(i + 1)
                .is_some_and(|t| t.delimiter() == Some(b'('));
        if is_call && holds_port_zero_address(trees[i + 1].inner()) {
            out.push(Bind {
                line: tree.line(),
                function: item.to_string(),
            });
        }
        port_zero_binds(tree.inner(), item, out);
    }
}

/// Whether `trees`, at any depth, hold an address with port 0: a `host:0`
/// string literal, a tuple ending in the literal `0`, or a `SocketAddr*::new`
/// whose port argument is `0`.
fn holds_port_zero_address(trees: &[TokenTree<'_>]) -> bool {
    trees.iter().enumerate().any(|(i, tree)| {
        if tree
            .leaf()
            .is_some_and(|t| t.kind == TokenKind::Str && is_port_zero_text(t.text))
        {
            return true;
        }
        if tree.delimiter() == Some(b'(') {
            let elements = split(tree.inner(), ",");
            let called = i > 0 && is_callee_end(&trees[i - 1]);
            if called && is_socket_addr_new(&trees[..i]) {
                if elements.get(1).is_some_and(|port| is_zero_literal(port)) {
                    return true;
                }
            } else if !called
                && elements.len() >= 2
                && elements.last().is_some_and(|last| is_zero_literal(last))
            {
                return true;
            }
        }
        holds_port_zero_address(tree.inner())
    })
}

/// Whether a `(…)` group right after `tree` is a call's argument list rather
/// than a tuple: it follows a name, a closing generic or a macro's `!`.
fn is_callee_end(tree: &TokenTree<'_>) -> bool {
    tree.leaf()
        .is_some_and(|t| t.kind == TokenKind::Ident || t.text == ">" || t.text == "!")
}

/// Whether `before` ends in `SocketAddr::new`, `SocketAddrV4::new` or
/// `SocketAddrV6::new`, the constructors whose second argument is the port.
fn is_socket_addr_new(before: &[TokenTree<'_>]) -> bool {
    match before {
        [.., ty, sep, new] => {
            new.is_ident("new")
                && sep.is_punct("::")
                && ["SocketAddr", "SocketAddrV4", "SocketAddrV6"]
                    .iter()
                    .any(|name| ty.is_ident(name))
        }
        _ => false,
    }
}

/// Whether `element` is the one integer literal `0`, with any `_` separators
/// and integer suffix (`0`, `0u16`, `0_u16`).
fn is_zero_literal(element: &[TokenTree<'_>]) -> bool {
    let [only] = element else {
        return false;
    };
    only.leaf()
        .filter(|t| t.kind == TokenKind::Number)
        .is_some_and(|t| {
            let digits = t.text.replace('_', "");
            let digits = [
                "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8",
                "i8",
            ]
            .iter()
            .find_map(|suffix| digits.strip_suffix(suffix))
            .unwrap_or(&digits);
            digits == "0"
        })
}

/// Whether a string literal's token text is a host with port 0: an IPv4
/// address, a bracketed IPv6 address or `localhost`, then `:0`.
fn is_port_zero_text(literal: &str) -> bool {
    let Some(content) = string_content(literal) else {
        return false;
    };
    let Some(host) = content.strip_suffix(":0") else {
        return false;
    };
    let ipv4 = host.split('.').count() == 4
        && host.split('.').all(|octet| {
            !octet.is_empty() && octet.len() <= 3 && octet.bytes().all(|b| b.is_ascii_digit())
        });
    let ipv6 = host.len() > 2
        && host.starts_with('[')
        && host.ends_with(']')
        && host[1..host.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
    ipv4 || ipv6 || host == "localhost"
}

/// What a plain or raw string literal's token text holds between its quotes;
/// `None` for a byte or C string, which no address parse takes.
fn string_content(literal: &str) -> Option<&str> {
    let raw = literal.strip_prefix('r').unwrap_or(literal);
    let hashes = raw.len() - raw.trim_start_matches('#').len();
    let body = raw[hashes..].strip_prefix('"')?;
    body.get(..body.len().checked_sub(1 + hashes)?)
}

/// At `i`, an attribute: `(index after it, whether it is an inner `#![…]`, whether
/// it makes its item test code)`.
fn attribute(trees: &[TokenTree<'_>], i: usize) -> Option<(usize, bool, bool)> {
    if !trees.get(i)?.is_punct("#") {
        return None;
    }
    let inner = trees.get(i + 1).is_some_and(|t| t.is_punct("!"));
    let group = trees.get(i + 1 + usize::from(inner))?;
    (group.delimiter() == Some(b'[')).then(|| {
        (
            i + 2 + usize::from(inner),
            inner,
            is_test_attribute(group.inner()),
        )
    })
}

/// `#[test]`, `#[tokio::test…]`, or `#[cfg(P)]` whose predicate requires `test`.
fn is_test_attribute(attr: &[TokenTree<'_>]) -> bool {
    match attr {
        [only] => only.is_ident("test"),
        [tokio, sep, test, ..] if tokio.is_ident("tokio") && sep.is_punct("::") => {
            test.is_ident("test")
        }
        [cfg, predicate] if cfg.is_ident("cfg") => requires_test(predicate.inner()),
        _ => false,
    }
}

/// Whether a `cfg` predicate holds only when `test` does.
fn requires_test(predicate: &[TokenTree<'_>]) -> bool {
    match predicate {
        [only] => only.is_ident("test"),
        [all, args] if all.is_ident("all") => {
            split(args.inner(), ",").into_iter().any(requires_test)
        }
        [any, args] if any.is_ident("any") => {
            let arms = split(args.inner(), ",");
            !arms.is_empty() && arms.into_iter().all(requires_test)
        }
        _ => false,
    }
}

/// The index of an item's keyword, past its visibility and qualifiers.
fn skip_qualifiers(trees: &[TokenTree<'_>], mut i: usize) -> usize {
    loop {
        let Some(tree) = trees.get(i) else {
            return i;
        };
        if tree.is_ident("pub") {
            i += 1;
            if trees.get(i).is_some_and(|t| t.delimiter() == Some(b'(')) {
                i += 1;
            }
        } else if tree.is_ident("const")
            && !trees.get(i + 1).is_some_and(|t| {
                t.is_ident("fn")
                    || t.is_ident("unsafe")
                    || t.is_ident("async")
                    || t.is_ident("extern")
            })
        {
            return i;
        } else if ["const", "async", "unsafe", "default", "auto"]
            .iter()
            .any(|q| tree.is_ident(q))
        {
            i += 1;
        } else if tree.is_ident("extern") {
            i += 1;
            if trees
                .get(i)
                .and_then(TokenTree::leaf)
                .is_some_and(|t| t.kind == TokenKind::Str)
            {
                i += 1;
            }
        } else {
            return i;
        }
    }
}

/// The index of a function's body: the first `{…}` after its signature, unless a
/// `;` (a declaration with no body) comes first.
fn fn_body_index(trees: &[TokenTree<'_>], from: usize) -> Option<usize> {
    (from..trees.len())
        .take_while(|&j| !trees[j].is_punct(";"))
        .find(|&j| trees[j].delimiter() == Some(b'{'))
}

/// The index of the first `{…}` from `from`.
fn first_brace(trees: &[TokenTree<'_>], from: usize) -> Option<usize> {
    (from..trees.len()).find(|&j| trees[j].delimiter() == Some(b'{'))
}

/// The index just past an item that starts at `i`: past its `;`, or past its
/// first `{…}` (a struct, an enum, a macro invocation).
fn item_end(trees: &[TokenTree<'_>], i: usize) -> usize {
    (i..trees.len())
        .find(|&j| trees[j].is_punct(";") || trees[j].delimiter() == Some(b'{'))
        .map_or(trees.len(), |j| j + 1)
}

fn ident_text(tree: Option<&TokenTree<'_>>) -> Option<String> {
    tree.and_then(TokenTree::leaf)
        .filter(|t| t.kind == TokenKind::Ident)
        .map(|t| t.text.to_string())
}

/// `trees` split on top-level `sep`, empty pieces dropped.
fn split<'a, 't>(trees: &'a [TokenTree<'t>], sep: &str) -> Vec<&'a [TokenTree<'t>]> {
    trees
        .split(|t| t.is_punct(sep))
        .filter(|piece| !piece.is_empty())
        .collect()
}

/// The port-0 binds in the test code of every `.rs` file under `root`'s `src/`
/// and `tests/`, keyed by its path relative to `root` with `/` separators;
/// `None` for a file that does not lex.
///
/// A file is test code as a whole when it sits under `tests/`, opens with
/// `#![cfg(test)]` ([`is_test_only_file`]), or is the file of a module declared
/// `#[cfg(test)] mod NAME;` (or declared at all inside such a file); in every
/// other file only test items are.
pub(crate) fn scan_crate(root: &Path) -> BTreeMap<String, Option<Vec<Bind>>> {
    let src = rust_files(&root.join("src"));
    let tests = rust_files(&root.join("tests"));
    let texts: BTreeMap<&PathBuf, String> = src
        .iter()
        .chain(&tests)
        .map(|path| {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            (path, text.replace("\r\n", "\n"))
        })
        .collect();
    let mut whole: BTreeSet<&PathBuf> = tests.iter().collect();
    whole.extend(src.iter().filter(|path| is_test_only_file(&texts[path])));
    loop {
        let mut grew = false;
        for file in &src {
            let Some(found) = scan(&texts[file], whole.contains(file)) else {
                continue;
            };
            for decl in found.mod_decls {
                if !(decl.test || whole.contains(file)) {
                    continue;
                }
                for candidate in declared_files(file, &decl.path) {
                    if let Some((path, _)) = texts.get_key_value(&candidate) {
                        grew |= whole.insert(path);
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    texts
        .iter()
        .map(|(path, text)| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            let key = relative.to_string_lossy().replace('\\', "/");
            (key, scan(text, whole.contains(path)).map(|s| s.binds))
        })
        .collect()
}

/// Every `.rs` file under `dir`, sorted.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The files a `mod` declaration in `file` can name: `NAME.rs` and
/// `NAME/mod.rs`, under `file`'s module directory and the inline modules around
/// the declaration.
fn declared_files(file: &Path, path: &[String]) -> [PathBuf; 2] {
    let parent = file.parent().unwrap_or(Path::new(""));
    let mut dir = match file.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(file.file_stem().unwrap_or_default()),
    };
    let (name, inline) = path
        .split_last()
        .map_or(("", &[][..]), |(n, i)| (n.as_str(), i));
    for module in inline {
        dir = dir.join(module);
    }
    [
        dir.join(format!("{name}.rs")),
        dir.join(name).join("mod.rs"),
    ]
}

#[cfg(test)]
mod tests;

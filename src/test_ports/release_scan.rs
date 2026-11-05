//! The data-flow half of `test_ports::tests::no_test_fixture_releases_a_port_to_fabricate_a_dead_one`:
//! find test code that binds a loopback socket to port 0, reads the port the
//! kernel chose, and lets the socket go while the port is still in use.
//!
//! **A release is a fact about data flow, not about syntax.** A socket bound to
//! port 0 is its port's HOLDER. The holder is released by `drop(holder)`, or at
//! the end of the block that declares it, unless it is moved out first: into a
//! call, a `move` closure or an `async move` block, an aggregate, `mem::forget`,
//! or the block's own value. A PORT VALUE is anything read from the holder's
//! `local_addr()`, or computed from another port value (`addr.port()`,
//! `format!("http://{addr}")`, `config.port = port`). The scan flags two shapes:
//!
//! * [`Shape::UsedAfterRelease`]: a port value is used after its holder was
//!   released, inside one function (`drop(l); register(port)`);
//! * [`Shape::ValueOutlivesHolder`]: a block or a function's value is a port
//!   value, while its holder is released at that block's end
//!   (`let port = { let l = bind(..)?; l.local_addr()?.port() };`, or a helper
//!   returning the port of a listener it lets go).
//!
//! A listener HELD until after the port's last use is not a release, which is
//! why the rule is keyed on data flow: a syntactic rule ("a bind followed by a
//! drop") flags every held listener, and a guard that flags correct code trains
//! people to allowlist it.
//!
//! What it follows. A bind is `bind(…)` whose argument is a loopback or
//! wildcard address with port 0 (`"127.0.0.1:0"`, `("127.0.0.1", 0)`,
//! `SocketAddr::from(([127, 0, 0, 1], 0))`, or a binding initialised from one),
//! as a path call (`TcpListener::bind`, whose value is the holder) or as a
//! method call on a socket (`sock.bind(&addr)`, which makes `sock` the holder).
//! A same-file helper whose value is a fresh holder, or a tuple of one and its
//! port (`fn ephemeral_listener() -> (TcpListener, u16)`), is followed into its
//! callers. A drop on a branch that diverges (`return`, `break`, `continue`,
//! `panic!`) does not release the holder on the path after the branch.
//!
//! What it does not follow, so it can miss a release but never invents one:
//! helpers in other files, holders stored in a field or a collection, a port
//! captured by a closure that is called later, and `return` from a closure.

mod flow;
#[cfg(test)]
mod tests;

use crate::source_scan::{is_test_only_file, token_trees, tokens, TokenKind, TokenTree};
use flow::{Analyzer, Helper};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

/// A test fixture that let a port-0 socket go while its port was still in use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    /// The function the socket was bound in.
    pub(crate) function: String,
    /// The line of the bind.
    pub(crate) bound_at: usize,
    /// The line the release shows on: the use after the release, or the value
    /// that outlives its holder.
    pub(crate) line: usize,
    pub(crate) shape: Shape,
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.shape {
            Shape::UsedAfterRelease { released_at } => write!(
                f,
                "fn {}: the port of the socket bound to port 0 at line {} is used after the \
                 socket is released at line {released_at}",
                self.function, self.bound_at
            ),
            Shape::ValueOutlivesHolder => write!(
                f,
                "fn {}: this value is the port of the socket bound to port 0 at line {}, which \
                 is released as the block that holds it ends",
                self.function, self.bound_at
            ),
        }
    }
}

/// How a [`Release`] shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// The port is used after its holder was released at line `released_at`.
    UsedAfterRelease { released_at: usize },
    /// A block or a function's value is the port, and the holder it was read
    /// from is released at that block's end.
    ValueOutlivesHolder,
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
    pub(crate) releases: Vec<Release>,
    /// How many port-0 binds in test code the scan followed. A scan that
    /// followed none of the binds it was pointed at proves nothing.
    pub(crate) binds: usize,
    /// Each test-code function the scan analysed, by name, and how many binds it
    /// followed there.
    pub(crate) function_binds: BTreeMap<String, usize>,
    /// The file's `mod NAME;` declarations.
    pub(crate) mod_decls: Vec<ModDecl>,
}

/// Scan one source file. With `whole_file_is_test` every function is test code
/// (an integration test, a module compiled only for tests); otherwise only
/// functions under `#[test]`, `#[tokio::test]` or `#[cfg(test)]` are. `None`
/// when the file does not lex or its delimiters do not balance.
pub(crate) fn scan(text: &str, whole_file_is_test: bool) -> Option<FileScan> {
    let tokens = tokens(text)?;
    let trees = token_trees(&tokens)?;
    let mut items = Items::default();
    collect_items(&trees, whole_file_is_test, &[], &mut items);

    // Pass 1: which same-file helpers hand out a fresh holder (and its port).
    let no_helpers = HashMap::new();
    let mut helpers: HashMap<String, Option<Helper>> = HashMap::new();
    for item in &items.fns {
        let mut analyzer = Analyzer::new(&item.name, &no_helpers);
        let shape = analyzer.function(item.body, item.close_line, true);
        if let Some(shape) = shape {
            helpers
                .entry(item.name.clone())
                .and_modify(|seen| {
                    if seen.as_ref() != Some(&shape) {
                        *seen = None;
                    }
                })
                .or_insert(Some(shape));
        }
    }

    // Pass 2: every test-code function, its nested functions included.
    let mut scan = FileScan {
        mod_decls: items.decls,
        ..FileScan::default()
    };
    let mut queue: Vec<(String, &[TokenTree<'_>], usize)> = items
        .fns
        .iter()
        .filter(|item| item.test)
        .map(|item| (item.name.clone(), item.body, item.close_line))
        .collect();
    while let Some((name, body, close_line)) = queue.pop() {
        let mut analyzer = Analyzer::new(&name, &helpers);
        analyzer.function(body, close_line, false);
        scan.binds += analyzer.binds();
        *scan.function_binds.entry(name).or_default() += analyzer.binds();
        scan.releases.extend(analyzer.releases);
        queue.extend(analyzer.nested);
    }
    scan.releases.sort_by_key(|r| (r.bound_at, r.line));
    Some(scan)
}

/// A function item and whether it is test code.
struct FnItem<'a, 't> {
    name: String,
    body: &'a [TokenTree<'t>],
    close_line: usize,
    test: bool,
}

#[derive(Default)]
struct Items<'a, 't> {
    fns: Vec<FnItem<'a, 't>>,
    decls: Vec<ModDecl>,
}

/// Walk an item list (a file, a module, an `impl` or a `trait` body), collecting
/// its functions and module declarations. `test` is whether the list is test code.
fn collect_items<'a, 't>(
    trees: &'a [TokenTree<'t>],
    mut test: bool,
    path: &[String],
    items: &mut Items<'a, 't>,
) {
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
        if head.is_ident("fn") {
            let name = ident_text(trees.get(keyword + 1)).unwrap_or_default();
            match fn_body_index(trees, keyword + 2) {
                Some(body) => {
                    items.fns.push(FnItem {
                        name,
                        body: trees[body].inner(),
                        close_line: trees[body].end_line(),
                        test: item_test,
                    });
                    i = body + 1;
                }
                None => i = item_end(trees, keyword),
            }
        } else if head.is_ident("mod") {
            let name = ident_text(trees.get(keyword + 1)).unwrap_or_default();
            match trees.get(keyword + 2) {
                Some(group) if group.delimiter() == Some(b'{') => {
                    let mut inner_path = path.to_vec();
                    inner_path.push(name);
                    collect_items(group.inner(), item_test, &inner_path, items);
                    i = keyword + 3;
                }
                _ => {
                    let mut decl_path = path.to_vec();
                    decl_path.push(name);
                    items.decls.push(ModDecl {
                        path: decl_path,
                        test: item_test,
                    });
                    i = item_end(trees, keyword);
                }
            }
        } else if head.is_ident("impl") || head.is_ident("trait") {
            match first_brace(trees, keyword + 1) {
                Some(body) => {
                    collect_items(trees[body].inner(), item_test, path, items);
                    i = body + 1;
                }
                None => i = item_end(trees, keyword),
            }
        } else {
            i = item_end(trees, keyword);
        }
    }
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

/// Scan every `.rs` file under `root`'s `src/` and `tests/`, keyed by its path
/// relative to `root` with `/` separators; `None` for a file that does not lex.
///
/// A file is test code as a whole when it sits under `tests/`, opens with
/// `#![cfg(test)]` ([`is_test_only_file`]), or is the file of a module declared
/// `#[cfg(test)] mod NAME;` (or declared at all inside such a file); in every
/// other file only test items are.
pub(crate) fn scan_crate(root: &Path) -> BTreeMap<String, Option<FileScan>> {
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
            (key, scan(text, whole.contains(path)))
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

//! Per-language symbol extraction via tree-sitter.
//!
//! Each language has its own `extract_*` entry point that takes the source
//! text and returns a deduped `Vec<Symbol>` of every top-level definition the
//! grammar identified. Output is best-effort — if tree-sitter returns ERROR
//! nodes (partial parse / typing-in-progress source), we still surface
//! whatever symbols *were* identified; we never fail the file.
//!
//! Symbol-naming conventions per language:
//!
//! - **Rust**: free `fn foo`, `struct Foo`, `enum Foo`, `trait Foo`,
//!   `mod foo`, and `impl Foo { fn bar() }` methods. Method names take the
//!   form `Foo::bar` so `impl Foo { fn bar() }` and `impl Foo { fn baz() }`
//!   are distinct symbols (claim contention at method granularity).
//!   Trait-signature items inside `trait Foo { fn baz(); }` are exposed as
//!   `Foo::baz` for the same reason.
//! - **TypeScript**: only `export`ed definitions count. Function, class,
//!   interface, type alias declarations are top-level symbols; class
//!   methods are `Class.method`.
//! - **Python**: every module-level `def` / `class`. Methods are
//!   `Class.method`. Indentation-based scoping.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A definition extracted from a source file.
///
/// Used for both the diff (compares hash-set-of-Symbol pre/post-save) and
/// the claim-acquisition payload (resource_key uses `name`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    /// Language-native identifier (e.g. `Foo::bar` for a Rust method).
    pub name: String,
    /// 0-indexed inclusive start byte offset in the source file. Cheap to
    /// derive from tree-sitter `Node::byte_range`; used downstream by the
    /// diff to detect "same name, different location" as a modification.
    pub start_line: u32,
    /// 0-indexed inclusive end byte offset.
    pub end_line: u32,
}

/// Dispatch on file extension. Unrecognized extensions return empty.
///
/// Caller is responsible for resolving the extension from the watched
/// path; `file_watch.rs` filters before calling.
pub fn extract_symbols(ext: &str, source: &str) -> Vec<Symbol> {
    match ext {
        "rs" => extract_rust(source),
        "ts" | "tsx" => extract_typescript(source, ext == "tsx"),
        "py" => extract_python(source),
        _ => Vec::new(),
    }
}

/// Build a parser for the given language. Returns `None` if the
/// language couldn't be set on the parser (extremely rare — would
/// indicate an ABI mismatch between `tree-sitter` and the grammar crate).
fn make_parser(language: tree_sitter::Language) -> Option<tree_sitter::Parser> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    Some(parser)
}

// -------- Rust --------

pub fn extract_rust(source: &str) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let Some(mut parser) = make_parser(lang) else {
        return Vec::new();
    };
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut out: Vec<Symbol> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    walk_rust(root, source.as_bytes(), None, &mut out, &mut seen);
    out
}

/// Recursive walker for Rust. `impl_type` is `Some(TypeName)` when we're
/// inside an `impl Foo` body (so child `function_item`s become `Foo::name`).
/// Trait body items get the same `Trait::name` treatment.
fn walk_rust(
    node: tree_sitter::Node,
    src: &[u8],
    impl_type: Option<&str>,
    out: &mut Vec<Symbol>,
    seen: &mut HashSet<String>,
) {
    let kind = node.kind();

    match kind {
        // Free function: emit as `name` (or as `Type::name` if we're inside
        // an impl/trait body).
        "function_item" | "function_signature_item" => {
            if let Some(name) = child_name_field(node, src, "name") {
                let symbol_name = match impl_type {
                    Some(ty) => format!("{ty}::{name}"),
                    None => name,
                };
                push_unique(node, symbol_name, out, seen);
            }
        }
        "struct_item" | "enum_item" | "trait_item" | "union_item" => {
            if let Some(name) = child_name_field(node, src, "name") {
                push_unique(node, name, out, seen);
            }
        }
        "mod_item" => {
            if let Some(name) = child_name_field(node, src, "name") {
                push_unique(node, name, out, seen);
            }
        }
        "type_item" => {
            // `type Foo = ...;` — surface so renames are visible.
            if let Some(name) = child_name_field(node, src, "name") {
                push_unique(node, name, out, seen);
            }
        }
        "impl_item" => {
            // Resolve the impl's primary type so child methods inherit
            // the `Foo::` qualifier. `field_name` is `type` on
            // `impl_item`. Skip the trait impl's "trait" sub-field — we
            // qualify by the *struct* the impl is for, not the trait.
            let ty_text = node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(src).ok())
                .map(strip_generics_and_lifetime);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_rust(child, src, ty_text.as_deref(), out, seen);
            }
            return;
        }
        _ => {}
    }

    // For trait body, attach the trait name as qualifier so signature
    // items inside `trait Foo { fn bar(); }` become `Foo::bar`.
    // (The trait name itself was pushed by the `trait_item` arm above.)
    if kind == "trait_item" {
        let trait_name = child_name_field(node, src, "name");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_rust(child, src, trait_name.as_deref(), out, seen);
        }
        return;
    }

    // Default: recurse without changing qualifier.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_rust(child, src, impl_type, out, seen);
    }
}

/// Extract `<TypeName>` from an `impl_item`'s `type` field text. Drops
/// generics (`Foo<T>` → `Foo`), lifetimes (`&'a Foo` → `Foo`), and
/// reference markers (`&mut Foo` → `Foo`). Cheap string manipulation —
/// the qualifier only feeds resource_key names, not type-system queries.
fn strip_generics_and_lifetime(raw: &str) -> String {
    let trimmed = raw.trim();
    // Drop leading `&`, `&mut `, `*const `, `*mut `.
    let without_ref = trimmed
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches("*const ")
        .trim_start_matches("*mut ")
        .trim_start();
    // Drop lifetime like `'a `.
    let no_lifetime = if let Some(rest) = without_ref.strip_prefix('\'') {
        rest.split_once(' ').map(|(_, r)| r).unwrap_or(rest).trim()
    } else {
        without_ref
    };
    // Drop generics: everything from first `<` on.
    let base = match no_lifetime.find('<') {
        Some(idx) => &no_lifetime[..idx],
        None => no_lifetime,
    };
    base.trim().to_string()
}

// -------- TypeScript / TSX --------

pub fn extract_typescript(source: &str, is_tsx: bool) -> Vec<Symbol> {
    let lang: tree_sitter::Language = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    let Some(mut parser) = make_parser(lang) else {
        return Vec::new();
    };
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk_ts(root, source.as_bytes(), false, &mut out, &mut seen);
    out
}

fn walk_ts(
    node: tree_sitter::Node,
    src: &[u8],
    parent_is_exported: bool,
    out: &mut Vec<Symbol>,
    seen: &mut HashSet<String>,
) {
    let kind = node.kind();

    // `export_statement` wraps an inner declaration. Children inherit
    // the "is exported" flag.
    if kind == "export_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_ts(child, src, true, out, seen);
        }
        return;
    }

    match kind {
        "function_declaration" | "generator_function_declaration" if parent_is_exported => {
            if let Some(name) = child_name_field(node, src, "name") {
                push_unique(node, name, out, seen);
            }
        }
        "class_declaration" if parent_is_exported => {
            if let Some(class_name) = child_name_field(node, src, "name") {
                push_unique(node, class_name.clone(), out, seen);
                // Walk class body for methods → `Class.method`.
                if let Some(body) = node.child_by_field_name("body") {
                    emit_class_methods(body, src, &class_name, out, seen);
                }
                // Don't recurse further — we already handled methods.
                return;
            }
        }
        "interface_declaration" | "type_alias_declaration" | "enum_declaration"
            if parent_is_exported =>
        {
            if let Some(name) = child_name_field(node, src, "name") {
                push_unique(node, name, out, seen);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ts(child, src, false, out, seen);
    }
}

fn emit_class_methods(
    body: tree_sitter::Node,
    src: &[u8],
    class_name: &str,
    out: &mut Vec<Symbol>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "method_definition" {
            if let Some(name) = child_name_field(child, src, "name") {
                push_unique(child, format!("{class_name}.{name}"), out, seen);
            }
        }
    }
}

// -------- Python --------

pub fn extract_python(source: &str) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    let Some(mut parser) = make_parser(lang) else {
        return Vec::new();
    };
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk_py(root, source.as_bytes(), None, &mut out, &mut seen);
    out
}

fn walk_py(
    node: tree_sitter::Node,
    src: &[u8],
    class_ctx: Option<&str>,
    out: &mut Vec<Symbol>,
    seen: &mut HashSet<String>,
) {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            if let Some(name) = child_name_field(node, src, "name") {
                let symbol_name = match class_ctx {
                    Some(c) => format!("{c}.{name}"),
                    None => name,
                };
                push_unique(node, symbol_name, out, seen);
            }
            // Don't recurse into nested function bodies — closures inside
            // a function body aren't useful for claim-tracking and would
            // produce false-positive churn.
            return;
        }
        "class_definition" => {
            if let Some(class_name) = child_name_field(node, src, "name") {
                push_unique(node, class_name.clone(), out, seen);
                // Walk class body to extract methods.
                if let Some(body) = node.child_by_field_name("body") {
                    walk_py(body, src, Some(&class_name), out, seen);
                }
                return;
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_py(child, src, class_ctx, out, seen);
    }
}

// -------- shared helpers --------

fn child_name_field(node: tree_sitter::Node, src: &[u8], field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.to_string())
}

fn push_unique(
    node: tree_sitter::Node,
    name: String,
    out: &mut Vec<Symbol>,
    seen: &mut HashSet<String>,
) {
    // Dedup on (name + start_line) so two methods with the same simple
    // name in distinct impl blocks stay distinct. The `seen` set keys on
    // a synthetic `name@line` because two impl blocks with the same type
    // could be split across the file (rare but legal Rust).
    let row = node.start_position().row as u32;
    let end_row = node.end_position().row as u32;
    let key = format!("{name}@{row}");
    if seen.insert(key) {
        out.push(Symbol {
            name,
            start_line: row,
            end_line: end_row,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(syms: &[Symbol]) -> Vec<&str> {
        let mut v: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn rust_extracts_free_function() {
        let src = "fn foo() {} fn bar() {}\n";
        let syms = extract_rust(src);
        assert_eq!(names(&syms), vec!["bar", "foo"]);
    }

    #[test]
    fn rust_extracts_struct_and_enum() {
        let src = "struct Foo; enum Bar { A, B }\n";
        let syms = extract_rust(src);
        assert_eq!(names(&syms), vec!["Bar", "Foo"]);
    }

    #[test]
    fn rust_extracts_impl_methods_qualified() {
        let src = "struct Foo;\nimpl Foo { fn bar(&self) {} fn baz(&self) {} }\n";
        let syms = extract_rust(src);
        let ns = names(&syms);
        assert!(ns.contains(&"Foo"), "got {ns:?}");
        assert!(ns.contains(&"Foo::bar"), "got {ns:?}");
        assert!(ns.contains(&"Foo::baz"), "got {ns:?}");
    }

    #[test]
    fn rust_extracts_trait_signature_items_qualified() {
        let src = "trait Greeter { fn hello(&self); fn bye(&self); }\n";
        let syms = extract_rust(src);
        let ns = names(&syms);
        assert!(ns.contains(&"Greeter"), "got {ns:?}");
        assert!(ns.contains(&"Greeter::hello"), "got {ns:?}");
        assert!(ns.contains(&"Greeter::bye"), "got {ns:?}");
    }

    #[test]
    fn rust_strips_generics_from_impl_type() {
        let src = "struct Foo<T>(T);\nimpl<T> Foo<T> { fn id(self) -> T { self.0 } }\n";
        let syms = extract_rust(src);
        let ns = names(&syms);
        // The struct itself surfaces as `Foo` (struct_item's name field
        // is "Foo", not "Foo<T>"). The impl's method qualifier is also
        // `Foo` after generics-stripping.
        assert!(ns.contains(&"Foo::id"), "got {ns:?}");
    }

    #[test]
    fn rust_partial_parse_still_extracts_what_it_can() {
        // Half-typed function inside a valid one. tree-sitter should
        // still surface `complete` and may surface `partial` depending
        // on grammar version — either way we don't panic.
        let src = "fn complete() {}\nfn partial( {\n";
        let syms = extract_rust(src);
        assert!(syms.iter().any(|s| s.name == "complete"));
    }

    #[test]
    fn rust_module_item_extracted() {
        let src = "mod submod { fn inner() {} }\n";
        let syms = extract_rust(src);
        let ns = names(&syms);
        assert!(ns.contains(&"submod"), "got {ns:?}");
        // `inner` inside a module is also extracted (we don't restrict to
        // crate-root); this matches Rust's reality that two agents
        // touching `submod::inner` should contend.
        assert!(ns.contains(&"inner"), "got {ns:?}");
    }

    #[test]
    fn ts_extracts_exported_function() {
        let src = "export function foo() {}\nfunction bar() {}\n";
        let syms = extract_typescript(src, false);
        let ns = names(&syms);
        // `bar` is not exported → not extracted.
        assert_eq!(ns, vec!["foo"]);
    }

    #[test]
    fn ts_extracts_exported_class_and_methods() {
        let src = "export class Foo {\n  bar() {}\n  baz() {}\n}\n";
        let syms = extract_typescript(src, false);
        let ns = names(&syms);
        assert!(ns.contains(&"Foo"), "got {ns:?}");
        assert!(ns.contains(&"Foo.bar"), "got {ns:?}");
        assert!(ns.contains(&"Foo.baz"), "got {ns:?}");
    }

    #[test]
    fn ts_extracts_interface_and_type_alias() {
        let src =
            "export interface Foo { x: number; }\nexport type Bar = string;\ninterface Hidden {}\n";
        let syms = extract_typescript(src, false);
        let ns = names(&syms);
        assert!(ns.contains(&"Foo"));
        assert!(ns.contains(&"Bar"));
        // Non-exported interface is filtered out.
        assert!(!ns.contains(&"Hidden"));
    }

    #[test]
    fn tsx_extracts_exported_component_function() {
        let src = "export function MyComp() { return <div/>; }\n";
        let syms = extract_typescript(src, true);
        let ns = names(&syms);
        assert_eq!(ns, vec!["MyComp"]);
    }

    #[test]
    fn py_extracts_function_and_class() {
        let src = "def foo():\n    pass\n\nclass Bar:\n    def baz(self): pass\n";
        let syms = extract_python(src);
        let ns = names(&syms);
        assert!(ns.contains(&"foo"), "got {ns:?}");
        assert!(ns.contains(&"Bar"), "got {ns:?}");
        assert!(ns.contains(&"Bar.baz"), "got {ns:?}");
    }

    #[test]
    fn py_no_nested_fn_recursion() {
        // Local closures shouldn't pollute the symbol set.
        let src = "def outer():\n    def inner():\n        pass\n    return inner\n";
        let syms = extract_python(src);
        let ns = names(&syms);
        assert!(ns.contains(&"outer"));
        assert!(!ns.contains(&"inner"));
    }

    #[test]
    fn dispatch_unknown_ext_empty() {
        assert!(extract_symbols("md", "anything").is_empty());
    }

    #[test]
    fn dispatch_by_extension() {
        assert!(!extract_symbols("rs", "fn foo() {}\n").is_empty());
        assert!(!extract_symbols("ts", "export function foo() {}\n").is_empty());
        assert!(!extract_symbols("tsx", "export function Foo(){return <div/>;}\n").is_empty());
        assert!(!extract_symbols("py", "def foo(): pass\n").is_empty());
    }

    #[test]
    fn strip_generics_helpers() {
        assert_eq!(strip_generics_and_lifetime("Foo"), "Foo");
        assert_eq!(strip_generics_and_lifetime("Foo<T>"), "Foo");
        assert_eq!(strip_generics_and_lifetime("Foo<T, U>"), "Foo");
        assert_eq!(strip_generics_and_lifetime("&Foo"), "Foo");
        assert_eq!(strip_generics_and_lifetime("&mut Foo"), "Foo");
        assert_eq!(strip_generics_and_lifetime("&'a Foo"), "Foo");
    }
}

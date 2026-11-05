//! The data flow [`super::scan`] follows inside one function: which values
//! hold a port-0 socket, which carry its port, and where each socket is moved,
//! released or kept. See the parent module for what counts as a release.

use super::{
    attribute, first_brace, fn_body_index, ident_text, item_end, skip_qualifiers, split, Release,
    Shape,
};
use crate::source_scan::{TokenKind, TokenTree};
use std::collections::{BTreeSet, HashMap};

/// What a same-file helper function hands its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Helper {
    /// A fresh holder.
    Holder,
    /// A tuple of `arity` holding a fresh holder at `holder` and its port at `ports`.
    Pair {
        arity: usize,
        holder: usize,
        ports: Vec<usize>,
    },
}

type ResId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Held,
    /// Moved somewhere the scan does not follow: never reported.
    Escaped,
    Released(usize),
}

#[derive(Clone, Debug)]
struct Resource {
    bound_at: usize,
    state: State,
    /// The scope depth of the binding that holds it; it is released when that
    /// scope ends.
    depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    Holder(ResId),
    Port(BTreeSet<ResId>),
    /// An address with port 0, for a later `bind(addr)`.
    ZeroAddr,
    Other,
}

/// What an expression's value carries.
#[derive(Clone, Debug, Default)]
struct Value {
    ports: BTreeSet<ResId>,
    /// The value IS this holder: a fresh bind, a helper's holder, or a holder
    /// named bare.
    holder: Option<ResId>,
    /// The holder named bare, when it came from a binding (moved out of it).
    from_binding: Option<String>,
    /// The value is a pair helper's tuple: (its holder, the helper's shape).
    pair: Option<(ResId, Helper)>,
    zero_addr: bool,
}

impl Value {
    fn absorb(&mut self, other: Value) {
        self.ports.extend(other.ports);
        self.zero_addr |= other.zero_addr;
    }
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    /// Inside a `move` closure or an `async move` block: a holder named here is
    /// moved into it.
    moving: bool,
    /// Inside a formatting macro's arguments: they are borrowed, and a string
    /// literal's `{name}` is a use of `name`.
    formatting: bool,
}

/// Macros that only borrow their arguments and read `{name}` in their format
/// string.
const FORMATTING_MACROS: [&str; 27] = [
    "format",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "format_args",
    "unreachable",
    "todo",
    "unimplemented",
    "info",
    "warn",
    "error",
    "debug",
    "trace",
    "anyhow",
    "bail",
    "ensure",
    "matches",
];

/// Macros that end the path through the code that invokes them.
const DIVERGING_MACROS: [&str; 4] = ["panic", "unreachable", "todo", "unimplemented"];

const KEYWORDS: [&str; 38] = [
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where",
];

pub(super) struct Analyzer<'h> {
    function: String,
    helpers: &'h HashMap<String, Option<Helper>>,
    resources: Vec<Resource>,
    scopes: Vec<Vec<(String, Binding)>>,
    /// Holders created by the statement being analysed that no binding has
    /// taken yet: a temporary is released at the end of its statement.
    temporaries: Vec<ResId>,
    pub(super) releases: Vec<Release>,
    reported: BTreeSet<ResId>,
    /// Functions declared inside the bodies analysed, for the caller to scan.
    pub(super) nested: Vec<(String, &'h [TokenTree<'h>], usize)>,
    /// Scope depth a `return` drops down to: the function's, or a closure's.
    return_floor: usize,
}

/// Where a statement ends, and how.
struct Stmt {
    start: usize,
    end: usize,
    kind: StmtKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StmtKind {
    Item,
    Let,
    /// An expression statement; `terminated` when a `;` ends it.
    Expr {
        terminated: bool,
    },
}

impl<'h> Analyzer<'h> {
    pub(super) fn new(function: &str, helpers: &'h HashMap<String, Option<Helper>>) -> Self {
        Analyzer {
            function: function.to_string(),
            helpers,
            resources: Vec::new(),
            scopes: Vec::new(),
            temporaries: Vec::new(),
            releases: Vec::new(),
            reported: BTreeSet::new(),
            nested: Vec::new(),
            return_floor: 0,
        }
    }

    /// Analyse a function body. With `helper_shape`, report what its value hands
    /// a caller (pass 1); otherwise `None`.
    pub(super) fn function<'t>(
        &mut self,
        body: &'h [TokenTree<'t>],
        close_line: usize,
        helper_shape: bool,
    ) -> Option<Helper>
    where
        't: 'h,
    {
        self.scopes.push(Vec::new());
        self.return_floor = self.scopes.len();
        let (value, tail) = self.block(body, close_line, Ctx::default());
        let shape = helper_shape
            .then(|| self.helper_shape(&value, tail))
            .flatten();
        self.scopes.pop();
        shape
    }

    /// A helper's shape from its body's value. Uses the tail's own trees for a
    /// tuple, since the tuple's evaluation moves its holder.
    fn helper_shape(&self, value: &Value, tail: Option<HelperTail>) -> Option<Helper> {
        if let Some(HelperTail::Tuple(elements)) = tail {
            let holder = elements
                .iter()
                .position(|e| matches!(e, Some(Binding::Holder(_))))?;
            let Some(Binding::Holder(r)) = elements[holder] else {
                return None;
            };
            let ports: Vec<usize> = elements
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e, Some(Binding::Port(set)) if set.contains(&r)))
                .map(|(k, _)| k)
                .collect();
            return (!ports.is_empty()).then_some(Helper::Pair {
                arity: elements.len(),
                holder,
                ports,
            });
        }
        value.holder.is_some().then_some(Helper::Holder)
    }

    /// How many port-0 binds (helper holders included) the analysis followed.
    pub(super) fn binds(&self) -> usize {
        self.resources.len()
    }

    fn depth(&self) -> usize {
        self.scopes.len()
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|(n, _)| n == name)
            .map(|(_, b)| b)
    }

    fn bind(&mut self, name: &str, binding: Binding) {
        if let Binding::Holder(r) = binding {
            self.resources[r].depth = self.depth();
            self.temporaries.retain(|t| *t != r);
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.push((name.to_string(), binding));
        }
    }

    /// A binding moved out of: it holds nothing any more.
    fn moved_out_of(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(entry) = scope.iter_mut().rev().find(|(n, _)| n == name) {
                entry.1 = Binding::Other;
                return;
            }
        }
    }

    fn new_resource(&mut self, line: usize) -> ResId {
        self.resources.push(Resource {
            bound_at: line,
            state: State::Held,
            depth: self.depth(),
        });
        let r = self.resources.len() - 1;
        self.temporaries.push(r);
        r
    }

    fn escape(&mut self, r: ResId) {
        if self.resources[r].state == State::Held {
            self.resources[r].state = State::Escaped;
        }
        self.temporaries.retain(|t| *t != r);
    }

    fn release(&mut self, r: ResId, line: usize) {
        if self.resources[r].state == State::Held {
            self.resources[r].state = State::Released(line);
        }
        self.temporaries.retain(|t| *t != r);
    }

    fn report(&mut self, r: ResId, line: usize, shape: Shape) {
        if self.reported.insert(r) {
            self.releases.push(Release {
                function: self.function.clone(),
                bound_at: self.resources[r].bound_at,
                line,
                shape,
            });
        }
    }

    /// Release the temporaries a statement created and no binding took.
    fn end_statement(&mut self, line: usize) {
        for r in std::mem::take(&mut self.temporaries) {
            if self.resources[r].state == State::Held {
                self.resources[r].state = State::Released(line);
            }
        }
    }

    /// Analyse a block: its statements in a new scope, then its tail. Returns
    /// the block's value and, for a helper, its tail.
    fn block<'t>(
        &mut self,
        items: &'h [TokenTree<'t>],
        close_line: usize,
        ctx: Ctx,
    ) -> (Value, Option<HelperTail>)
    where
        't: 'h,
    {
        self.scopes.push(Vec::new());
        let depth = self.depth();
        let stmts = statements(items);
        let mut value = Value::default();
        let mut tail = None;
        let mut tail_line = close_line;
        for (k, stmt) in stmts.iter().enumerate() {
            let trees = &items[stmt.start..stmt.end];
            match stmt.kind {
                StmtKind::Item => self.nested_item(trees),
                StmtKind::Let => {
                    self.let_statement(trees, ctx);
                    self.end_statement(trees.last().map_or(close_line, TokenTree::end_line));
                }
                StmtKind::Expr { terminated } => {
                    let is_tail = k + 1 == stmts.len() && !terminated;
                    if is_tail {
                        tail = Some(self.helper_tail(trees));
                        tail_line = trees.first().map_or(close_line, TokenTree::line);
                    }
                    let assigns = trees
                        .first()
                        .and_then(TokenTree::leaf)
                        .is_some_and(|t| t.kind == TokenKind::Ident && !KEYWORDS.contains(&t.text));
                    let v = match trees.iter().position(|t| t.is_punct("=")) {
                        Some(eq) if assigns && !is_tail => {
                            self.assignment(&trees[..eq], &trees[eq + 1..], ctx);
                            Value::default()
                        }
                        _ => self.expr(trees, ctx),
                    };
                    if is_tail {
                        value = v;
                    } else {
                        self.end_statement(trees.last().map_or(close_line, TokenTree::end_line));
                    }
                }
            }
        }
        // The value is computed before the block's locals are dropped, so a
        // port in it outlives a holder that dies here.
        let dying: Vec<ResId> = (0..self.resources.len())
            .filter(|&r| self.resources[r].state == State::Held && self.resources[r].depth >= depth)
            .filter(|&r| value.holder != Some(r))
            .collect();
        for &r in &dying {
            if value.ports.contains(&r) {
                self.report(r, tail_line, Shape::ValueOutlivesHolder);
            }
        }
        if let Some(r) = value.holder {
            // Moving out as the block's value, to whatever takes it.
            self.resources[r].depth = depth - 1;
            if !self.temporaries.contains(&r) {
                self.temporaries.push(r);
            }
        }
        for r in dying {
            self.release(r, close_line);
        }
        self.scopes.pop();
        (value, tail.flatten())
    }

    /// For pass 1: what the tail hands out, while its bindings are still in scope.
    fn helper_tail(&self, trees: &[TokenTree<'_>]) -> Option<HelperTail> {
        let [group] = trees else {
            return None;
        };
        if group.delimiter() != Some(b'(') {
            return None;
        }
        let elements = split(group.inner(), ",");
        (elements.len() >= 2).then(|| {
            HelperTail::Tuple(
                elements
                    .iter()
                    .map(|e| match e {
                        [one] => one
                            .leaf()
                            .filter(|t| t.kind == TokenKind::Ident)
                            .and_then(|t| self.lookup(t.text).cloned()),
                        _ => None,
                    })
                    .collect(),
            )
        })
    }

    /// A function declared inside a body: scanned on its own, as test code.
    fn nested_item<'t>(&mut self, trees: &'h [TokenTree<'t>])
    where
        't: 'h,
    {
        let keyword = skip_qualifiers(trees, 0);
        if trees.get(keyword).is_some_and(|t| t.is_ident("fn")) {
            if let (Some(name), Some(body)) = (
                ident_text(trees.get(keyword + 1)),
                fn_body_index(trees, keyword + 2),
            ) {
                self.nested
                    .push((name, trees[body].inner(), trees[body].end_line()));
            }
        }
    }

    /// `let PATTERN [: TYPE] [= INIT [else {…}]];`
    fn let_statement<'t>(&mut self, trees: &'h [TokenTree<'t>], ctx: Ctx)
    where
        't: 'h,
    {
        let body = match trees.last() {
            Some(last) if last.is_punct(";") => &trees[..trees.len() - 1],
            _ => trees,
        };
        let eq = body.iter().position(|t| t.is_punct("="));
        let pattern_end = body[1..eq.unwrap_or(body.len())]
            .iter()
            .position(|t| t.is_punct(":"))
            .map_or(eq.unwrap_or(body.len()), |c| c + 1);
        let pattern = &body[1..pattern_end];
        let Some(eq) = eq else {
            for name in pattern_names(pattern) {
                self.bind(&name, Binding::Other);
            }
            return;
        };
        let rest = &body[eq + 1..];
        let (init, else_block) = match rest.iter().position(|t| t.is_ident("else")) {
            Some(e) if rest.get(e + 1).is_some_and(|g| g.delimiter() == Some(b'{')) => {
                (&rest[..e], Some(&rest[e + 1]))
            }
            _ => (rest, None),
        };

        // `let x = x.adapter(…)`: the new binding holds what the old one did.
        let rebind = match (pattern_names(pattern).as_slice(), init) {
            ([name], [receiver, dot, method, ..])
                if receiver.is_ident(name)
                    && dot.is_punct(".")
                    && !method.is_ident("local_addr") =>
            {
                match self.lookup(name) {
                    Some(Binding::Holder(r)) => Some(*r),
                    _ => None,
                }
            }
            _ => None,
        };

        let value = self.expr(init, ctx);
        if let Some(group) = else_block {
            let snapshot = self.states();
            let (_, _) = self.block(group.inner(), group.end_line(), ctx);
            self.restore(snapshot);
        }
        let line = trees.first().map_or(0, TokenTree::line);
        self.bind_pattern(pattern, value, rebind, line);
    }

    fn bind_pattern(
        &mut self,
        pattern: &[TokenTree<'_>],
        value: Value,
        rebind: Option<ResId>,
        line: usize,
    ) {
        let pattern = strip_binding_modes(pattern);
        if let Some(r) = rebind {
            if let [name] = pattern {
                if let Some(text) = name.leaf().map(|t| t.text) {
                    self.bind(text, Binding::Holder(r));
                }
                return;
            }
        }
        match pattern {
            [single] if single.leaf().is_some_and(|t| t.kind == TokenKind::Ident) => {
                let name = single.leaf().map_or("", |t| t.text);
                if name == "_" {
                    // `let _ = bind(…)` drops the value at once; `let _ = holder;`
                    // neither moves nor drops it.
                    if let (Some(r), None) = (value.holder, &value.from_binding) {
                        self.release(r, line);
                    }
                    return;
                }
                if let Some(r) = value.holder {
                    if let Some(from) = &value.from_binding {
                        self.moved_out_of(from);
                    }
                    self.bind(name, Binding::Holder(r));
                } else if let Some((r, _)) = value.pair {
                    // A pair not taken apart: where its holder goes is not followed.
                    self.escape(r);
                    self.bind(name, Binding::Other);
                } else if !value.ports.is_empty() {
                    self.bind(name, Binding::Port(value.ports));
                } else if value.zero_addr {
                    self.bind(name, Binding::ZeroAddr);
                } else {
                    self.bind(name, Binding::Other);
                }
            }
            [group] if group.delimiter() == Some(b'(') => {
                let elements = split(group.inner(), ",");
                match value.pair {
                    Some((
                        r,
                        Helper::Pair {
                            arity,
                            holder,
                            ports,
                        },
                    )) if arity == elements.len() => {
                        for (k, element) in elements.iter().enumerate() {
                            let names = pattern_names(element);
                            let wildcard =
                                matches!(strip_binding_modes(element), [w] if w.is_ident("_"));
                            if k == holder {
                                match names.as_slice() {
                                    [name] => self.bind(name, Binding::Holder(r)),
                                    _ if wildcard => self.release(r, line),
                                    _ => self.escape(r),
                                }
                            } else if ports.contains(&k) {
                                for name in names {
                                    self.bind(&name, Binding::Port(BTreeSet::from([r])));
                                }
                            } else {
                                for name in names {
                                    self.bind(&name, Binding::Other);
                                }
                            }
                        }
                    }
                    pair => {
                        if let Some((r, _)) = pair {
                            self.escape(r);
                        }
                        if let Some(r) = value.holder {
                            self.escape(r);
                        }
                        self.bind_names(pattern, &value.ports);
                    }
                }
            }
            _ => {
                if let Some(r) = value.holder {
                    self.escape(r);
                }
                if let Some((r, _)) = value.pair {
                    self.escape(r);
                }
                self.bind_names(pattern, &value.ports);
            }
        }
    }

    /// Bind every name in a pattern: to the value's ports, or to nothing.
    fn bind_names(&mut self, pattern: &[TokenTree<'_>], ports: &BTreeSet<ResId>) {
        for name in pattern_names(pattern) {
            let binding = if ports.is_empty() {
                Binding::Other
            } else {
                Binding::Port(ports.clone())
            };
            self.bind(&name, binding);
        }
    }

    fn states(&self) -> Vec<State> {
        self.resources.iter().map(|r| r.state).collect()
    }

    fn restore(&mut self, snapshot: Vec<State>) {
        for (r, state) in snapshot.into_iter().enumerate() {
            self.resources[r].state = state;
        }
    }

    /// Analyse an expression and return its value.
    fn expr<'t>(&mut self, trees: &'h [TokenTree<'t>], ctx: Ctx) -> Value
    where
        't: 'h,
    {
        let mut value = Value::default();
        let mut i = 0;
        while i < trees.len() {
            let tree = &trees[i];
            let prev = i.checked_sub(1).map(|p| &trees[p]);
            match tree {
                TokenTree::Leaf(token) if token.kind == TokenKind::Ident => {
                    match token.text {
                        "if" | "while" => {
                            i = self.conditional(trees, i, ctx, &mut value);
                            continue;
                        }
                        "match" => {
                            i = self.match_expr(trees, i, ctx, &mut value);
                            continue;
                        }
                        "for" => {
                            i = self.for_loop(trees, i, ctx, &mut value);
                            continue;
                        }
                        "loop" | "unsafe"
                            if trees
                                .get(i + 1)
                                .is_some_and(|g| g.delimiter() == Some(b'{')) =>
                        {
                            let group = &trees[i + 1];
                            let (v, _) = self.block(group.inner(), group.end_line(), ctx);
                            value.absorb(v);
                            i += 2;
                            continue;
                        }
                        "async" => {
                            let moving = trees.get(i + 1).is_some_and(|t| t.is_ident("move"));
                            let at = i + 1 + usize::from(moving);
                            if let Some(group) =
                                trees.get(at).filter(|g| g.delimiter() == Some(b'{'))
                            {
                                let inner = Ctx {
                                    moving: ctx.moving || moving,
                                    ..ctx
                                };
                                let (v, _) = self.block(group.inner(), group.end_line(), inner);
                                value.absorb(v);
                                i = at + 1;
                                continue;
                            }
                            if moving
                                && trees
                                    .get(at)
                                    .is_some_and(|t| t.is_punct("|") || t.is_punct("||"))
                            {
                                i = self.closure(
                                    trees,
                                    at,
                                    Ctx {
                                        moving: true,
                                        ..ctx
                                    },
                                    &mut value,
                                );
                                continue;
                            }
                            i += 1;
                            continue;
                        }
                        "move"
                            if trees
                                .get(i + 1)
                                .is_some_and(|t| t.is_punct("|") || t.is_punct("||")) =>
                        {
                            i = self.closure(
                                trees,
                                i + 1,
                                Ctx {
                                    moving: true,
                                    ..ctx
                                },
                                &mut value,
                            );
                            continue;
                        }
                        "return" => {
                            let returned = self.expr(&trees[i + 1..], ctx);
                            self.returned(&returned, token.line);
                            return value;
                        }
                        _ => {}
                    }
                    // A macro invocation: `name!(…)`.
                    if let (Some(bang), Some(group)) = (trees.get(i + 1), trees.get(i + 2)) {
                        if bang.is_punct("!") && group.delimiter().is_some() {
                            let v = self.macro_call(token.text, group, ctx);
                            value.absorb(v);
                            i += 3;
                            continue;
                        }
                    }
                    let after_dot = prev.is_some_and(|p| p.is_punct("."));
                    let path_segment = prev.is_some_and(|p| p.is_punct("::"))
                        || trees.get(i + 1).is_some_and(|n| n.is_punct("::"));
                    if token.text == "bind" && (after_dot || prev.is_some_and(|p| p.is_punct("::")))
                    {
                        if let Some(args) = trees.get(i + 1).filter(|g| g.delimiter() == Some(b'('))
                        {
                            if self.is_zero_address(args.inner()) {
                                i = self.bind_call(trees, i, after_dot, &mut value);
                                continue;
                            }
                        }
                    }
                    let call = trees.get(i + 1).filter(|g| g.delimiter() == Some(b'('));
                    let drop_or_forget = matches!(token.text, "drop" | "forget") && !after_dot;
                    if let Some(call) = call.filter(|_| drop_or_forget || !path_segment) {
                        if !after_dot
                            && !KEYWORDS.contains(&token.text)
                            && self.call(token.text, call, ctx, &mut value)
                        {
                            i += 2;
                            continue;
                        }
                    }
                    if after_dot || path_segment || KEYWORDS.contains(&token.text) {
                        i += 1;
                        continue;
                    }
                    self.use_name(trees, i, ctx, &mut value);
                    i += 1;
                }
                TokenTree::Leaf(token) if token.kind == TokenKind::Str => {
                    if ctx.formatting {
                        self.format_uses(token.text, token.line, &mut value);
                    }
                    if is_zero_address_literal(token.text) {
                        value.zero_addr = true;
                    }
                    i += 1;
                }
                TokenTree::Leaf(token)
                    if token.kind == TokenKind::Punct
                        && (token.text == "|" || token.text == "||")
                        && starts_expression(prev) =>
                {
                    i = self.closure(trees, i, ctx, &mut value);
                }
                TokenTree::Group { open, inner, close } => {
                    match open.text {
                        "(" if is_call(prev) => {
                            for arg in split(inner, ",") {
                                let v = self.expr(arg, ctx);
                                self.moved_into_call(&v);
                                value.absorb(v);
                            }
                        }
                        "(" => {
                            let elements = split(inner, ",");
                            if let [single] = elements.as_slice() {
                                let v = self.expr(single, ctx);
                                value.holder = v.holder.or(value.holder);
                                value.from_binding =
                                    v.from_binding.clone().or(value.from_binding.take());
                                value.pair = v.pair.clone().or(value.pair.take());
                                value.absorb(v);
                            } else {
                                for element in elements {
                                    let v = self.expr(element, ctx);
                                    self.moved_into_call(&v);
                                    value.absorb(v);
                                }
                            }
                        }
                        "[" => {
                            for element in inner.split(|t| t.is_punct(",") || t.is_punct(";")) {
                                let v = self.expr(element, ctx);
                                self.moved_into_call(&v);
                                value.absorb(v);
                            }
                        }
                        _ if is_struct_literal(prev) => {
                            self.struct_literal(inner, ctx, &mut value);
                        }
                        _ => {
                            let (v, _) = self.block(inner, close.line, ctx);
                            value.holder = v.holder.or(value.holder);
                            value.absorb(v);
                        }
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        value
    }

    /// `place = value;`: a port assigned into `x` or `x.field` makes `x` carry it
    /// (`config.port = probe.local_addr()?.port()`); a holder assigned anywhere is
    /// moved where the scan does not follow it.
    fn assignment<'t>(&mut self, place: &'h [TokenTree<'t>], rhs: &'h [TokenTree<'t>], ctx: Ctx)
    where
        't: 'h,
    {
        // The place's own index expressions are uses too (`v[port] = …`).
        let _ = self.expr(place.get(1..).unwrap_or(&[]), ctx);
        let value = self.expr(rhs, ctx);
        self.moved_into_call(&value);
        let Some(root) = place
            .first()
            .and_then(TokenTree::leaf)
            .filter(|t| t.kind == TokenKind::Ident)
        else {
            return;
        };
        if value.ports.is_empty() {
            return;
        }
        let name = root.text;
        let merged = match self.lookup(name) {
            Some(Binding::Port(existing)) => existing.union(&value.ports).copied().collect(),
            Some(Binding::Other) | None => value.ports.clone(),
            Some(Binding::Holder(_) | Binding::ZeroAddr) => return,
        };
        for scope in self.scopes.iter_mut().rev() {
            if let Some(entry) = scope.iter_mut().rev().find(|(n, _)| n == name) {
                entry.1 = Binding::Port(merged);
                return;
            }
        }
    }

    /// A holder, a pair or a fresh holder passed into a call, a tuple or an
    /// array is moved there: the scan does not follow it further.
    fn moved_into_call(&mut self, v: &Value) {
        if let Some(r) = v.holder {
            self.escape(r);
        }
        if let Some((r, _)) = &v.pair {
            self.escape(*r);
        }
    }

    /// A call `name(args)`: `drop`, `forget` and same-file helpers. Returns
    /// whether it handled the call.
    fn call<'t>(&mut self, name: &str, args: &'h TokenTree<'t>, ctx: Ctx, value: &mut Value) -> bool
    where
        't: 'h,
    {
        let line = args.line();
        match name {
            "drop" | "forget" => {
                let arguments = split(args.inner(), ",");
                if let [[arg]] = arguments.as_slice() {
                    if let Some(Binding::Holder(r)) =
                        arg.leaf().and_then(|t| self.lookup(t.text)).cloned()
                    {
                        if name == "drop" {
                            self.release(r, line);
                        } else {
                            self.escape(r);
                        }
                        return true;
                    }
                }
                false
            }
            _ => match self.helpers.get(name) {
                Some(Some(helper)) => {
                    for arg in split(args.inner(), ",") {
                        let v = self.expr(arg, ctx);
                        self.moved_into_call(&v);
                        value.absorb(v);
                    }
                    let r = self.new_resource(line);
                    match helper {
                        Helper::Holder => value.holder = Some(r),
                        Helper::Pair { .. } => value.pair = Some((r, helper.clone())),
                    }
                    true
                }
                _ => false,
            },
        }
    }

    /// A bind of a port-0 address at `trees[i]` (`bind`), its arguments next.
    /// Returns the index after the arguments.
    fn bind_call(
        &mut self,
        trees: &[TokenTree<'_>],
        i: usize,
        method: bool,
        value: &mut Value,
    ) -> usize {
        let line = trees[i].line();
        if method {
            // `sock.bind(&addr)`: the receiver becomes the holder.
            let receiver = i
                .checked_sub(2)
                .and_then(|r| trees[r].leaf())
                .filter(|t| t.kind == TokenKind::Ident)
                .map(|t| t.text.to_string());
            if let Some(name) = receiver {
                let r = self.new_resource(line);
                let depth = self
                    .scopes
                    .iter()
                    .rposition(|scope| scope.iter().any(|(n, _)| *n == name))
                    .map_or(self.depth(), |s| s + 1);
                self.resources[r].depth = depth;
                self.temporaries.retain(|t| *t != r);
                for scope in self.scopes.iter_mut().rev() {
                    if let Some(entry) = scope.iter_mut().rev().find(|(n, _)| *n == name) {
                        entry.1 = Binding::Holder(r);
                        break;
                    }
                }
            }
            return i + 2;
        }
        // `TcpListener::bind(addr)`: the value is a fresh holder, or, read through
        // `.local_addr()` in the same chain, its port from a temporary.
        let r = self.new_resource(line);
        if chain_reads_local_addr(&trees[i + 2..]) {
            value.ports.insert(r);
        } else {
            value.holder = Some(r);
        }
        i + 2
    }

    /// Whether a bind's arguments name a port-0 address.
    fn is_zero_address(&self, args: &[TokenTree<'_>]) -> bool {
        args.iter().any(|t| match t {
            TokenTree::Leaf(token) if token.kind == TokenKind::Str => is_zero_address_literal(token.text),
            TokenTree::Leaf(token) if token.kind == TokenKind::Ident => {
                self.lookup(token.text) == Some(&Binding::ZeroAddr)
            }
            TokenTree::Group { inner, .. } => {
                let elements = split(inner, ",");
                matches!(elements.as_slice(), [_, [zero]] if zero.leaf().is_some_and(|z| z.kind == TokenKind::Number && z.text == "0"))
                    || self.is_zero_address(inner)
            }
            TokenTree::Leaf(_) => false,
        })
    }

    /// A name used in an expression.
    fn use_name(&mut self, trees: &[TokenTree<'_>], i: usize, ctx: Ctx, value: &mut Value) {
        let token = trees[i].leaf().copied();
        let Some(token) = token else {
            return;
        };
        let Some(binding) = self.lookup(token.text).cloned() else {
            return;
        };
        let next = trees.get(i + 1);
        let borrowed = i
            .checked_sub(1)
            .is_some_and(|p| trees[p].is_punct("&") || trees[p].is_punct("&&"))
            || (i >= 2 && trees[i - 1].is_ident("mut") && trees[i - 2].is_punct("&"));
        match binding {
            Binding::Holder(r) => {
                if next.is_some_and(|n| n.is_punct(".")) {
                    if trees.get(i + 2).is_some_and(|m| m.is_ident("local_addr")) {
                        value.ports.insert(r);
                    }
                    if ctx.moving {
                        self.escape(r);
                    }
                } else if ctx.moving {
                    self.escape(r);
                } else if !borrowed && !ctx.formatting {
                    value.holder = Some(r);
                    value.from_binding = Some(token.text.to_string());
                }
            }
            Binding::Port(set) => self.use_port(set, token.line, value),
            Binding::ZeroAddr => value.zero_addr = true,
            Binding::Other => {}
        }
    }

    /// A port value used at `line`: a release if its holder is gone.
    fn use_port(&mut self, set: BTreeSet<ResId>, line: usize, value: &mut Value) {
        for &r in &set {
            if let State::Released(at) = self.resources[r].state {
                self.report(r, line, Shape::UsedAfterRelease { released_at: at });
            }
        }
        value.ports.extend(set);
    }

    /// `{name}` placeholders in a formatting macro's string literal are uses.
    fn format_uses(&mut self, literal: &str, line: usize, value: &mut Value) {
        let mut rest = literal;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(after) = rest.strip_prefix('{') {
                rest = after;
                continue;
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() || name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Some(Binding::Port(set)) = self.lookup(&name).cloned() {
                self.use_port(set, line, value);
            }
        }
    }

    /// `name!(…)`: a formatting macro borrows its arguments; any other macro's
    /// arguments are analysed as expressions that may move.
    fn macro_call<'t>(&mut self, name: &str, group: &'h TokenTree<'t>, ctx: Ctx) -> Value
    where
        't: 'h,
    {
        let formatting = FORMATTING_MACROS.contains(&name);
        let inner = Ctx { formatting, ..ctx };
        let mut value = Value::default();
        for arg in split(group.inner(), ",") {
            let v = self.expr(arg, inner);
            if !formatting {
                self.moved_into_call(&v);
            }
            value.absorb(v);
        }
        value
    }

    /// `|params| body`, `move |params| body`. `trees[at]` is the opening `|` or `||`.
    fn closure<'t>(
        &mut self,
        trees: &'h [TokenTree<'t>],
        at: usize,
        ctx: Ctx,
        value: &mut Value,
    ) -> usize
    where
        't: 'h,
    {
        let (params, body_at) = if trees[at].is_punct("||") {
            (&trees[at..at], at + 1)
        } else {
            let close = (at + 1..trees.len())
                .find(|&j| trees[j].is_punct("|"))
                .unwrap_or(trees.len());
            (&trees[at + 1..close], close + 1)
        };
        let floor = self.return_floor;
        self.scopes.push(Vec::new());
        self.return_floor = self.depth();
        for name in pattern_names(params) {
            self.bind(&name, Binding::Other);
        }
        let end = match trees.get(body_at) {
            Some(group) if group.delimiter() == Some(b'{') => {
                let (v, _) = self.block(group.inner(), group.end_line(), ctx);
                value.absorb(v);
                body_at + 1
            }
            _ => {
                let v = self.expr(&trees[body_at.min(trees.len())..], ctx);
                value.absorb(v);
                trees.len()
            }
        };
        self.scopes.pop();
        self.return_floor = floor;
        end
    }

    /// `return value`: every local of the function (or closure) is dropped, so a
    /// port in the value outlives a holder still held here.
    fn returned(&mut self, value: &Value, line: usize) {
        if let Some(r) = value.holder {
            self.escape(r);
        }
        let floor = self.return_floor;
        let dying: Vec<ResId> = value
            .ports
            .iter()
            .copied()
            .filter(|&r| self.resources[r].state == State::Held && self.resources[r].depth >= floor)
            .collect();
        for r in dying {
            self.report(r, line, Shape::ValueOutlivesHolder);
        }
    }

    /// `if`/`while` CONDITION `{…}` [`else` …]. Returns the index after it.
    fn conditional<'t>(
        &mut self,
        trees: &'h [TokenTree<'t>],
        start: usize,
        ctx: Ctx,
        value: &mut Value,
    ) -> usize
    where
        't: 'h,
    {
        let before = self.states();
        let mut outcomes: Vec<Option<Vec<State>>> = Vec::new();
        let mut i = start;
        let mut has_else = false;
        loop {
            // At `if`/`while`: the condition runs up to the first `{…}`.
            let cond_start = i + 1;
            let Some(body) = first_brace(trees, cond_start) else {
                return trees.len();
            };
            let cond = &trees[cond_start..body];
            self.scopes.push(Vec::new());
            self.condition(cond, ctx);
            let group = &trees[body];
            let (v, _) = self.block(group.inner(), group.end_line(), ctx);
            value.absorb(v);
            self.scopes.pop();
            outcomes.push(self.branch_outcome(group.inner(), &before));
            i = body + 1;
            if trees.get(i).is_some_and(|t| t.is_ident("else")) {
                if trees.get(i + 1).is_some_and(|t| t.is_ident("if")) {
                    i += 1;
                    continue;
                }
                if let Some(group) = trees.get(i + 1).filter(|g| g.delimiter() == Some(b'{')) {
                    has_else = true;
                    let (v, _) = self.block(group.inner(), group.end_line(), ctx);
                    value.absorb(v);
                    outcomes.push(self.branch_outcome(group.inner(), &before));
                    i += 2;
                }
            }
            break;
        }
        if !has_else {
            outcomes.push(Some(before.clone()));
        }
        self.merge(outcomes, before);
        i
    }

    /// A branch's resulting states, `None` when it diverges; the states are reset
    /// to `before` for the next branch.
    fn branch_outcome(&mut self, body: &[TokenTree<'_>], before: &[State]) -> Option<Vec<State>> {
        let after = self.states();
        self.restore(before.to_vec());
        (!diverges(body)).then_some(after)
    }

    /// Merge branch outcomes: released on any path that continues is released,
    /// escaped on any is escaped. All paths diverging leaves the states as they were.
    fn merge(&mut self, outcomes: Vec<Option<Vec<State>>>, before: Vec<State>) {
        let live: Vec<Vec<State>> = outcomes.into_iter().flatten().collect();
        let mut merged = before;
        merged.resize(self.resources.len(), State::Held);
        for (r, slot) in merged.iter_mut().enumerate() {
            let states = live.iter().filter_map(|o| o.get(r).copied());
            let mut released = None;
            let mut escaped = false;
            for state in states {
                match state {
                    State::Released(at) => {
                        released = Some(released.map_or(at, |r: usize| r.min(at)))
                    }
                    State::Escaped => escaped = true,
                    State::Held => {}
                }
            }
            if let Some(at) = released {
                *slot = State::Released(at);
            } else if escaped {
                *slot = State::Escaped;
            }
        }
        self.restore(merged);
    }

    /// An `if`/`while` condition: `let PATTERN = EXPR` binds into the body's scope.
    fn condition<'t>(&mut self, cond: &'h [TokenTree<'t>], ctx: Ctx)
    where
        't: 'h,
    {
        if cond.first().is_some_and(|t| t.is_ident("let")) {
            if let Some(eq) = cond.iter().position(|t| t.is_punct("=")) {
                let v = self.expr(&cond[eq + 1..], ctx);
                if let Some(r) = v.holder {
                    self.escape(r);
                }
                self.bind_names(&cond[1..eq], &v.ports);
                return;
            }
        }
        let v = self.expr(cond, ctx);
        if let Some(r) = v.holder {
            self.escape(r);
        }
    }

    /// `match SCRUTINEE { PATTERN => ARM, … }`. Returns the index after it.
    fn match_expr<'t>(
        &mut self,
        trees: &'h [TokenTree<'t>],
        start: usize,
        ctx: Ctx,
        value: &mut Value,
    ) -> usize
    where
        't: 'h,
    {
        let Some(body) = first_brace(trees, start + 1) else {
            return trees.len();
        };
        let scrutinee = self.expr(&trees[start + 1..body], ctx);
        if let Some(r) = scrutinee.holder {
            self.escape(r);
        }
        let before = self.states();
        let mut outcomes = Vec::new();
        let arms = trees[body].inner();
        let mut i = 0;
        while i < arms.len() {
            let Some(arrow) = (i..arms.len()).find(|&j| arms[j].is_punct("=>")) else {
                break;
            };
            self.scopes.push(Vec::new());
            let pattern_end = (i..arrow)
                .find(|&j| arms[j].is_ident("if"))
                .unwrap_or(arrow);
            self.bind_names(&arms[i..pattern_end], &scrutinee.ports);
            if pattern_end < arrow {
                let guard = self.expr(&arms[pattern_end + 1..arrow], ctx);
                value.absorb(guard);
            }
            let arm_start = arrow + 1;
            let (arm_end, arm_trees): (usize, &[TokenTree<'t>]) = match arms.get(arm_start) {
                Some(group) if group.delimiter() == Some(b'{') => {
                    let (v, _) = self.block(group.inner(), group.end_line(), ctx);
                    value.absorb(v);
                    let end = arm_start
                        + 1
                        + usize::from(arms.get(arm_start + 1).is_some_and(|t| t.is_punct(",")));
                    (end, group.inner())
                }
                _ => {
                    let end = (arm_start..arms.len())
                        .find(|&j| arms[j].is_punct(","))
                        .unwrap_or(arms.len());
                    let v = self.expr(&arms[arm_start..end], ctx);
                    value.absorb(v);
                    (end + 1, &arms[arm_start..end])
                }
            };
            self.scopes.pop();
            outcomes.push(self.branch_outcome(arm_trees, &before));
            i = arm_end;
        }
        self.merge(outcomes, before);
        body + 1
    }

    /// `for PATTERN in EXPR {…}`. Returns the index after it.
    fn for_loop<'t>(
        &mut self,
        trees: &'h [TokenTree<'t>],
        start: usize,
        ctx: Ctx,
        value: &mut Value,
    ) -> usize
    where
        't: 'h,
    {
        let Some(body) = first_brace(trees, start + 1) else {
            return trees.len();
        };
        let in_at = (start + 1..body)
            .find(|&j| trees[j].is_ident("in"))
            .unwrap_or(body);
        let iterated = self.expr(&trees[in_at + 1..body], ctx);
        if let Some(r) = iterated.holder {
            self.escape(r);
        }
        self.scopes.push(Vec::new());
        self.bind_names(&trees[start + 1..in_at], &iterated.ports);
        let group = &trees[body];
        let (v, _) = self.block(group.inner(), group.end_line(), ctx);
        value.absorb(v);
        self.scopes.pop();
        body + 1
    }

    /// `Path { field: value, shorthand, ..base }`: every value is moved in.
    fn struct_literal<'t>(&mut self, inner: &'h [TokenTree<'t>], ctx: Ctx, value: &mut Value)
    where
        't: 'h,
    {
        for field in split(inner, ",") {
            let expr = match field {
                [name, colon, rest @ ..]
                    if name.leaf().is_some_and(|t| t.kind == TokenKind::Ident)
                        && colon.is_punct(":") =>
                {
                    rest
                }
                [dots, rest @ ..] if dots.is_punct("..") => rest,
                whole => whole,
            };
            let v = self.expr(expr, ctx);
            self.moved_into_call(&v);
            value.absorb(v);
        }
    }
}

/// What a helper's tail hands out, read before the tail is evaluated.
enum HelperTail {
    Tuple(Vec<Option<Binding>>),
}

/// The statements of a block's contents.
fn statements(items: &[TokenTree<'_>]) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let start = i;
        // Attributes on a statement.
        while let Some((next, _, _)) = attribute(items, i) {
            i = next;
        }
        if i >= items.len() {
            break;
        }
        if items[i].is_punct(";") {
            i += 1;
            continue;
        }
        let keyword = skip_qualifiers(items, i);
        let is_item = items.get(keyword).is_some_and(|t| {
            [
                "fn",
                "struct",
                "enum",
                "impl",
                "trait",
                "mod",
                "use",
                "static",
                "type",
                "union",
                "macro_rules",
            ]
            .iter()
            .any(|k| t.is_ident(k))
        }) || (items[i].is_ident("const") && keyword == i);
        if is_item {
            let end = if items[keyword].is_ident("fn") {
                fn_body_index(items, keyword + 2)
                    .map_or_else(|| item_end(items, keyword), |b| b + 1)
            } else {
                item_end(items, keyword)
            };
            out.push(Stmt {
                start,
                end,
                kind: StmtKind::Item,
            });
            i = end;
            continue;
        }
        if items[i].is_ident("let") {
            let end = (i..items.len())
                .find(|&j| items[j].is_punct(";"))
                .map_or(items.len(), |j| j + 1);
            out.push(Stmt {
                start,
                end,
                kind: StmtKind::Let,
            });
            i = end;
            continue;
        }
        let end = expression_statement_end(items, i);
        let terminated = items
            .get(end.wrapping_sub(1))
            .is_some_and(|t| t.is_punct(";"));
        out.push(Stmt {
            start,
            end,
            kind: StmtKind::Expr { terminated },
        });
        i = end;
    }
    out
}

/// The end of the expression statement starting at `i`: past its `;`, or past a
/// block-like expression (`if`, `match`, `loop`, `while`, `for`, `unsafe`, a
/// block, a braced macro) that nothing continues.
fn expression_statement_end(items: &[TokenTree<'_>], i: usize) -> usize {
    let block_end = block_like_end(items, i);
    if let Some(end) = block_end {
        let continued = items
            .get(end)
            .is_some_and(|t| t.is_punct(".") || t.is_punct("?"));
        if !continued {
            return if items.get(end).is_some_and(|t| t.is_punct(";")) {
                end + 1
            } else {
                end
            };
        }
    }
    (i..items.len())
        .find(|&j| items[j].is_punct(";"))
        .map_or(items.len(), |j| j + 1)
}

/// Where a block-like expression starting at `i` ends, `None` if it is not one.
fn block_like_end(items: &[TokenTree<'_>], mut i: usize) -> Option<usize> {
    // A loop label: `'outer: loop {…}`.
    if items
        .get(i)?
        .leaf()
        .is_some_and(|t| t.kind == TokenKind::Lifetime)
        && items.get(i + 1)?.is_punct(":")
    {
        i += 2;
    }
    let head = items.get(i)?;
    if head.delimiter() == Some(b'{') {
        return Some(i + 1);
    }
    if head.is_ident("loop") || head.is_ident("unsafe") {
        return items
            .get(i + 1)
            .filter(|g| g.delimiter() == Some(b'{'))
            .map(|_| i + 2);
    }
    if head.is_ident("async") {
        let at = i + 1 + usize::from(items.get(i + 1).is_some_and(|t| t.is_ident("move")));
        return items
            .get(at)
            .filter(|g| g.delimiter() == Some(b'{'))
            .map(|_| at + 1);
    }
    if head.is_ident("while") || head.is_ident("for") || head.is_ident("match") {
        return first_brace(items, i + 1).map(|b| b + 1);
    }
    if head.is_ident("if") {
        let mut at = first_brace(items, i + 1)? + 1;
        while items.get(at).is_some_and(|t| t.is_ident("else")) {
            if items.get(at + 1).is_some_and(|t| t.is_ident("if")) {
                at = first_brace(items, at + 2)? + 1;
            } else {
                at = items
                    .get(at + 1)
                    .filter(|g| g.delimiter() == Some(b'{'))
                    .map(|_| at + 2)?;
            }
        }
        return Some(at);
    }
    // A braced macro invocation: `name! {…}`, `path::name! {…}`.
    let mut j = i;
    while items
        .get(j)
        .is_some_and(|t| t.leaf().is_some_and(|l| l.kind == TokenKind::Ident))
        && items.get(j + 1).is_some_and(|t| t.is_punct("::"))
    {
        j += 2;
    }
    if items
        .get(j)
        .is_some_and(|t| t.leaf().is_some_and(|l| l.kind == TokenKind::Ident))
        && items.get(j + 1).is_some_and(|t| t.is_punct("!"))
        && items
            .get(j + 2)
            .is_some_and(|g| g.delimiter() == Some(b'{'))
    {
        return Some(j + 3);
    }
    None
}

/// Whether a block's code never reaches its end: a `return`, `break` or
/// `continue` statement, or a diverging macro, at its top level.
fn diverges(body: &[TokenTree<'_>]) -> bool {
    statements(body).iter().any(|stmt| {
        let first = &body[stmt.start];
        first.is_ident("return")
            || first.is_ident("break")
            || first.is_ident("continue")
            || DIVERGING_MACROS.iter().any(|m| {
                first.is_ident(m) && body.get(stmt.start + 1).is_some_and(|t| t.is_punct("!"))
            })
    })
}

/// The names a pattern binds: identifiers that are not field names, paths,
/// constructors or binding modes.
fn pattern_names(pattern: &[TokenTree<'_>]) -> Vec<String> {
    let mut names = Vec::new();
    for (k, tree) in pattern.iter().enumerate() {
        match tree {
            TokenTree::Leaf(token) if token.kind == TokenKind::Ident => {
                let next = pattern.get(k + 1);
                let prev = k.checked_sub(1).map(|p| &pattern[p]);
                let constructor = next.is_some_and(|n| {
                    n.delimiter().is_some() || n.is_punct("::") || n.is_punct(":")
                });
                let path = prev.is_some_and(|p| p.is_punct("::"));
                let keyword = KEYWORDS.contains(&token.text) || token.text == "_";
                let starts_upper = token.text.starts_with(|c: char| c.is_ascii_uppercase());
                if !constructor && !path && !keyword && !starts_upper {
                    names.push(token.text.to_string());
                }
            }
            TokenTree::Group { inner, .. } => names.extend(pattern_names(inner)),
            TokenTree::Leaf(_) => {}
        }
    }
    names
}

/// A pattern without its leading `mut` / `ref` / `ref mut`.
fn strip_binding_modes<'a, 't>(pattern: &'a [TokenTree<'t>]) -> &'a [TokenTree<'t>] {
    let mut pattern = pattern;
    while pattern
        .first()
        .is_some_and(|t| t.is_ident("mut") || t.is_ident("ref"))
    {
        pattern = &pattern[1..];
    }
    pattern
}

/// Whether a string literal's text is a loopback or wildcard address with port 0.
fn is_zero_address_literal(literal: &str) -> bool {
    let content = literal
        .trim_start_matches(['b', 'c', 'r', '#'])
        .trim_end_matches('#');
    let content = content
        .strip_prefix('"')
        .and_then(|c| c.strip_suffix('"'))
        .unwrap_or("");
    [
        "127.0.0.1:0",
        "localhost:0",
        "0.0.0.0:0",
        "[::1]:0",
        "[::]:0",
    ]
    .contains(&content)
}

/// Whether the postfix chain after a call (`.unwrap()`, `?`, `.await`, …) reads
/// `.local_addr()` before it ends.
fn chain_reads_local_addr(rest: &[TokenTree<'_>]) -> bool {
    let mut i = 0;
    while i < rest.len() {
        if rest[i].is_punct("?") {
            i += 1;
        } else if rest[i].is_punct(".") {
            if rest.get(i + 1).is_some_and(|m| m.is_ident("local_addr")) {
                return true;
            }
            i += 2;
            if rest.get(i).is_some_and(|g| g.delimiter() == Some(b'(')) {
                i += 1;
            }
        } else {
            return false;
        }
    }
    false
}

/// Whether a `(…)` after `prev` is a call's arguments rather than a tuple or a
/// parenthesised expression.
fn is_call(prev: Option<&TokenTree<'_>>) -> bool {
    match prev {
        Some(TokenTree::Leaf(token)) => match token.kind {
            TokenKind::Ident => {
                !KEYWORDS.contains(&token.text) || token.text == "self" || token.text == "Self"
            }
            TokenKind::Punct => token.text == ">",
            _ => false,
        },
        Some(TokenTree::Group { open, .. }) => open.text == "(" || open.text == "[",
        None => false,
    }
}

/// Whether a `{…}` after `prev` is a struct literal rather than a block.
fn is_struct_literal(prev: Option<&TokenTree<'_>>) -> bool {
    match prev.and_then(TokenTree::leaf) {
        Some(token) if token.kind == TokenKind::Ident => {
            !KEYWORDS.contains(&token.text) || token.text == "Self"
        }
        Some(token) if token.kind == TokenKind::Punct => token.text == ">",
        _ => false,
    }
}

/// Whether a `|` after `prev` opens a closure rather than being a binary or.
fn starts_expression(prev: Option<&TokenTree<'_>>) -> bool {
    match prev {
        None => true,
        Some(TokenTree::Leaf(token)) => match token.kind {
            TokenKind::Punct => !matches!(token.text, ")" | "]" | "}" | "?"),
            TokenKind::Ident => {
                KEYWORDS.contains(&token.text)
                    && !matches!(token.text, "self" | "Self" | "true" | "false")
            }
            _ => false,
        },
        Some(TokenTree::Group { .. }) => false,
    }
}

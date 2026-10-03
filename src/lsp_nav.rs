//! Name resolution for the LSP's go-to-definition and signature help
//! (`src/lsp.rs`): over the open document, and from there into the modules it
//! imports and the classes it defines.
//!
//! The document is parsed with the runtime's own parser and every binding is
//! filed under the scope Python gives it: the module, a function (its
//! parameters and everything it assigns, minus `global` names), or a class body.
//! A name at the cursor resolves the way the compiler resolves it — the
//! innermost function outward, class bodies skipped from inside their methods,
//! then the module — to the binding in that scope nearest above the cursor, or
//! the first one when every binding comes later (a module function called from
//! a function defined above it).
//!
//! An attribute resolves the way it does at run time for the receivers whose
//! value the document itself determines: `self.x` / `cls.x` in a method — the
//! enclosing class, its instance attributes (`self.x = …` in any of its
//! methods) and then its bases, left to right — a class the document defines,
//! and a module it imports. An imported name resolves into the imported
//! module's file, found as `sys.path[0]` finds it — beside the document — or,
//! for a relative import, in the document's package; a name that module itself
//! imports is followed on.
//!
//! Positions are 0-based `(line, character)` with `character` counted in
//! `char`s, as the rest of the server counts them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::{Expr, Params, Pattern, PatternKind, Stmt, StmtKind};

/// What introduced a binding, which decides where on its line the name sits.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BindKind {
    Def,
    Class,
    Param,
    Other,
}

#[derive(Clone, Debug)]
struct Binding {
    name: String,
    /// 1-based line of the binding statement (a `def`'s own line for its
    /// parameters).
    line: u32,
    kind: BindKind,
    /// For a `def`/`class`, the definition itself (signature help reads it).
    stmt: Option<Stmt>,
    /// For a name an `import` binds, the module (and name) it comes from.
    origin: Option<ImportOrigin>,
}

/// What an `import` binds a name to.
#[derive(Clone, Debug)]
struct ImportOrigin {
    /// The dotted module path as written (`""` for `from . import x`).
    module: String,
    /// Leading dots of a relative import.
    level: usize,
    /// The name taken from the module (`from m import name`), or `None` when
    /// the binding is the module itself (`import m`).
    name: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ScopeKind {
    Module,
    Function,
    Class,
}

#[derive(Debug)]
struct Scope {
    kind: ScopeKind,
    /// 1-based inclusive line range the scope's code occupies.
    start: u32,
    end: u32,
    parent: Option<usize>,
    bindings: Vec<Binding>,
    globals: HashSet<String>,
    /// For a class body, the `class` statement it is the body of.
    class: Option<Stmt>,
}

/// Every scope of a parsed document.
struct Scopes(Vec<Scope>);

impl Scopes {
    fn build(stmts: &[Stmt]) -> Scopes {
        let mut scopes = Scopes(vec![Scope {
            kind: ScopeKind::Module,
            start: 1,
            end: u32::MAX,
            parent: None,
            bindings: Vec::new(),
            globals: HashSet::new(),
            class: None,
        }]);
        scopes.block(stmts, 0, u32::MAX);
        scopes
    }

    fn bind(&mut self, scope: usize, name: &str, line: u32, kind: BindKind, stmt: Option<&Stmt>) {
        self.bind_from(scope, name, line, kind, stmt, None);
    }

    fn bind_from(
        &mut self,
        scope: usize,
        name: &str,
        line: u32,
        kind: BindKind,
        stmt: Option<&Stmt>,
        origin: Option<ImportOrigin>,
    ) {
        // A `global` name assigned in a function is a module binding.
        let scope = if self.0[scope].globals.contains(name) {
            0
        } else {
            scope
        };
        self.0[scope].bindings.push(Binding {
            name: name.to_string(),
            line,
            kind,
            stmt: stmt.cloned(),
            origin,
        });
    }

    /// File the bindings of a block. `end` is the last line the block can
    /// reach, so a statement's own reach runs to the line before its next
    /// sibling (continuation lines included).
    fn block(&mut self, stmts: &[Stmt], scope: usize, end: u32) {
        for (i, s) in stmts.iter().enumerate() {
            let reach = stmts.get(i + 1).map_or(end, |n| n.line.saturating_sub(1));
            self.stmt(s, scope, reach);
        }
    }

    fn stmt(&mut self, s: &Stmt, scope: usize, reach: u32) {
        match &s.kind {
            StmtKind::FuncDef {
                name, params, body, ..
            } => {
                self.bind(scope, name, s.line, BindKind::Def, Some(s));
                let inner = self.open(ScopeKind::Function, s.line, reach, scope);
                for p in param_names(params) {
                    self.bind(inner, &p, s.line, BindKind::Param, None);
                }
                self.block(body, inner, reach);
            }
            StmtKind::ClassDef { name, body, .. } => {
                self.bind(scope, name, s.line, BindKind::Class, Some(s));
                let inner = self.open(ScopeKind::Class, s.line, reach, scope);
                self.0[inner].class = Some(s.clone());
                self.block(body, inner, reach);
            }
            StmtKind::Assign { targets, .. } => {
                for t in targets {
                    self.targets(t, scope, s.line);
                }
            }
            StmtKind::AugAssign { target, .. } | StmtKind::AnnAssign { target, .. } => {
                self.targets(target, scope, s.line)
            }
            StmtKind::For {
                target,
                body,
                orelse,
                ..
            } => {
                self.targets(target, scope, s.line);
                self.block(body, scope, reach);
                self.block(orelse, scope, reach);
            }
            StmtKind::While { body, orelse, .. } | StmtKind::If { body, orelse, .. } => {
                self.block(body, scope, reach);
                self.block(orelse, scope, reach);
            }
            StmtKind::With { items, body, .. } => {
                for item in items {
                    if let Some(v) = &item.vars {
                        self.targets(v, scope, s.line);
                    }
                }
                self.block(body, scope, reach);
            }
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                self.block(body, scope, reach);
                for h in handlers {
                    if let (Some(name), Some(first)) = (&h.name, h.body.first()) {
                        // The `except … as name` clause sits on the line above
                        // its body's first statement.
                        self.bind(
                            scope,
                            name,
                            first.line.saturating_sub(1),
                            BindKind::Other,
                            None,
                        );
                    }
                    self.block(&h.body, scope, reach);
                }
                self.block(orelse, scope, reach);
                self.block(finalbody, scope, reach);
            }
            StmtKind::Import(aliases) => {
                for a in aliases {
                    // `import a.b` binds the top package `a`; `import a.b as
                    // m` binds the submodule.
                    let (bound, module) = match &a.asname {
                        Some(n) => (n.as_str(), a.name.as_str()),
                        None => {
                            let top = a.name.split('.').next().unwrap_or(&a.name);
                            (top, top)
                        }
                    };
                    let origin = ImportOrigin {
                        module: module.to_string(),
                        level: 0,
                        name: None,
                    };
                    self.bind_from(scope, bound, s.line, BindKind::Other, None, Some(origin));
                }
            }
            StmtKind::ImportFrom {
                module,
                names,
                level,
            } => {
                for a in names {
                    let bound = a.asname.as_deref().unwrap_or(&a.name);
                    if bound != "*" {
                        let origin = ImportOrigin {
                            module: module.clone().unwrap_or_default(),
                            level: *level,
                            name: Some(a.name.clone()),
                        };
                        self.bind_from(scope, bound, s.line, BindKind::Other, None, Some(origin));
                    }
                }
            }
            StmtKind::Global(names) => {
                self.0[scope].globals.extend(names.iter().cloned());
            }
            StmtKind::Match { cases, .. } => {
                for c in cases {
                    let mut captured = Vec::new();
                    pattern_names(&c.pattern, &mut captured);
                    let line = c.body.first().map_or(s.line, |b| b.line.saturating_sub(1));
                    for n in captured {
                        self.bind(scope, &n, line, BindKind::Other, None);
                    }
                    self.block(&c.body, scope, reach);
                }
            }
            _ => {}
        }
    }

    fn open(&mut self, kind: ScopeKind, start: u32, end: u32, parent: usize) -> usize {
        self.0.push(Scope {
            kind,
            start,
            end,
            parent: Some(parent),
            bindings: Vec::new(),
            globals: HashSet::new(),
            class: None,
        });
        self.0.len() - 1
    }

    /// Every name an assignment target binds.
    fn targets(&mut self, e: &Expr, scope: usize, line: u32) {
        match e.unspanned() {
            Expr::Name(n) => self.bind(scope, n, line, BindKind::Other, None),
            Expr::Tuple(items) | Expr::List(items) => {
                for i in items {
                    self.targets(i, scope, line);
                }
            }
            Expr::Starred(inner) => self.targets(inner, scope, line),
            _ => {}
        }
    }

    /// The innermost scope whose lines contain `line` (1-based).
    fn innermost(&self, line: u32) -> usize {
        let mut best = 0;
        for (i, s) in self.0.iter().enumerate().skip(1) {
            // A scope's own header line belongs to the enclosing scope.
            if line > s.start && line <= s.end && self.depth(i) > self.depth(best) {
                best = i;
            }
        }
        best
    }

    fn depth(&self, mut i: usize) -> usize {
        let mut d = 0;
        while let Some(p) = self.0[i].parent {
            d += 1;
            i = p;
        }
        d
    }

    /// The binding `name` refers to from `line` (1-based).
    fn resolve(&self, name: &str, line: u32) -> Option<&Binding> {
        let start = self.innermost(line);
        let mut scope = Some(start);
        while let Some(i) = scope {
            let s = &self.0[i];
            // A class body is visible only to its own statements, not to the
            // methods nested in it.
            let visible = s.kind != ScopeKind::Class || i == start;
            if s.kind == ScopeKind::Function && s.globals.contains(name) {
                scope = Some(0);
                continue;
            }
            if visible {
                let mut found = s.bindings.iter().filter(|b| b.name == name);
                if let Some(first) = found.next() {
                    let before = std::iter::once(first)
                        .chain(found)
                        .rfind(|b| b.line <= line);
                    return Some(before.unwrap_or(first));
                }
            }
            scope = s.parent;
        }
        None
    }
}

/// A function's parameter names in signature order.
fn param_names(p: &Params) -> Vec<String> {
    let mut out = p.names.clone();
    if let Some(s) = p.star.as_ref().filter(|s| !s.is_empty()) {
        out.push(s.clone());
    }
    out.extend(p.kwonly.iter().cloned());
    if let Some(k) = &p.kwargs {
        out.push(k.clone());
    }
    out
}

/// The names a `case` pattern captures.
fn pattern_names(p: &Pattern, out: &mut Vec<String>) {
    match &p.kind {
        PatternKind::Capture(n) => out.push(n.clone()),
        PatternKind::As(inner, n) => {
            pattern_names(inner, out);
            out.push(n.clone());
        }
        PatternKind::Or(alts) => alts.iter().for_each(|a| pattern_names(a, out)),
        PatternKind::Sequence { elems, .. } => elems.iter().for_each(|e| pattern_names(e, out)),
        PatternKind::Star(Some(n)) => out.push(n.clone()),
        PatternKind::Mapping { keys, rest } => {
            keys.iter().for_each(|(_, v)| pattern_names(v, out));
            out.extend(rest.iter().cloned());
        }
        PatternKind::Class { pos, kw, .. } => {
            pos.iter().for_each(|q| pattern_names(q, out));
            kw.iter().for_each(|(_, q)| pattern_names(q, out));
        }
        PatternKind::Wildcard | PatternKind::Value(_) | PatternKind::Star(None) => {}
    }
}

/// Parse `text`, tolerating the line being typed: an editor asks for help
/// while the current statement is incomplete, so a document that does not
/// parse is retried with the cursor's line replaced by a `pass` at the same
/// indentation (which keeps a block that line was the body of well-formed),
/// then cut off there.
fn parse_tolerant(text: &str, line0: usize) -> Option<Vec<Stmt>> {
    if let Ok(s) = crate::parser::parse(text) {
        return Some(s);
    }
    let lines: Vec<&str> = text.lines().collect();
    let patched: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == line0 {
                let indent: String = l.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}pass")
            } else {
                l.to_string()
            }
        })
        .collect();
    if let Ok(s) = crate::parser::parse(&patched.join("\n")) {
        return Some(s);
    }
    crate::parser::parse(&lines[..line0.min(lines.len())].join("\n")).ok()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The char column of `name` as a whole word in `line`, at or after `from`.
fn find_word(line: &str, name: &str, from: usize) -> Option<usize> {
    let chars: Vec<char> = line.chars().collect();
    let want: Vec<char> = name.chars().collect();
    (from..chars.len().saturating_sub(want.len() - 1)).find(|&i| {
        chars[i..].starts_with(&want)
            && (i == 0 || !is_ident(chars[i - 1]))
            && chars.get(i + want.len()).map_or(true, |c| !is_ident(*c))
    })
}

/// Where a binding's name sits: `(line0, char0)`.
fn binding_position(lines: &[&str], b: &Binding) -> (u32, u32) {
    let first = (b.line as usize).saturating_sub(1);
    // A `def`/`class` line follows its decorators, and a parameter may sit on a
    // later line of a wrapped header; look a few lines on.
    for (i, text) in lines.iter().enumerate().skip(first).take(64) {
        let from = match b.kind {
            BindKind::Def => match find_word(text, "def", 0) {
                Some(k) => k + 3,
                None => continue,
            },
            BindKind::Class => match find_word(text, "class", 0) {
                Some(k) => k + 5,
                None => continue,
            },
            // A parameter is inside the header's parentheses; skipping to the
            // `(` keeps a parameter named like its function off the name.
            BindKind::Param if i == first => {
                text.find('(').map_or(0, |b| text[..b].chars().count())
            }
            BindKind::Param | BindKind::Other => 0,
        };
        if let Some(col) = find_word(text, &b.name, from) {
            return (i as u32, col as u32);
        }
        if b.kind == BindKind::Other {
            break;
        }
    }
    (first as u32, 0)
}

/// The identifier spanning `(line0, char0)`.
fn word_at(lines: &[&str], line0: usize, char0: usize) -> Option<String> {
    let chars: Vec<char> = lines.get(line0)?.chars().collect();
    let col = char0.min(chars.len());
    let mut start = col;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_ident(chars[end]) {
        end += 1;
    }
    (start < end).then(|| chars[start..end].iter().collect())
}

/// A source file and its scopes: the open document, or a module it imports.
struct Source {
    /// `None` for the open document.
    path: Option<PathBuf>,
    /// The directory its imports are looked up from.
    dir: Option<PathBuf>,
    text: String,
    scopes: Scopes,
}

/// What a name, or a step along an attribute chain, has reached. Files are
/// indices into [`Sources`].
#[derive(Clone)]
enum Reached {
    /// A module: the file itself.
    Module(usize),
    /// An instance of the class whose body is scope `.1` of file `.0` — what
    /// a method's `self` is.
    Instance(usize, usize),
    /// A binding in file `.0`.
    Binding(usize, Box<Binding>),
}

/// How many imports and base classes a lookup follows before giving up: a
/// cycle (`a` importing `b` importing `a`, a class deriving from itself) ends
/// here instead of looping.
const MAX_HOPS: usize = 16;

/// Every file a lookup has read; the open document is index 0. Each file is
/// read and parsed once.
struct Sources(Vec<Source>);

impl Sources {
    fn new(text: &str, stmts: &[Stmt], doc: Option<&Path>) -> Sources {
        Sources(vec![Source {
            path: None,
            dir: doc.and_then(Path::parent).map(Path::to_path_buf),
            text: text.to_string(),
            scopes: Scopes::build(stmts),
        }])
    }

    /// The file at `path`, read and parsed on first use.
    fn load(&mut self, path: PathBuf) -> Option<usize> {
        if let Some(i) = self.0.iter().position(|s| s.path.as_ref() == Some(&path)) {
            return Some(i);
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let stmts = crate::parser::parse(&text).ok()?;
        self.0.push(Source {
            dir: path.parent().map(Path::to_path_buf),
            path: Some(path),
            scopes: Scopes::build(&stmts),
            text,
        });
        Some(self.0.len() - 1)
    }

    /// The module `import module` (with `level` leading dots) reads from file
    /// `from`.
    fn module(&mut self, from: usize, module: &str, level: usize) -> Option<usize> {
        let path = module_file(self.0[from].dir.as_deref()?, module, level)?;
        self.load(path)
    }

    /// Submodule `name` of the package file `m` is the `__init__.py` of.
    fn submodule(&mut self, m: usize, name: &str) -> Option<usize> {
        let init = self.0[m].path.clone()?;
        if init.file_name()? != "__init__.py" {
            return None;
        }
        let path = module_file(init.parent()?, name, 0)?;
        self.load(path)
    }

    /// The module scope's last binding of `name` in file `src` — the binding
    /// an importer sees once the module has run.
    fn top_level(&self, src: usize, name: &str) -> Option<Binding> {
        let module = &self.0[src].scopes.0[0];
        module
            .bindings
            .iter()
            .rev()
            .find(|b| b.name == name)
            .cloned()
    }

    /// What an import binding of file `from` names: the binding in the
    /// imported module, or the module itself.
    fn import(&mut self, from: usize, origin: &ImportOrigin) -> Option<Reached> {
        let m = self.module(from, &origin.module, origin.level);
        let Some(name) = &origin.name else {
            return m.map(Reached::Module);
        };
        match m.and_then(|m| Some((m, self.top_level(m, name)?))) {
            Some((m, b)) => Some(Reached::Binding(m, Box::new(b))),
            // `from package import submodule`.
            None => self.submodule(m?, name).map(Reached::Module),
        }
    }

    /// Follow `at` through imports to what is finally bound: a package's
    /// `__init__.py` re-export leads to the definition.
    fn settle(&mut self, at: Reached) -> Option<Reached> {
        let mut at = at;
        for _ in 0..MAX_HOPS {
            match &at {
                Reached::Binding(src, b) => match &b.origin {
                    Some(origin) => at = self.import(*src, &origin.clone())?,
                    None => return Some(at),
                },
                _ => return Some(at),
            }
        }
        None
    }

    /// `attr` on what `at` reached.
    fn attribute(&mut self, at: Reached, attr: &str, hops: usize) -> Option<Reached> {
        if hops > MAX_HOPS {
            return None;
        }
        match self.settle(at)? {
            Reached::Module(m) => match self.top_level(m, attr) {
                Some(b) => Some(Reached::Binding(m, Box::new(b))),
                None => self.submodule(m, attr).map(Reached::Module),
            },
            Reached::Instance(src, class) => self.class_member(src, class, attr, hops),
            Reached::Binding(src, b) => {
                let class = class_scope(&self.0[src].scopes, &b)?;
                self.class_member(src, class, attr, hops)
            }
        }
    }

    /// `attr` looked up on the class whose body is scope `class` of file
    /// `src`: the class body's own binding, else an instance attribute one of
    /// its methods assigns through its first parameter, else its bases, left
    /// to right.
    fn class_member(
        &mut self,
        src: usize,
        class: usize,
        attr: &str,
        hops: usize,
    ) -> Option<Reached> {
        let scope = &self.0[src].scopes.0[class];
        if let Some(b) = scope.bindings.iter().rev().find(|b| b.name == attr) {
            return Some(Reached::Binding(src, Box::new(b.clone())));
        }
        let StmtKind::ClassDef { body, bases, .. } = &scope.class.as_ref()?.kind else {
            return None;
        };
        if let Some(line) = instance_attribute(body, attr) {
            let b = Binding {
                name: attr.to_string(),
                line,
                kind: BindKind::Other,
                stmt: None,
                origin: None,
            };
            return Some(Reached::Binding(src, Box::new(b)));
        }
        let class_line = scope.start;
        let bases: Vec<String> = bases
            .iter()
            .filter_map(|e| match e.unspanned() {
                Expr::Name(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        bases.iter().find_map(|base| {
            let b = self.0[src].scopes.resolve(base, class_line)?.clone();
            self.attribute(Reached::Binding(src, Box::new(b)), attr, hops + 1)
        })
    }

    /// What the dotted `chain` (`helper`, `self.x`, `Box.area`,
    /// `os.path.join`) names from `line` (1-based) of the open document.
    fn resolve(&mut self, chain: &[String], line: u32) -> Option<Reached> {
        let (first, rest) = chain.split_first()?;
        let scopes = &self.0[0].scopes;
        let mut at = match enclosing_class(scopes, first, line) {
            Some(class) if !rest.is_empty() => Reached::Instance(0, class),
            _ => Reached::Binding(0, Box::new(scopes.resolve(first, line)?.clone())),
        };
        for attr in rest {
            at = self.attribute(at, attr, 0)?;
        }
        Some(at)
    }
}

/// The file `import module` (with `level` leading dots) reads, looked for as
/// `sys.path[0]` finds it — in `dir`, the importing file's directory — or, for
/// a relative import, in the package `level - 1` directories above: `m.py`,
/// else `m/__init__.py`.
fn module_file(dir: &Path, module: &str, level: usize) -> Option<PathBuf> {
    let mut stem = dir.to_path_buf();
    for _ in 1..level {
        stem = stem.parent()?.to_path_buf();
    }
    let parts: Vec<&str> = module.split('.').filter(|p| !p.is_empty()).collect();
    for p in &parts {
        stem.push(p);
    }
    let candidates = if parts.is_empty() {
        vec![stem.join("__init__.py")]
    } else {
        vec![stem.with_extension("py"), stem.join("__init__.py")]
    };
    candidates.into_iter().find(|f| f.is_file())
}

/// The first line (1-based) of `body`'s methods that assigns `attr` through
/// the method's first parameter — `self.attr = …`, `self.attr += …`,
/// `self.attr: T = …` — which is where an instance attribute is defined.
fn instance_attribute(body: &[Stmt], attr: &str) -> Option<u32> {
    fn assigns(stmts: &[Stmt], me: &str, attr: &str) -> Option<u32> {
        let hits = |t: &Expr| {
            matches!(t.unspanned(), Expr::Attribute(recv, a)
                if a == attr && matches!(recv.unspanned(), Expr::Name(n) if n == me))
        };
        stmts.iter().find_map(|s| match &s.kind {
            StmtKind::Assign { targets, .. } if targets.iter().any(hits) => Some(s.line),
            StmtKind::AugAssign { target, .. } | StmtKind::AnnAssign { target, .. }
                if hits(target) =>
            {
                Some(s.line)
            }
            StmtKind::If { body, orelse, .. }
            | StmtKind::While { body, orelse, .. }
            | StmtKind::For { body, orelse, .. } => {
                assigns(body, me, attr).or_else(|| assigns(orelse, me, attr))
            }
            StmtKind::With { body, .. } => assigns(body, me, attr),
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => assigns(body, me, attr)
                .or_else(|| handlers.iter().find_map(|h| assigns(&h.body, me, attr)))
                .or_else(|| assigns(orelse, me, attr))
                .or_else(|| assigns(finalbody, me, attr)),
            _ => None,
        })
    }
    body.iter().find_map(|s| match &s.kind {
        StmtKind::FuncDef { params, body, .. } => {
            let me = params.names.first()?;
            assigns(body, me, attr)
        }
        _ => None,
    })
}

/// The dotted names before the `.` at char index `dot` of `chars` — `self` in
/// `self.x`, `os.path` in `os.path.join` — or `None` when the receiver is not
/// a plain dotted name (a call, a subscript, a literal).
fn receiver_chain(chars: &[char], dot: usize) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut end = dot;
    loop {
        let mut start = end;
        while start > 0 && is_ident(chars[start - 1]) {
            start -= 1;
        }
        if start == end || chars[start].is_ascii_digit() {
            return None;
        }
        names.push(chars[start..end].iter().collect::<String>());
        if start > 0 && chars[start - 1] == '.' {
            end = start - 1;
        } else {
            names.reverse();
            return Some(names);
        }
    }
}

/// The class scope a method's first parameter (`self`, `cls`) stands for,
/// when `name` is that parameter of the function enclosing `line`.
fn enclosing_class(scopes: &Scopes, name: &str, line: u32) -> Option<usize> {
    let func = &scopes.0[scopes.innermost(line)];
    if func.kind != ScopeKind::Function {
        return None;
    }
    let first = func.bindings.iter().find(|b| b.kind == BindKind::Param)?;
    let class = func.parent?;
    (first.name == name && scopes.0[class].kind == ScopeKind::Class).then_some(class)
}

/// The scope of the class body a `class` binding defines.
fn class_scope(scopes: &Scopes, b: &Binding) -> Option<usize> {
    if b.kind != BindKind::Class {
        return None;
    }
    scopes
        .0
        .iter()
        .position(|s| s.kind == ScopeKind::Class && s.start == b.line)
}

/// The dotted name ending at the identifier spanning `(line0, char0)`:
/// `["self", "width"]` for the cursor on `width` in `self.width`.
fn chain_at(lines: &[&str], line0: usize, char0: usize) -> Option<Vec<String>> {
    let name = word_at(lines, line0, char0)?;
    let chars: Vec<char> = lines.get(line0)?.chars().collect();
    let start = (0..=char0.min(chars.len()))
        .rev()
        .find(|&i| i == 0 || !is_ident(chars[i - 1]))
        .unwrap_or(0);
    let mut chain = if start > 0 && chars[start - 1] == '.' {
        receiver_chain(&chars, start - 1)?
    } else {
        Vec::new()
    };
    chain.push(name);
    Some(chain)
}

/// Where a definition is: `file` is `None` for the open document; `line` and
/// `character` are 0-based and `len` is the name's length in chars (0 for a
/// module, which is the start of its file).
#[derive(Debug, PartialEq)]
pub struct Target {
    pub file: Option<PathBuf>,
    pub line: u32,
    pub character: u32,
    pub len: u32,
}

/// Go-to-definition: where the name under the cursor is bound, in the open
/// document or in a module it imports. `doc` is the document's own path, which
/// the imports are found relative to. `None` for a builtin, an attribute of a
/// value the document does not determine, or a name bound nowhere it can see.
pub fn definition_at(text: &str, doc: Option<&Path>, line0: u32, char0: u32) -> Option<Target> {
    let lines: Vec<&str> = text.lines().collect();
    let chain = chain_at(&lines, line0 as usize, char0 as usize)?;
    let stmts = parse_tolerant(text, line0 as usize)?;
    let mut sources = Sources::new(text, &stmts, doc);
    let at = sources.resolve(&chain, line0 + 1)?;
    // A name imported from another module goes on to its definition there;
    // when that module cannot be read, the import itself is the answer.
    let at = sources.settle(at.clone()).unwrap_or(at);
    match at {
        Reached::Module(m) => Some(Target {
            file: sources.0[m].path.clone(),
            line: 0,
            character: 0,
            len: 0,
        }),
        Reached::Binding(src, b) => {
            let source = &sources.0[src];
            let lines: Vec<&str> = source.text.lines().collect();
            let (line, character) = binding_position(&lines, &b);
            Some(Target {
                file: source.path.clone(),
                line,
                character,
                len: b.name.chars().count() as u32,
            })
        }
        Reached::Instance(..) => None,
    }
}

/// [`definition_at`] within the open document only, as `(line0, char0,
/// length)`.
pub fn definition(text: &str, line0: u32, char0: u32) -> Option<(u32, u32, u32)> {
    let t = definition_at(text, None, line0, char0)?;
    t.file.is_none().then_some((t.line, t.character, t.len))
}

/// A signature to show: the label, each parameter's text, the active
/// parameter, and the docstring.
#[derive(Debug, PartialEq)]
pub struct Signature {
    pub label: String,
    pub params: Vec<String>,
    pub active: u32,
    pub doc: Option<String>,
}

/// The call the cursor is inside: the char offset just past its `(`, and the
/// arguments typed so far split at top-level commas. Strings and comments are
/// skipped, so a `(` or `,` in a literal does not count.
fn open_call(text: &str, line0: usize, char0: usize) -> Option<(usize, Vec<String>)> {
    let mut offset = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if i == line0 {
            offset += char0.min(l.chars().count());
            break;
        }
        offset += l.chars().count() + 1;
    }
    let chars: Vec<char> = text.chars().take(offset).collect();
    // Each open bracket: its kind, where its contents start, and the argument
    // pieces seen so far.
    let mut stack: Vec<(char, usize, Vec<String>, String)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\'' | '"' => {
                let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
                let quote = if triple { 3 } else { 1 };
                let mut j = i + quote;
                while j < chars.len() {
                    if chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == c && (!triple || chars[j..].starts_with(&[c, c, c])) {
                        break;
                    }
                    if !triple && chars[j] == '\n' {
                        break;
                    }
                    j += 1;
                }
                if let Some(top) = stack.last_mut() {
                    top.3.extend(&chars[i..(j + quote).min(chars.len())]);
                }
                i = j + quote;
                continue;
            }
            '(' | '[' | '{' => stack.push((c, i + 1, Vec::new(), String::new())),
            ')' | ']' | '}' => {
                let closed = stack.pop();
                // The enclosing argument records only that a bracket was
                // there, which is all the keyword check needs.
                if let (Some(top), Some(inner)) = (stack.last_mut(), closed) {
                    top.3.push(inner.0);
                    top.3.push(c);
                }
            }
            ',' => {
                if let Some(top) = stack.last_mut() {
                    let arg = std::mem::take(&mut top.3);
                    top.2.push(arg);
                }
            }
            _ => {
                if let Some(top) = stack.last_mut() {
                    top.3.push(c);
                }
            }
        }
        i += 1;
    }
    let (kind, open, mut args, current) = stack.pop()?;
    if kind != '(' {
        return None;
    }
    args.push(current);
    Some((open, args))
}

/// Split a parameter list's source at its top-level commas.
fn split_params(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in src.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur.trim().to_string());
    out.retain(|p| !p.is_empty());
    out
}

/// The source text between a `def`'s parentheses, read from the document so
/// defaults and annotations show exactly as written.
fn header_params(lines: &[&str], def_line: u32, name: &str) -> Option<Vec<String>> {
    let first = (def_line as usize).saturating_sub(1);
    let rest: String = lines.get(first..)?.join("\n");
    let at = rest.find(&format!("def {name}"))?;
    let after = &rest[at..];
    let open = after.find('(')?;
    let mut depth = 0i32;
    for (i, c) in after[open..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(split_params(&after[open + 1..open + i]));
                }
            }
            _ => {}
        }
    }
    None
}

fn docstring(body: &[Stmt]) -> Option<String> {
    match body.first().map(|s| &s.kind) {
        Some(StmtKind::Expr(e)) => match e.unspanned() {
            Expr::Str(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Signature help for the call the cursor is inside, when its callee is a
/// function or class the document can see: defined in it, imported from a
/// module beside it, or a method reached through `self`, a class or a module
/// (a class shows `__init__` without `self`, and so does a method called on
/// `self`).
pub fn signature_help(text: &str, line0: u32, char0: u32) -> Option<Signature> {
    signature_help_at(text, None, line0, char0)
}

/// [`signature_help`], with the document's path to find its imports from.
pub fn signature_help_at(
    text: &str,
    doc: Option<&Path>,
    line0: u32,
    char0: u32,
) -> Option<Signature> {
    let (open, args) = open_call(text, line0 as usize, char0 as usize)?;
    // The callee is the dotted name right before the `(`.
    let chars: Vec<char> = text.chars().collect();
    let mut end = open - 1;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let chain = receiver_chain(&chars, end)?;
    let callee = chain.last()?.clone();
    let call_line = chars[..end].iter().filter(|c| **c == '\n').count();

    let stmts = parse_tolerant(text, line0 as usize)?;
    let mut sources = Sources::new(text, &stmts, doc);
    let reached = sources.resolve(&chain, call_line as u32 + 1)?;
    let Reached::Binding(src, b) = sources.settle(reached)? else {
        return None;
    };
    // A method called through `self`/`cls` is bound: its first parameter is
    // not passed.
    let bound = chain.len() > 1
        && enclosing_class(&sources.0[0].scopes, &chain[0], call_line as u32 + 1).is_some();
    let lines: Vec<&str> = sources.0[src].text.lines().collect();
    let (def_name, def_line, body, drop_self) = match &b.stmt.as_ref()?.kind {
        StmtKind::FuncDef { name, body, .. } => (name.clone(), b.line, body.clone(), bound),
        StmtKind::ClassDef { body, .. } => {
            let init = body
                .iter()
                .find(|s| matches!(&s.kind, StmtKind::FuncDef { name, .. } if name == "__init__"));
            match init {
                Some(s) => match &s.kind {
                    StmtKind::FuncDef { body: ib, .. } => {
                        ("__init__".to_string(), s.line, ib.clone(), true)
                    }
                    _ => unreachable!(),
                },
                None => {
                    return Some(Signature {
                        label: format!("{callee}()"),
                        params: Vec::new(),
                        active: 0,
                        doc: docstring(body),
                    })
                }
            }
        }
        _ => return None,
    };
    let mut params = header_params(&lines, def_line, &def_name)?;
    if drop_self && !params.is_empty() {
        params.remove(0);
    }
    let label = format!("{callee}({})", params.join(", "));
    // The `/` and bare `*` markers are part of the label, not parameters.
    let params: Vec<String> = params
        .into_iter()
        .filter(|p| p != "/" && p != "*")
        .collect();
    let current = args
        .last()
        .map(|a| a.trim().to_string())
        .unwrap_or_default();
    let keyword = current
        .split_once('=')
        .map(|(k, _)| k.trim())
        .filter(|k| !k.is_empty() && k.chars().all(is_ident));
    let param_name = |p: &str| -> String {
        p.trim_start_matches('*')
            .split([':', '='])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let active = match keyword {
        Some(k) => params.iter().position(|p| param_name(p) == k),
        None => {
            let index = args.len() - 1;
            // Past the last positional slot, a `*args` collector takes the rest.
            params
                .iter()
                .position(|p| p.starts_with('*') && !p.starts_with("**"))
                .filter(|&star| index >= star)
                .or(Some(index))
        }
    };
    Some(Signature {
        label,
        params,
        active: active.unwrap_or(0) as u32,
        doc: docstring(&body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
import os.path as osp
LIMIT = 10

def helper(a, b=2, *rest, key=None, **kw):
    \"\"\"Add things.\"\"\"
    total = a + b
    return total + LIMIT

class Box:
    size = 3
    def __init__(self, width, height=1):
        self.width = width

    def area(self):
        return size_of(self)

def main():
    global LIMIT
    LIMIT = 5
    value = helper(1, 2)
    for i, item in enumerate([value]):
        print(i, item, osp)
    return Box(4)

def size_of(box):
    return box.width
";

    fn def_of(line0: u32, word: &str) -> Option<(u32, u32, u32)> {
        let text = SRC.lines().nth(line0 as usize).unwrap();
        let col = find_word(text, word, 0).expect("word on line") as u32;
        definition(SRC, line0, col)
    }

    #[test]
    fn definition_resolves_through_python_scopes() {
        // A local, from inside its function.
        assert_eq!(def_of(6, "total"), Some((5, 4, 5)));
        // A parameter.
        assert_eq!(def_of(5, "a"), Some((3, 11, 1)));
        // A module global read in a function.
        assert_eq!(def_of(6, "LIMIT"), Some((1, 0, 5)));
        // `global LIMIT` makes the assignment in `main` a module binding, and
        // the nearest binding above the cursor wins.
        assert_eq!(def_of(18, "LIMIT"), Some((18, 4, 5)));
        assert_eq!(def_of(19, "helper"), Some((3, 4, 6)));
        // A function defined BELOW its caller still resolves.
        assert_eq!(def_of(14, "size_of"), Some((24, 4, 7)));
        // A class-body name is not visible from a method, and `size` is bound
        // nowhere else.
        assert_eq!(def_of(14, "self"), Some((13, 13, 4)));
        // `for` targets and an import alias.
        assert_eq!(def_of(21, "item"), Some((20, 11, 4)));
        assert_eq!(def_of(21, "osp"), Some((0, 18, 3)));
        // A class name.
        assert_eq!(def_of(22, "Box"), Some((8, 6, 3)));
        // An attribute and a builtin have no definition here.
        assert_eq!(def_of(25, "width"), None);
        assert_eq!(def_of(21, "print"), None);
    }

    #[test]
    fn definition_survives_the_line_being_typed() {
        let text = format!("{SRC}x = helper(");
        assert_eq!(definition(&text, 26, 5), Some((3, 4, 6)));
    }

    #[test]
    fn signature_help_reads_the_header_as_written() {
        let text = format!("{SRC}helper(1, ");
        let sig = signature_help(&text, 26, 10).expect("signature");
        assert_eq!(sig.label, "helper(a, b=2, *rest, key=None, **kw)");
        assert_eq!(sig.params, ["a", "b=2", "*rest", "key=None", "**kw"]);
        assert_eq!(sig.active, 1);
        assert_eq!(sig.doc.as_deref(), Some("Add things."));
        // Past the positionals the `*rest` collector is active; a keyword
        // selects its own parameter; strings do not count commas.
        let text = format!("{SRC}helper(1, 2, 3, ',(', ");
        assert_eq!(signature_help(&text, 26, 23).expect("sig").active, 2);
        let text = format!("{SRC}helper(1, key=");
        assert_eq!(signature_help(&text, 26, 14).expect("sig").active, 3);
        // A class shows `__init__` without `self`.
        let text = format!("{SRC}Box(");
        let sig = signature_help(&text, 26, 4).expect("signature");
        assert_eq!(sig.label, "Box(width, height=1)");
        assert_eq!(sig.active, 0);
        // A nested call shows the innermost callee.
        let text = format!("{SRC}helper(size_of(");
        assert_eq!(
            signature_help(&text, 26, 15).expect("sig").label,
            "size_of(box)"
        );
        // Outside any call there is nothing to show.
        assert_eq!(signature_help(SRC, 1, 3), None);
    }
    /// The receivers whose value the document determines: a method's `self`
    /// (instance attributes assigned in any method, the class body, then the
    /// bases left to right) and a class named directly.
    #[test]
    fn attributes_resolve_through_self_and_classes() {
        let src = "\
class Base:
    def ping(self):
        return 1

class Box(Base):
    size = 3
    def __init__(self, width):
        if width:
            self.width = width
    def area(self):
        return self.width * self.size + self.ping()

def make():
    return Box.size, Box(1).area
";
        let at = |line0: u32, word: &str| {
            let text = src.lines().nth(line0 as usize).unwrap();
            let col = find_word(text, word, 0).expect("word on line") as u32;
            definition(src, line0, col)
        };
        // An instance attribute, from the method that assigns it.
        assert_eq!(at(10, "width"), Some((8, 17, 5)));
        // A class attribute and an inherited method, through `self`.
        assert_eq!(at(10, "size"), Some((5, 4, 4)));
        assert_eq!(at(10, "ping"), Some((1, 8, 4)));
        // A class attribute through the class's name.
        assert_eq!(at(13, "size"), Some((5, 4, 4)));
        // A call's result is not a value the document determines.
        assert_eq!(at(13, "area"), None);
        // A method called on `self` is bound: no `self` in the signature.
        let class: Vec<&str> = src.lines().take(11).collect();
        let text = format!(
            "{}\n    def grow(self):\n        self.area(",
            class.join("\n")
        );
        let sig = signature_help(&text, 12, 18).expect("signature");
        assert_eq!(sig.label, "area()");
    }

    /// An imported name resolves into the module beside the document, through
    /// a package's `__init__.py` re-export, and a module's attributes resolve
    /// into its file.
    #[test]
    fn imports_resolve_into_the_imported_module() {
        let dir = std::env::temp_dir().join(format!("pythonrs_lsp_nav_{}", std::process::id()));
        let pkg = dir.join("pkg");
        std::fs::create_dir_all(&pkg).expect("temp package");
        std::fs::write(dir.join("helpers.py"), "X = 1\n\ndef scale(value, factor=10):\n    \"\"\"Scale.\"\"\"\n    return value * factor\n").unwrap();
        std::fs::write(pkg.join("__init__.py"), "from .core import Engine\n").unwrap();
        std::fs::write(
            pkg.join("core.py"),
            "import os\n\nclass Engine:\n    def run(self, n):\n        return n\n",
        )
        .unwrap();
        let doc = dir.join("main.py");
        let src = "from helpers import scale\nimport pkg\nfrom pkg import Engine\nscale(1)\npkg.Engine\nEngine.run\n";
        let at = |line0: u32, col: u32| definition_at(src, Some(&doc), line0, col);
        let helpers = Some(dir.join("helpers.py"));
        let core = Some(pkg.join("core.py"));
        // `scale` called, and named in its own import.
        let scale = Target {
            file: helpers.clone(),
            line: 2,
            character: 4,
            len: 5,
        };
        assert_eq!(at(3, 1), Some(scale));
        assert_eq!(at(0, 21).map(|t| t.file), Some(helpers));
        // `Engine` through `pkg/__init__.py`'s re-export.
        let engine = Target {
            file: core.clone(),
            line: 2,
            character: 6,
            len: 6,
        };
        assert_eq!(at(2, 17), Some(engine));
        let engine = Target {
            file: core.clone(),
            line: 2,
            character: 6,
            len: 6,
        };
        assert_eq!(at(4, 6), Some(engine));
        // A method of the imported class, and the package itself.
        let run = Target {
            file: core,
            line: 3,
            character: 8,
            len: 3,
        };
        assert_eq!(at(5, 8), Some(run));
        let init = Target {
            file: Some(pkg.join("__init__.py")),
            line: 0,
            character: 0,
            len: 0,
        };
        assert_eq!(at(1, 8), Some(init));
        // Signature help reads the imported definition.
        let text = format!("{src}scale(2, ");
        let sig = signature_help_at(&text, Some(&doc), 6, 9).expect("signature");
        assert_eq!(sig.label, "scale(value, factor=10)");
        assert_eq!(sig.active, 1);
        assert_eq!(sig.doc.as_deref(), Some("Scale."));
        // Without the document's path the module cannot be found, and the
        // import statement is as far as the name goes.
        let import = Target {
            file: None,
            line: 0,
            character: 20,
            len: 5,
        };
        assert_eq!(definition_at(src, None, 3, 1), Some(import));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

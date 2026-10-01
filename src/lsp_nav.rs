//! Name resolution over one open document, for the LSP's go-to-definition and
//! signature help (`src/lsp.rs`).
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
//! Positions are 0-based `(line, character)` with `character` counted in
//! `char`s, as the rest of the server counts them.

use std::collections::HashSet;

use crate::ast::{Expr, Params, Pattern, Stmt, StmtKind};

/// What introduced a binding, which decides where on its line the name sits.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BindKind {
    Def,
    Class,
    Param,
    Other,
}

#[derive(Debug)]
struct Binding {
    name: String,
    /// 1-based line of the binding statement (a `def`'s own line for its
    /// parameters).
    line: u32,
    kind: BindKind,
    /// For a `def`/`class`, the definition itself (signature help reads it).
    stmt: Option<Stmt>,
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
        }]);
        scopes.block(stmts, 0, u32::MAX);
        scopes
    }

    fn bind(&mut self, scope: usize, name: &str, line: u32, kind: BindKind, stmt: Option<&Stmt>) {
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
                    let bound = match &a.asname {
                        Some(n) => n.as_str(),
                        None => a.name.split('.').next().unwrap_or(&a.name),
                    };
                    self.bind(scope, bound, s.line, BindKind::Other, None);
                }
            }
            StmtKind::ImportFrom { names, .. } => {
                for a in names {
                    let bound = a.asname.as_deref().unwrap_or(&a.name);
                    if bound != "*" {
                        self.bind(scope, bound, s.line, BindKind::Other, None);
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
                        .filter(|b| b.line <= line)
                        .last();
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
    match p {
        Pattern::Capture(n) => out.push(n.clone()),
        Pattern::As(inner, n) => {
            pattern_names(inner, out);
            out.push(n.clone());
        }
        Pattern::Or(alts) => alts.iter().for_each(|a| pattern_names(a, out)),
        Pattern::Sequence { elems, .. } => elems.iter().for_each(|e| pattern_names(e, out)),
        Pattern::Star(Some(n)) => out.push(n.clone()),
        Pattern::Mapping { keys, rest } => {
            keys.iter().for_each(|(_, v)| pattern_names(v, out));
            out.extend(rest.iter().cloned());
        }
        Pattern::Class { pos, kw, .. } => {
            pos.iter().for_each(|q| pattern_names(q, out));
            kw.iter().for_each(|(_, q)| pattern_names(q, out));
        }
        Pattern::Wildcard | Pattern::Value(_) | Pattern::Star(None) => {}
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
            && chars.get(i + want.len()).is_none_or(|c| !is_ident(*c))
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

/// Go-to-definition: where the name under the cursor is bound, as
/// `(line0, char0, length)`. `None` for a builtin, an attribute, or a name the
/// document never binds.
pub fn definition(text: &str, line0: u32, char0: u32) -> Option<(u32, u32, u32)> {
    let lines: Vec<&str> = text.lines().collect();
    let name = word_at(&lines, line0 as usize, char0 as usize)?;
    // `obj.name` is an attribute, not a name lookup.
    let chars: Vec<char> = lines[line0 as usize].chars().collect();
    let start = (0..=(char0 as usize).min(chars.len()))
        .rev()
        .find(|&i| i == 0 || !is_ident(chars[i - 1]))
        .unwrap_or(0);
    if start > 0 && chars[start - 1] == '.' {
        return None;
    }
    let stmts = parse_tolerant(text, line0 as usize)?;
    let scopes = Scopes::build(&stmts);
    let b = scopes.resolve(&name, line0 + 1)?;
    let (l, c) = binding_position(&lines, b);
    Some((l, c, name.chars().count() as u32))
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

/// Signature help for the call the cursor is inside, when its callee is a name
/// this document defines with `def` or `class` (a class shows `__init__`
/// without `self`).
pub fn signature_help(text: &str, line0: u32, char0: u32) -> Option<Signature> {
    let (open, args) = open_call(text, line0 as usize, char0 as usize)?;
    // The callee is the identifier right before the `(`.
    let chars: Vec<char> = text.chars().collect();
    let mut end = open - 1;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    if start == end || (start > 0 && chars[start - 1] == '.') {
        return None;
    }
    let callee: String = chars[start..end].iter().collect();
    let call_line = chars[..start].iter().filter(|c| **c == '\n').count();

    let lines: Vec<&str> = text.lines().collect();
    let stmts = parse_tolerant(text, line0 as usize)?;
    let scopes = Scopes::build(&stmts);
    let b = scopes.resolve(&callee, call_line as u32 + 1)?;
    let (def_name, def_line, body, drop_self) = match &b.stmt.as_ref()?.kind {
        StmtKind::FuncDef { name, body, .. } => (name.clone(), b.line, body.clone(), false),
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
}

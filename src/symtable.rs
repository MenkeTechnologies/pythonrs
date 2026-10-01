//! The declaration checks of CPython's symbol table (`Python/symtable.c`).
//!
//! CPython builds a symbol table for the whole module before it generates any
//! code, visiting statements in source order and OR-ing flags into each name as
//! it goes. A `global` or `nonlocal` statement then looks at the flags its names
//! ALREADY carry in the current scope (`symtable_visit_stmt`, `Global_kind` /
//! `Nonlocal_kind`): a parameter, a use, an annotation or an assignment seen
//! earlier makes the declaration a `SyntaxError`, pointed at the declaring
//! statement. Once every block is built, the analysis pass (`analyze_name`)
//! rejects a name declared both `global` and `nonlocal` in one scope.
//!
//! What counts as "earlier" follows the table's own scoping: a nested `def`,
//! `class`, `lambda` or comprehension body is a block of its own, so a name
//! used only there flags nothing here. What is evaluated in the enclosing scope
//! still does — decorators, default values, base classes, the first iterable of
//! a comprehension, and the target of a `:=` inside a comprehension (PEP 572
//! binds it in the enclosing function). Annotations are not: under PEP 649
//! they are compiled into an annotation scope of their own. An `import` sets
//! `DEF_IMPORT`, which the declaration check does not test, so `import x`
//! followed by `global x` is accepted, as it is by CPython.

use crate::ast::*;
use crate::parser::{at_pos, SYNTAX_FIELD};
use indexmap::IndexMap;

/// `DEF_PARAM`: bound as a parameter of the current function.
const PARAM: u8 = 1;
/// `USE`: read in the current scope.
const USE: u8 = 2;
/// `DEF_ANNOT`: the target of an annotated assignment.
const ANNOT: u8 = 4;
/// `DEF_LOCAL`: bound in the current scope (assignment, `for`, `with … as`,
/// `except … as`, `del`, `def`, `class`, a pattern capture, `:=`).
const LOCAL: u8 = 8;
/// `DEF_GLOBAL`: declared `global`.
const GLOBAL: u8 = 16;
/// `DEF_NONLOCAL`: declared `nonlocal`.
const NONLOCAL: u8 = 32;

/// One block of the table: the flags of each name in first-seen order, and
/// where each name was first declared `global` or `nonlocal` (the location
/// the analysis pass reports a conflict at).
#[derive(Default)]
struct Block {
    flags: IndexMap<String, u8>,
    directives: IndexMap<String, (u32, u32, u32, u32)>,
}

impl Block {
    fn add(&mut self, name: &str, flag: u8) {
        *self.flags.entry(name.to_string()).or_insert(0) |= flag;
    }
}

/// Run the checks over a module. The first declaration error in source order
/// wins; failing that, the first `global`/`nonlocal` conflict in the order the
/// analysis pass visits blocks (a block before the blocks nested in it).
pub fn check(stmts: &[Stmt]) -> Result<(), String> {
    let mut table = Table::default();
    let order = table.open();
    let mut module = Block::default();
    table.body(stmts, &mut module)?;
    table.close(module, order);
    table.analysis.sort_by_key(|(order, _)| *order);
    match table.analysis.into_iter().next() {
        Some((_, e)) => Err(e),
        None => Ok(()),
    }
}

#[derive(Default)]
struct Table {
    /// How many blocks have been opened, which numbers them in the order the
    /// analysis pass would reach them.
    opened: usize,
    /// Analysis-pass errors, tagged with the number of the block they are in.
    analysis: Vec<(usize, String)>,
}

impl Table {
    /// Number a new block. Blocks are numbered as they are opened, so a block
    /// precedes the blocks nested in it, which is the order the analysis pass
    /// visits them in.
    fn open(&mut self) -> usize {
        self.opened += 1;
        self.opened - 1
    }

    /// Finish block number `order`: record its first `global`/`nonlocal`
    /// conflict.
    fn close(&mut self, block: Block, order: usize) {
        let conflict = block
            .flags
            .iter()
            .find(|(_, f)| **f & GLOBAL != 0 && **f & NONLOCAL != 0);
        if let Some((name, _)) = conflict {
            let span = block.directives[name];
            let msg = format!("SyntaxError: name '{name}' is nonlocal and global");
            self.analysis.push((order, symtable_error(&msg, span)));
        }
    }

    /// Visit a nested block's body with a fresh table, `params` pre-flagged.
    fn nested(&mut self, params: &[&str], body: &[Stmt]) -> Result<(), String> {
        let order = self.open();
        let mut block = Block::default();
        for p in params {
            block.add(p, PARAM);
        }
        self.body(body, &mut block)?;
        self.close(block, order);
        Ok(())
    }

    fn body(&mut self, stmts: &[Stmt], b: &mut Block) -> Result<(), String> {
        for s in stmts {
            self.stmt(s, b)?;
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt, b: &mut Block) -> Result<(), String> {
        match &s.kind {
            StmtKind::Expr(e) => self.expr(e, b)?,
            // The value is a scope of its own, evaluated lazily.
            StmtKind::TypeAlias { name, .. } => b.add(name, LOCAL),
            StmtKind::Assign { targets, value } => {
                for t in targets {
                    self.store(t, b)?;
                }
                self.expr(value, b)?;
            }
            StmtKind::AugAssign { target, value, .. } => {
                self.store(target, b)?;
                self.expr(value, b)?;
            }
            StmtKind::AnnAssign { target, value, .. } => {
                match target.unspanned() {
                    Expr::Name(n) => {
                        let flag = if value.is_some() { ANNOT | LOCAL } else { ANNOT };
                        b.add(n, flag);
                    }
                    _ => self.store(target, b)?,
                }
                if let Some(v) = value {
                    self.expr(v, b)?;
                }
            }
            StmtKind::If { test, body, orelse } | StmtKind::While { test, body, orelse } => {
                self.expr(test, b)?;
                self.body(body, b)?;
                self.body(orelse, b)?;
            }
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                ..
            } => {
                self.store(target, b)?;
                self.expr(iter, b)?;
                self.body(body, b)?;
                self.body(orelse, b)?;
            }
            StmtKind::With { items, body, .. } => {
                for it in items {
                    self.expr(&it.context, b)?;
                    if let Some(v) = &it.vars {
                        self.store(v, b)?;
                    }
                }
                self.body(body, b)?;
            }
            StmtKind::FuncDef {
                name,
                params,
                body,
                decorators,
                ..
            } => {
                for d in decorators {
                    self.expr(d, b)?;
                }
                self.defaults(params, b)?;
                b.add(name, LOCAL);
                let names = param_names(params);
                self.nested(&names, body)?;
            }
            StmtKind::ClassDef {
                name,
                bases,
                keywords,
                body,
                decorators,
            } => {
                for d in decorators {
                    self.expr(d, b)?;
                }
                for e in bases {
                    self.expr(e, b)?;
                }
                for k in keywords {
                    self.expr(&k.value, b)?;
                }
                b.add(name, LOCAL);
                self.nested(&[], body)?;
            }
            StmtKind::Return(Some(e)) => self.expr(e, b)?,
            StmtKind::Delete(targets) => {
                for t in targets {
                    self.store(t, b)?;
                }
            }
            StmtKind::Global(names) => self.declare(names, GLOBAL, s.span, b)?,
            StmtKind::Nonlocal(names) => self.declare(names, NONLOCAL, s.span, b)?,
            StmtKind::Raise { exc, cause } => {
                for e in [exc, cause].into_iter().flatten() {
                    self.expr(e, b)?;
                }
            }
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                self.body(body, b)?;
                for h in handlers {
                    if let Some(t) = &h.typ {
                        self.expr(t, b)?;
                    }
                    if let Some(n) = &h.name {
                        b.add(n, LOCAL);
                    }
                    self.body(&h.body, b)?;
                }
                self.body(orelse, b)?;
                self.body(finalbody, b)?;
            }
            StmtKind::Assert { test, msg } => {
                self.expr(test, b)?;
                if let Some(m) = msg {
                    self.expr(m, b)?;
                }
            }
            StmtKind::Match { subject, cases } => {
                self.expr(subject, b)?;
                for c in cases {
                    self.pattern(&c.pattern, b)?;
                    if let Some(g) = &c.guard {
                        self.expr(g, b)?;
                    }
                    self.body(&c.body, b)?;
                }
            }
            StmtKind::Return(None)
            | StmtKind::Pass
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import(_)
            | StmtKind::ImportFrom { .. } => {}
        }
        Ok(())
    }

    /// `global` / `nonlocal`: each name must not already be a parameter, used,
    /// annotated or bound in this block.
    fn declare(
        &mut self,
        names: &[String],
        kind: u8,
        span: Option<(u32, u32, u32, u32)>,
        b: &mut Block,
    ) -> Result<(), String> {
        let word = if kind == GLOBAL { "global" } else { "nonlocal" };
        for n in names {
            let cur = b.flags.get(n).copied().unwrap_or(0);
            let msg = if cur & PARAM != 0 {
                Some(format!("name '{n}' is parameter and {word}"))
            } else if cur & USE != 0 {
                Some(format!("name '{n}' is used prior to {word} declaration"))
            } else if cur & ANNOT != 0 {
                Some(format!("annotated name '{n}' can't be {word}"))
            } else if cur & LOCAL != 0 {
                Some(format!("name '{n}' is assigned to before {word} declaration"))
            } else {
                None
            };
            if let (Some(msg), Some(span)) = (msg, span) {
                return Err(symtable_error(&format!("SyntaxError: {msg}"), span));
            }
            b.add(n, kind);
            if let Some(span) = span {
                b.directives.entry(n.clone()).or_insert(span);
            }
        }
        Ok(())
    }

    /// The default values of a `def` or `lambda`, evaluated where it stands.
    fn defaults(&mut self, params: &Params, b: &mut Block) -> Result<(), String> {
        for d in &params.defaults {
            self.expr(d, b)?;
        }
        for d in params.kwonly_defaults.iter().flatten() {
            self.expr(d, b)?;
        }
        Ok(())
    }

    /// An assignment or `del` target: a name is bound, and the object of an
    /// attribute or subscript target is read.
    fn store(&mut self, t: &Expr, b: &mut Block) -> Result<(), String> {
        match t.unspanned() {
            Expr::Name(n) => b.add(n, LOCAL),
            Expr::Tuple(xs) | Expr::List(xs) => {
                for x in xs {
                    self.store(x, b)?;
                }
            }
            Expr::Starred(x) => self.store(x, b)?,
            other => self.expr(other, b)?,
        }
        Ok(())
    }

    fn pattern(&mut self, p: &Pattern, b: &mut Block) -> Result<(), String> {
        match p {
            Pattern::Wildcard | Pattern::Star(None) => {}
            Pattern::Capture(n) | Pattern::Star(Some(n)) => b.add(n, LOCAL),
            Pattern::Value(e) => self.expr(e, b)?,
            Pattern::Or(alts) => {
                for a in alts {
                    self.pattern(a, b)?;
                }
            }
            Pattern::As(inner, n) => {
                self.pattern(inner, b)?;
                b.add(n, LOCAL);
            }
            Pattern::Sequence { elems, .. } => {
                for e in elems {
                    self.pattern(e, b)?;
                }
            }
            Pattern::Mapping { keys, rest } => {
                for (k, v) in keys {
                    self.expr(k, b)?;
                    self.pattern(v, b)?;
                }
                if let Some(r) = rest {
                    b.add(r, LOCAL);
                }
            }
            Pattern::Class { cls, pos, kw } => {
                self.expr(cls, b)?;
                for p in pos {
                    self.pattern(p, b)?;
                }
                for (_, p) in kw {
                    self.pattern(p, b)?;
                }
            }
        }
        Ok(())
    }

    /// An expression evaluated in this block.
    fn expr(&mut self, e: &Expr, b: &mut Block) -> Result<(), String> {
        match e.unspanned() {
            Expr::Name(n) => b.add(n, USE),
            Expr::NamedExpr(target, value) => {
                self.store(target, b)?;
                self.expr(value, b)?;
            }
            // The body is a block of its own, and one that can hold no
            // declaration; only the defaults are evaluated here.
            Expr::Lambda { params, .. } => self.defaults(params, b)?,
            Expr::ListComp(..) | Expr::SetComp(..) | Expr::GenExp(..) | Expr::DictComp(..) => {
                let (elts, comps) = comprehension_parts(e.unspanned());
                self.comprehension(&elts, comps, b)?;
                no_yield_in_comprehension(e.unspanned())?;
            }
            Expr::List(xs) | Expr::Tuple(xs) | Expr::Set(xs) | Expr::BoolOp(_, xs) => {
                for x in xs {
                    self.expr(x, b)?;
                }
            }
            Expr::Dict(pairs) => {
                for (k, v) in pairs {
                    if let Some(k) = k {
                        self.expr(k, b)?;
                    }
                    self.expr(v, b)?;
                }
            }
            Expr::Starred(x)
            | Expr::UnaryOp(_, x)
            | Expr::YieldFrom(x)
            | Expr::Await(x)
            | Expr::Attribute(x, _)
            | Expr::Yield(Some(x)) => self.expr(x, b)?,
            Expr::BinOp(_, l, r) | Expr::Subscript(l, r) => {
                self.expr(l, b)?;
                self.expr(r, b)?;
            }
            Expr::Compare(l, rest) => {
                self.expr(l, b)?;
                for (_, x) in rest {
                    self.expr(x, b)?;
                }
            }
            Expr::IfExp { test, body, orelse } => {
                self.expr(test, b)?;
                self.expr(body, b)?;
                self.expr(orelse, b)?;
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                self.expr(func, b)?;
                for a in args {
                    self.expr(a, b)?;
                }
                for k in keywords {
                    self.expr(&k.value, b)?;
                }
            }
            Expr::Slice { lo, hi, step } => {
                for x in [lo, hi, step].into_iter().flatten() {
                    self.expr(x, b)?;
                }
            }
            Expr::FString(parts) | Expr::TString(parts) => self.fstring(parts, b)?,
            _ => {}
        }
        Ok(())
    }

    fn fstring(&mut self, parts: &[FStrPart], b: &mut Block) -> Result<(), String> {
        for p in parts {
            if let FStrPart::Expr { expr, spec, .. } = p {
                self.expr(expr, b)?;
                self.fstring(spec, b)?;
            }
        }
        Ok(())
    }

    /// A comprehension: its first iterable is evaluated here and everything
    /// else runs in the comprehension's own block — which can hold no
    /// declaration — except that a `:=` target in it binds in this one.
    fn comprehension(
        &mut self,
        elts: &[&Expr],
        comps: &[Comprehension],
        b: &mut Block,
    ) -> Result<(), String> {
        let mut targets = Vec::new();
        for (i, c) in comps.iter().enumerate() {
            if i == 0 {
                self.expr(&c.iter, b)?;
            } else {
                walrus_targets(&c.iter, &mut targets);
            }
            for cond in &c.ifs {
                walrus_targets(cond, &mut targets);
            }
        }
        for e in elts {
            walrus_targets(e, &mut targets);
        }
        for t in targets {
            b.add(&t, LOCAL);
        }
        Ok(())
    }
}

/// The value expressions and the clauses of a comprehension.
fn comprehension_parts(e: &Expr) -> (Vec<&Expr>, &[Comprehension]) {
    match e {
        Expr::ListComp(elt, comps) | Expr::SetComp(elt, comps) | Expr::GenExp(elt, comps) => {
            (vec![&**elt], comps)
        }
        Expr::DictComp(k, v, comps) => (vec![&**k, &**v], comps),
        _ => (Vec::new(), &[]),
    }
}

/// A comprehension's body is a function block of its own, and not one that
/// may suspend: a `yield` there is `symtable_raise_if_comprehension_block`'s
/// error, named for the innermost comprehension holding it. Its first iterable
/// belongs to the enclosing block and is checked there; a `lambda` is a block
/// of its own.
fn no_yield_in_comprehension(comp: &Expr) -> Result<(), String> {
    let kind = match comp {
        Expr::ListComp(..) => "list comprehension",
        Expr::SetComp(..) => "set comprehension",
        Expr::DictComp(..) => "dict comprehension",
        _ => "generator expression",
    };
    let (elts, comps) = comprehension_parts(comp);
    let mut parts: Vec<&Expr> = elts;
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            parts.push(&c.iter);
        }
        parts.push(&c.target);
        parts.extend(c.ifs.iter());
    }
    parts.into_iter().try_for_each(|p| yield_in(p, kind))
}

fn yield_in(e: &Expr, kind: &str) -> Result<(), String> {
    match e.unspanned() {
        Expr::Yield(_) | Expr::YieldFrom(_) => {
            let msg = format!("SyntaxError: 'yield' inside {kind}");
            let sp = e.span();
            if !sp.is_some() {
                return Err(msg);
            }
            Err(symtable_error(&msg, (sp.line, sp.start + 1, sp.line, sp.end + 1)))
        }
        Expr::Lambda { .. } => Ok(()),
        inner @ (Expr::ListComp(..) | Expr::SetComp(..) | Expr::GenExp(..) | Expr::DictComp(..)) => {
            // The nested comprehension's first iterable is evaluated in THIS
            // one; the rest is its own block.
            let (_, comps) = comprehension_parts(inner);
            if let Some(first) = comps.first() {
                yield_in(&first.iter, kind)?;
            }
            no_yield_in_comprehension(inner)
        }
        other => crate::compiler::expr_children(other)
            .into_iter()
            .try_for_each(|c| yield_in(c, kind)),
    }
}

/// The `:=` targets inside `e`, nested comprehensions included (their targets
/// bind in the same enclosing function) but not `lambda` bodies.
fn walrus_targets(e: &Expr, out: &mut Vec<String>) {
    let e = e.unspanned();
    if let Expr::NamedExpr(t, _) = e {
        if let Expr::Name(n) = t.unspanned() {
            out.push(n.clone());
        }
    }
    if matches!(e, Expr::Lambda { .. }) {
        return;
    }
    for child in crate::compiler::expr_children(e) {
        walrus_targets(child, out);
    }
}

/// Every name a parameter list binds, in order.
fn param_names(p: &Params) -> Vec<&str> {
    let mut out: Vec<&str> = p.names.iter().map(String::as_str).collect();
    if let Some(s) = p.star.as_deref().filter(|s| !s.is_empty()) {
        out.push(s);
    }
    out.extend(p.kwonly.iter().map(String::as_str));
    if let Some(k) = p.kwargs.as_deref() {
        out.push(k);
    }
    out
}

/// A symbol-table `SyntaxError` at `span`: it carries the position as
/// attributes only (`args == (msg,)`) and no source text of its own — see
/// `SyntaxPos::bare_args` and `SyntaxPos::no_source`.
pub(crate) fn symtable_error(msg: &str, (line, col, end_line, end_col): (u32, u32, u32, u32)) -> String {
    format!(
        "{}{SYNTAX_FIELD}nosrc=1{SYNTAX_FIELD}bare=1",
        at_pos(msg, line, col as i64, end_line, end_col as i64)
    )
}

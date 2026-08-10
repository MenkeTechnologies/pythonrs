//! Private-name mangling — CPython's `_Py_Mangle`.
//!
//! Any identifier of the form `__spam` (at least two leading underscores, at
//! most one trailing underscore) written textually inside a class body becomes
//! `_Classname__spam`, with leading underscores stripped from the class name.
//! It is what makes `self.__x` private: two classes in one hierarchy can each
//! keep a `__x` without colliding, and a subclass cannot accidentally clobber
//! a base's private attribute.
//!
//! ```text
//! class C:
//!     def __init__(self): self.__x = 1
//! C().__dict__            # CPython: {'_C__x': 1}
//! ```
//!
//! Without it `C().__dict__` is `{'__x': 1}`: the privacy guarantee is gone,
//! every `__x` in a hierarchy aliases every other, and `__slots__ = ('__x',)`
//! beside a `_C__x` class variable is accepted where CPython raises
//! `ValueError: '_C__x' in __slots__ conflicts with class variable`.
//!
//! # Where this runs
//!
//! In the compiler, on the class body, after parsing — the same place CPython
//! does it. `ast.parse` must NOT see mangled names (`ast.dump` of
//! `class C:\n def m(self): return self.__x` contains `__x`, not `_C__x`), and
//! `ast.parse` here routes through `compile(..., PyCF_ONLY_AST)` over the same
//! parser, so mangling at parse time would corrupt it.
//!
//! # What mangles
//!
//! Every identifier lexically inside the class body, including bodies of
//! methods and nested functions, verified against CPython 3.14:
//!
//! | written                      | becomes           |
//! |------------------------------|-------------------|
//! | `self.__x` (attribute)       | `self._C__x`      |
//! | `__y = 2` (class variable)   | `_C__y`           |
//! | `def __m(self)`              | `_C__m`           |
//! | `global __g` / `nonlocal __g`| `_C__g`           |
//! | a local, a parameter         | `_C__local`       |
//! | `import __m` / `except E as __e` | `_C__m` / `_C__e` |
//! | a `match` capture / `as` name| `_C__n`           |
//!
//! and what does NOT:
//!
//! | written                     | reason                                  |
//! |-----------------------------|-----------------------------------------|
//! | `f(__k=1)` (keyword arg)    | a call keyword is not an identifier ref |
//! | `__x__`                     | two or more trailing underscores        |
//! | `_z`, `__` , `___`          | fewer than two leading, or all underscore |
//! | the class's own name        | it is written in the ENCLOSING scope    |
//!
//! The innermost enclosing class wins: `class M: class N: ...` mangles `N`'s
//! body with `N`.

use crate::ast::{
    Comprehension, ExceptHandler, Expr, FStrPart, Keyword, MatchCase, Params, Pattern, Stmt,
    StmtKind, WithItem,
};

/// CPython `Python/compile.c:_Py_Mangle`. `None` when `name` is not private.
///
/// A private name has two or more leading underscores and at most one trailing
/// underscore. Leading underscores are stripped from the class name, and a
/// class name that is nothing but underscores mangles nothing at all (there
/// would be no prefix left to disambiguate with).
pub fn mangle(class: &str, name: &str) -> Option<String> {
    if !name.starts_with("__") || name.ends_with("__") {
        return None;
    }
    let stripped = class.trim_start_matches('_');
    if stripped.is_empty() {
        return None;
    }
    Some(format!("_{stripped}{name}"))
}

/// Rewrite `name` in place if it is private under `class`.
fn fix(class: &str, name: &mut String) {
    if let Some(m) = mangle(class, name) {
        *name = m;
    }
}

/// Whether `body` contains a `class` statement at any depth.
///
/// Nothing outside a class body can mangle, so a program with no class needs no
/// rewrite and no clone of the AST to hold it. A class can only ever appear as a
/// STATEMENT — never inside an expression, not even a `lambda` — so recursing
/// through the block-bearing statements is exhaustive.
pub fn has_class(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::ClassDef { .. } => true,
        StmtKind::If { body, orelse, .. }
        | StmtKind::While { body, orelse, .. }
        | StmtKind::For { body, orelse, .. } => has_class(body) || has_class(orelse),
        StmtKind::With { body, .. } | StmtKind::FuncDef { body, .. } => has_class(body),
        StmtKind::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            has_class(body)
                || handlers.iter().any(|h| has_class(&h.body))
                || has_class(orelse)
                || has_class(finalbody)
        }
        StmtKind::Match { cases, .. } => cases.iter().any(|c| has_class(&c.body)),
        _ => false,
    })
}

/// Apply mangling to every identifier in a class body.
///
/// Call with the class's own name on the statements of its body. Nested class
/// bodies are re-entered under their own name, matching CPython's innermost-
/// class rule.
pub fn mangle_body(class: &str, body: &mut [Stmt]) {
    for s in body {
        stmt(class, s);
    }
}

fn block(class: &str, body: &mut [Stmt]) {
    for s in body {
        stmt(class, s);
    }
}

fn stmt(class: &str, s: &mut Stmt) {
    match &mut s.kind {
        StmtKind::Expr(e) => expr(class, e),
        StmtKind::Assign { targets, value } => {
            for t in targets {
                expr(class, t);
            }
            expr(class, value);
        }
        StmtKind::AugAssign { target, value, .. } => {
            expr(class, target);
            expr(class, value);
        }
        StmtKind::AnnAssign {
            target,
            annotation,
            value,
        } => {
            expr(class, target);
            expr(class, annotation);
            if let Some(v) = value {
                expr(class, v);
            }
        }
        StmtKind::If { test, body, orelse } | StmtKind::While { test, body, orelse } => {
            expr(class, test);
            block(class, body);
            block(class, orelse);
        }
        StmtKind::For {
            target,
            iter,
            body,
            orelse,
            ..
        } => {
            expr(class, target);
            expr(class, iter);
            block(class, body);
            block(class, orelse);
        }
        StmtKind::With { items, body, .. } => {
            for WithItem { context, vars } in items {
                expr(class, context);
                if let Some(v) = vars {
                    expr(class, v);
                }
            }
            block(class, body);
        }
        StmtKind::FuncDef {
            name,
            params,
            body,
            decorators,
            ..
        } => {
            fix(class, name);
            parameters(class, params);
            block(class, body);
            for d in decorators {
                expr(class, d);
            }
        }
        // A nested class: its own name mangles under the OUTER class, and its
        // body then mangles under its own name (innermost wins). Its bases and
        // decorators are evaluated in the outer scope, so they keep the outer.
        StmtKind::ClassDef {
            name,
            bases,
            keywords,
            body,
            decorators,
        } => {
            let inner = name.clone();
            fix(class, name);
            for b in bases {
                expr(class, b);
            }
            for k in keywords {
                expr(class, &mut k.value);
            }
            for d in decorators {
                expr(class, d);
            }
            block(&inner, body);
        }
        StmtKind::Return(Some(e)) => expr(class, e),
        StmtKind::Return(None) | StmtKind::Pass | StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Delete(xs) => {
            for x in xs {
                expr(class, x);
            }
        }
        // `import __m` binds `__m`; `import a.b as __m` binds the asname.
        StmtKind::Import(names) => {
            for a in names {
                match &mut a.asname {
                    Some(n) => fix(class, n),
                    None => fix(class, &mut a.name),
                }
            }
        }
        // `from m import __n` binds `__n`, but the name IMPORTED from the module
        // is the unmangled one, so only an `as` binding can be rewritten.
        StmtKind::ImportFrom { names, .. } => {
            for a in names {
                if let Some(n) = &mut a.asname {
                    fix(class, n);
                }
            }
        }
        StmtKind::Global(names) | StmtKind::Nonlocal(names) => {
            for n in names {
                fix(class, n);
            }
        }
        StmtKind::Raise { exc, cause } => {
            if let Some(e) = exc {
                expr(class, e);
            }
            if let Some(c) = cause {
                expr(class, c);
            }
        }
        StmtKind::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            block(class, body);
            for ExceptHandler {
                typ, name, body, ..
            } in handlers
            {
                if let Some(t) = typ {
                    expr(class, t);
                }
                if let Some(n) = name {
                    fix(class, n);
                }
                block(class, body);
            }
            block(class, orelse);
            block(class, finalbody);
        }
        StmtKind::Assert { test, msg } => {
            expr(class, test);
            if let Some(m) = msg {
                expr(class, m);
            }
        }
        StmtKind::Match { subject, cases } => {
            expr(class, subject);
            for MatchCase {
                pattern,
                guard,
                body,
            } in cases
            {
                pat(class, pattern);
                if let Some(g) = guard {
                    expr(class, g);
                }
                block(class, body);
            }
        }
    }
}

/// Parameter names are ordinary locals, so they mangle. Annotations and
/// defaults are expressions in the same class scope.
fn parameters(class: &str, p: &mut Params) {
    for n in &mut p.names {
        fix(class, n);
    }
    for n in &mut p.kwonly {
        fix(class, n);
    }
    if let Some(n) = &mut p.star {
        fix(class, n);
    }
    if let Some(n) = &mut p.kwargs {
        fix(class, n);
    }
    for d in &mut p.defaults {
        expr(class, d);
    }
    for d in p.kwonly_defaults.iter_mut().flatten() {
        expr(class, d);
    }
    for (n, a) in &mut p.annotations {
        fix(class, n);
        expr(class, a);
    }
}

fn pat(class: &str, p: &mut Pattern) {
    match p {
        Pattern::Wildcard => {}
        Pattern::Capture(n) => fix(class, n),
        Pattern::Value(e) => expr(class, e),
        Pattern::Or(ps) => {
            for q in ps {
                pat(class, q);
            }
        }
        Pattern::As(inner, n) => {
            pat(class, inner);
            fix(class, n);
        }
        Pattern::Sequence { elems, .. } => {
            for q in elems {
                pat(class, q);
            }
        }
        Pattern::Star(Some(n)) => fix(class, n),
        Pattern::Star(None) => {}
        Pattern::Mapping { keys, rest } => {
            for (k, q) in keys {
                expr(class, k);
                pat(class, q);
            }
            if let Some(n) = rest {
                fix(class, n);
            }
        }
        Pattern::Class { cls, pos, kw } => {
            expr(class, cls);
            for q in pos {
                pat(class, q);
            }
            // A class pattern's keyword is an ATTRIBUTE name on the subject
            // (`Point(x=0)` reads `subject.x`), so it mangles like one.
            for (n, q) in kw {
                fix(class, n);
                pat(class, q);
            }
        }
    }
}

fn comps(class: &str, cs: &mut [Comprehension]) {
    for c in cs {
        expr(class, &mut c.target);
        expr(class, &mut c.iter);
        for i in &mut c.ifs {
            expr(class, i);
        }
    }
}

fn fparts(class: &str, ps: &mut [FStrPart]) {
    for p in ps {
        if let FStrPart::Expr { expr: e, spec, .. } = p {
            expr(class, e);
            fparts(class, spec);
        }
    }
}

fn expr(class: &str, e: &mut Expr) {
    match e {
        Expr::Name(n) => fix(class, n),
        // `value.__attr` — the attribute name mangles, the value is an
        // expression in the same scope.
        Expr::Attribute(v, attr) => {
            expr(class, v);
            fix(class, attr);
        }
        Expr::None
        | Expr::True
        | Expr::False
        | Expr::Ellipsis
        | Expr::Int(_)
        | Expr::BigInt(_)
        | Expr::Float(_)
        | Expr::Complex(_)
        | Expr::Str(_)
        | Expr::Bytes(_) => {}
        Expr::FString(ps) | Expr::TString(ps) => fparts(class, ps),
        Expr::List(xs) | Expr::Tuple(xs) | Expr::Set(xs) => {
            for x in xs {
                expr(class, x);
            }
        }
        Expr::Dict(items) => {
            for (k, v) in items {
                if let Some(k) = k {
                    expr(class, k);
                }
                expr(class, v);
            }
        }
        Expr::Starred(x) | Expr::UnaryOp(_, x) | Expr::YieldFrom(x) | Expr::Await(x) => {
            expr(class, x)
        }
        Expr::BoolOp(_, xs) => {
            for x in xs {
                expr(class, x);
            }
        }
        Expr::BinOp(_, a, b) | Expr::NamedExpr(a, b) | Expr::Subscript(a, b) => {
            expr(class, a);
            expr(class, b);
        }
        Expr::Compare(l, links) => {
            expr(class, l);
            for (_, r) in links {
                expr(class, r);
            }
        }
        Expr::IfExp { test, body, orelse } => {
            expr(class, test);
            expr(class, body);
            expr(class, orelse);
        }
        // A call KEYWORD is not an identifier reference — CPython leaves
        // `f(__k=1)` as `__k`. Only the callee and the values mangle.
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            expr(class, func);
            for a in args {
                expr(class, a);
            }
            for Keyword { value, .. } in keywords {
                expr(class, value);
            }
        }
        Expr::Slice { lo, hi, step } => {
            for b in [lo, hi, step].into_iter().flatten() {
                expr(class, b);
            }
        }
        Expr::Lambda { params, body } => {
            parameters(class, params);
            expr(class, body);
        }
        Expr::ListComp(el, cs) | Expr::SetComp(el, cs) | Expr::GenExp(el, cs) => {
            expr(class, el);
            comps(class, cs);
        }
        Expr::DictComp(k, v, cs) => {
            expr(class, k);
            expr(class, v);
            comps(class, cs);
        }
        Expr::Yield(x) => {
            if let Some(x) = x {
                expr(class, x);
            }
        }
        Expr::Spanned(x, _) => expr(class, x),
    }
}

//! An expression back to source text, as CPython's `Python/ast_unparse.c`
//! writes it: the string a PEP 563 annotation (`from __future__ import
//! annotations`) is stored as. Parentheses come from operator precedence, not
//! from the source, so `(a)` is `a` and `2 ** -1` is `2 ** (-1)`; constants are
//! their `repr`, with an infinite float spelled `1e309`.

use crate::ast::*;

// The precedence levels of `ast_unparse.c`, lowest first.
const PR_TUPLE: u8 = 0;
/// `if`-`else`, `lambda`.
const PR_TEST: u8 = 1;
const PR_OR: u8 = 2;
const PR_AND: u8 = 3;
const PR_NOT: u8 = 4;
/// `<`, `==`, `in`, `is not`, …
const PR_CMP: u8 = 5;
/// `|`.
const PR_BOR: u8 = 6;
const PR_EXPR: u8 = PR_BOR;
const PR_BXOR: u8 = 7;
const PR_BAND: u8 = 8;
const PR_SHIFT: u8 = 9;
/// `+`, `-`.
const PR_ARITH: u8 = 10;
/// `*`, `@`, `/`, `%`, `//`.
const PR_TERM: u8 = 11;
/// Unary `+`, `-`, `~`.
const PR_FACTOR: u8 = 12;
const PR_POWER: u8 = 13;
const PR_AWAIT: u8 = 14;
const PR_ATOM: u8 = 15;

/// `_PyAST_ExprAsUnicode`: `e` as an annotation string.
pub fn expr_as_unicode(e: &Expr) -> String {
    let mut out = String::new();
    expr(&mut out, e, PR_TEST);
    out
}

fn expr_at(e: &Expr, level: u8) -> String {
    let mut out = String::new();
    expr(&mut out, e, level);
    out
}

/// `append_ast_expr`: `e` written where precedence `level` is required.
fn expr(out: &mut String, e: &Expr, level: u8) {
    match e.unspanned() {
        Expr::BoolOp(op, values) => {
            let (word, pr) = match op {
                BoolOp::And => (" and ", PR_AND),
                BoolOp::Or => (" or ", PR_OR),
            };
            open(out, level > pr);
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push_str(word);
                }
                expr(out, v, pr + 1);
            }
            close(out, level > pr);
        }
        Expr::BinOp(op, left, right) => {
            let (sym, pr) = match op {
                BinOp::Add => (" + ", PR_ARITH),
                BinOp::Sub => (" - ", PR_ARITH),
                BinOp::Mul => (" * ", PR_TERM),
                BinOp::MatMul => (" @ ", PR_TERM),
                BinOp::Div => (" / ", PR_TERM),
                BinOp::Mod => (" % ", PR_TERM),
                BinOp::FloorDiv => (" // ", PR_TERM),
                BinOp::Shl => (" << ", PR_SHIFT),
                BinOp::Shr => (" >> ", PR_SHIFT),
                BinOp::BitOr => (" | ", PR_BOR),
                BinOp::BitXor => (" ^ ", PR_BXOR),
                BinOp::BitAnd => (" & ", PR_BAND),
                BinOp::Pow => (" ** ", PR_POWER),
            };
            // `**` is right-associative: its left operand binds tighter.
            let rassoc = u8::from(*op == BinOp::Pow);
            open(out, level > pr);
            expr(out, left, pr + rassoc);
            out.push_str(sym);
            expr(out, right, pr + 1 - rassoc);
            close(out, level > pr);
        }
        Expr::UnaryOp(op, operand) => {
            let (sym, pr) = match op {
                UnOp::Invert => ("~", PR_FACTOR),
                UnOp::Not => ("not ", PR_NOT),
                UnOp::Pos => ("+", PR_FACTOR),
                UnOp::Neg => ("-", PR_FACTOR),
            };
            open(out, level > pr);
            out.push_str(sym);
            expr(out, operand, pr);
            close(out, level > pr);
        }
        Expr::Lambda { params, body } => {
            open(out, level > PR_TEST);
            let positional = params.names.len();
            out.push_str(if positional > 0 { "lambda " } else { "lambda" });
            args(out, params);
            out.push_str(": ");
            expr(out, body, PR_TEST);
            close(out, level > PR_TEST);
        }
        Expr::IfExp { test, body, orelse } => {
            open(out, level > PR_TEST);
            expr(out, body, PR_TEST + 1);
            out.push_str(" if ");
            expr(out, test, PR_TEST + 1);
            out.push_str(" else ");
            expr(out, orelse, PR_TEST);
            close(out, level > PR_TEST);
        }
        Expr::Dict(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match k {
                    Some(k) => {
                        expr(out, k, PR_TEST);
                        out.push_str(": ");
                        expr(out, v, PR_TEST);
                    }
                    None => {
                        out.push_str("**");
                        expr(out, v, PR_EXPR);
                    }
                }
            }
            out.push('}');
        }
        Expr::Set(elts) if elts.is_empty() => out.push_str("{*()}"),
        Expr::Set(elts) => {
            out.push('{');
            sequence(out, elts);
            out.push('}');
        }
        Expr::List(elts) => {
            out.push('[');
            sequence(out, elts);
            out.push(']');
        }
        Expr::Tuple(elts) if elts.is_empty() => out.push_str("()"),
        Expr::Tuple(elts) => {
            open(out, level > PR_TUPLE);
            sequence(out, elts);
            if elts.len() == 1 {
                out.push(',');
            }
            close(out, level > PR_TUPLE);
        }
        Expr::ListComp(elt, comps) => {
            out.push('[');
            expr(out, elt, PR_TEST);
            comprehensions(out, comps);
            out.push(']');
        }
        Expr::SetComp(elt, comps) => {
            out.push('{');
            expr(out, elt, PR_TEST);
            comprehensions(out, comps);
            out.push('}');
        }
        Expr::DictComp(k, v, comps) => {
            out.push('{');
            expr(out, k, PR_TEST);
            out.push_str(": ");
            expr(out, v, PR_TEST);
            comprehensions(out, comps);
            out.push('}');
        }
        Expr::GenExp(elt, comps) => genexp(out, elt, comps),
        Expr::Yield(None) => out.push_str("(yield)"),
        Expr::Yield(Some(v)) => {
            out.push_str("(yield ");
            expr(out, v, PR_TEST);
            out.push(')');
        }
        Expr::YieldFrom(v) => {
            out.push_str("(yield from ");
            expr(out, v, PR_TEST);
            out.push(')');
        }
        Expr::Await(v) => {
            open(out, level > PR_AWAIT);
            out.push_str("await ");
            expr(out, v, PR_ATOM);
            close(out, level > PR_AWAIT);
        }
        Expr::Compare(left, rest) => {
            open(out, level > PR_CMP);
            expr(out, left, PR_CMP + 1);
            for (op, right) in rest {
                out.push_str(match op {
                    CmpOp::Eq => " == ",
                    CmpOp::Ne => " != ",
                    CmpOp::Lt => " < ",
                    CmpOp::Le => " <= ",
                    CmpOp::Gt => " > ",
                    CmpOp::Ge => " >= ",
                    CmpOp::Is => " is ",
                    CmpOp::IsNot => " is not ",
                    CmpOp::In => " in ",
                    CmpOp::NotIn => " not in ",
                });
                expr(out, right, PR_CMP + 1);
            }
            close(out, level > PR_CMP);
        }
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            expr(out, func, PR_ATOM);
            // A lone generator expression argument shares the call's parentheses.
            if let ([only], []) = (args.as_slice(), keywords.as_slice()) {
                if let Expr::GenExp(elt, comps) = only.unspanned() {
                    genexp(out, elt, comps);
                    return;
                }
            }
            out.push('(');
            let mut first = true;
            for a in args {
                sep(out, &mut first);
                expr(out, a, PR_TEST);
            }
            for k in keywords {
                sep(out, &mut first);
                match &k.name {
                    Some(name) => {
                        out.push_str(name);
                        out.push('=');
                    }
                    None => out.push_str("**"),
                }
                expr(out, &k.value, PR_TEST);
            }
            out.push(')');
        }
        Expr::None => out.push_str("None"),
        Expr::True => out.push_str("True"),
        Expr::False => out.push_str("False"),
        Expr::Ellipsis => out.push_str("..."),
        Expr::Int(n) => out.push_str(&n.to_string()),
        Expr::BigInt(digits) => out.push_str(digits),
        Expr::Float(f) => out.push_str(&infinite_as_1e309(crate::host::fmt_float(*f))),
        Expr::Complex(imag) => {
            out.push_str(&infinite_as_1e309(crate::host::fmt_complex(0.0, *imag)))
        }
        Expr::Str(s) => out.push_str(&crate::host::quote_str(s)),
        Expr::Bytes(b) => {
            out.push('b');
            out.push_str(&crate::host::quote_bytes(b, false));
        }
        Expr::FString(parts) => {
            out.push('f');
            out.push_str(&crate::host::quote_str(&ftstring_body(parts, false)));
        }
        Expr::TString(parts) => {
            out.push('t');
            out.push_str(&crate::host::quote_str(&ftstring_body(parts, true)));
        }
        Expr::Attribute(value, attr) => {
            expr(out, value, PR_ATOM);
            // `1 .real`: an integer needs a space before the period.
            let int = matches!(value.unspanned(), Expr::Int(_) | Expr::BigInt(_));
            out.push_str(if int { " ." } else { "." });
            out.push_str(attr);
        }
        Expr::Subscript(value, slice) => {
            expr(out, value, PR_ATOM);
            out.push('[');
            expr(out, slice, PR_TUPLE);
            out.push(']');
        }
        Expr::Slice { lo, hi, step } => {
            if let Some(lo) = lo {
                expr(out, lo, PR_TEST);
            }
            out.push(':');
            if let Some(hi) = hi {
                expr(out, hi, PR_TEST);
            }
            if let Some(step) = step {
                out.push(':');
                expr(out, step, PR_TEST);
            }
        }
        Expr::Starred(v) => {
            out.push('*');
            expr(out, v, PR_EXPR);
        }
        Expr::Name(id) => out.push_str(id),
        Expr::NamedExpr(target, value) => {
            open(out, level > PR_TUPLE);
            expr(out, target, PR_ATOM);
            out.push_str(" := ");
            expr(out, value, PR_ATOM);
            close(out, level > PR_TUPLE);
        }
        Expr::Spanned(..) => unreachable!("unspanned() peels every Spanned"),
    }
}

fn open(out: &mut String, paren: bool) {
    if paren {
        out.push('(');
    }
}

fn close(out: &mut String, paren: bool) {
    if paren {
        out.push(')');
    }
}

/// `APPEND_STR_IF_NOT_FIRST(", ")`.
fn sep(out: &mut String, first: &mut bool) {
    if !*first {
        out.push_str(", ");
    }
    *first = false;
}

/// The elements of a list, set or tuple display, each at `PR_TEST`.
fn sequence(out: &mut String, elts: &[Expr]) {
    for (i, e) in elts.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        expr(out, e, PR_TEST);
    }
}

fn genexp(out: &mut String, elt: &Expr, comps: &[Comprehension]) {
    out.push('(');
    expr(out, elt, PR_TEST);
    comprehensions(out, comps);
    out.push(')');
}

fn comprehensions(out: &mut String, comps: &[Comprehension]) {
    for c in comps {
        out.push_str(if c.is_async { " async for " } else { " for " });
        expr(out, &c.target, PR_TUPLE);
        out.push_str(" in ");
        expr(out, &c.iter, PR_TEST + 1);
        for cond in &c.ifs {
            out.push_str(" if ");
            expr(out, cond, PR_TEST + 1);
        }
    }
}

/// `append_ast_args`: a lambda's parameter list.
fn args(out: &mut String, p: &Params) {
    let mut first = true;
    let n = p.names.len();
    for (i, name) in p.names.iter().enumerate() {
        sep(out, &mut first);
        out.push_str(name);
        // The defaults belong to the trailing parameters.
        if let Some(d) = (i + p.defaults.len())
            .checked_sub(n)
            .map(|di| &p.defaults[di])
        {
            out.push('=');
            expr(out, d, PR_TEST);
        }
        if p.posonly > 0 && i + 1 == p.posonly {
            out.push_str(", /");
        }
    }
    // The `*args`, or a bare `*` opening keyword-only parameters.
    if let Some(star) = &p.star {
        sep(out, &mut first);
        out.push('*');
        out.push_str(star);
    }
    for (name, default) in p.kwonly.iter().zip(&p.kwonly_defaults) {
        sep(out, &mut first);
        out.push_str(name);
        if let Some(d) = default {
            out.push('=');
            expr(out, d, PR_TEST);
        }
    }
    if let Some(kw) = &p.kwargs {
        sep(out, &mut first);
        out.push_str("**");
        out.push_str(kw);
    }
}

/// `append_repr` spells an infinite float or complex part `1e309`, which
/// reads back as the same value.
fn infinite_as_1e309(repr: String) -> String {
    repr.replace("inf", "1e309")
}

/// `build_ftstring_body`: the text inside an f- or t-string's quotes, which
/// the caller then quotes with `repr`. A t-string's top-level interpolation is
/// written as its source text; every other field is unparsed.
fn ftstring_body(parts: &[FStrPart], template: bool) -> String {
    let mut out = String::new();
    for p in parts {
        match p {
            // `append_fstring_unicode`: a literal brace is doubled.
            FStrPart::Lit(s) => out.push_str(&s.replace('{', "{{").replace('}', "}}")),
            FStrPart::Expr {
                expr: value,
                src,
                conv,
                spec,
            } => {
                let text = if template {
                    src.clone()
                } else {
                    // `PR_TEST + 1` parenthesizes a lambda, whose `:` would
                    // otherwise start the format spec.
                    expr_at(value, PR_TEST + 1)
                };
                // A field starting with a brace is set off from the outer one.
                out.push_str(if text.starts_with('{') { "{ " } else { "{" });
                out.push_str(&text);
                if let Some(c) = conv {
                    out.push('!');
                    out.push(*c);
                }
                if !spec.is_empty() {
                    out.push(':');
                    out.push_str(&ftstring_body(spec, false));
                }
                out.push('}');
            }
        }
    }
    out
}

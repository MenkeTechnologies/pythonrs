//! Recursive-descent Python parser: token stream -> AST.
//!
//! Precedence climbs from the ternary `a if b else c` down through boolean,
//! comparison (chained), bitwise, shift, arithmetic, unary, power (right-assoc),
//! and postfix (call/subscript/attribute) to atoms. Suites are the
//! `NEWLINE INDENT ... DEDENT` blocks the lexer delimits, or a one-line simple
//! statement after `:`.

use crate::ast::*;
use crate::lexer::{lex, Tok, Token};

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

/// Parse a full module into a list of statements. Inline `rust { ... }` FFI
/// blocks are desugared to `__rust_compile(...)` calls before lexing.
pub fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    let src = crate::rust_ffi::desugar(src);
    let toks = lex(&src)?;
    let mut p = Parser { toks, pos: 0 };
    p.parse_module()
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

/// Wrap a caret-bearing expression with its source span. `anchor_start ==
/// anchor_end` means no sub-anchor (the whole span renders `^`); otherwise
/// `[anchor_start, anchor_end)` is the operator/bracket region (`^`) and the
/// rest of the span renders `~` — CPython's `~^~` / `~~~^^^` traceback carets.
fn spanned(e: Expr, line: u32, start: u32, end: u32, anchor_start: u32, anchor_end: u32) -> Expr {
    Expr::Spanned(
        Box::new(e),
        Span {
            line,
            start,
            end,
            anchor_start,
            anchor_end,
            suppress: false,
        },
    )
}

impl Parser {
    // ── cursor ────────────────────────────────────────────────────────────
    fn cur(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }
    /// 0-based character column where the current token starts (for carets).
    fn col(&self) -> u32 {
        self.toks[self.pos].col
    }
    /// End column (exclusive) of the current token.
    fn cur_end_col(&self) -> u32 {
        self.toks[self.pos].end_col
    }
    /// End column (exclusive) of the just-consumed token — the end of the
    /// expression whose last token sits at `pos - 1`.
    fn prev_end_col(&self) -> u32 {
        self.toks[self.pos.saturating_sub(1)].end_col
    }
    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn at_op(&self, s: &str) -> bool {
        matches!(self.cur(), Tok::Op(o) if o == s)
    }
    fn eat_op(&mut self, s: &str) -> bool {
        if self.at_op(s) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, s: &str) -> Result<(), String> {
        if self.eat_op(s) {
            Ok(())
        } else {
            Err(format!(
                "SyntaxError: expected '{s}' but found {:?} (line {})",
                self.cur(),
                self.line()
            ))
        }
    }
    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.cur(), Tok::Name(n) if n == kw)
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn at_newline(&self) -> bool {
        matches!(self.cur(), Tok::Newline)
    }
    fn skip_newlines(&mut self) {
        while matches!(self.cur(), Tok::Newline) {
            self.advance();
        }
    }
    fn expect_name(&mut self) -> Result<String, String> {
        match self.cur().clone() {
            Tok::Name(n) if !is_keyword(&n) => {
                self.advance();
                Ok(n)
            }
            other => Err(format!(
                "SyntaxError: expected a name, found {other:?} (line {})",
                self.line()
            )),
        }
    }

    // ── module / suites ───────────────────────────────────────────────────
    fn parse_module(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !matches!(self.cur(), Tok::Eof) {
            self.parse_statement(&mut stmts)?;
            self.skip_newlines();
        }
        Ok(stmts)
    }

    /// A suite after a `:` — either a one-line simple statement or an indented
    /// block.
    /// `desc` names the construct for the missing-block error (CPython's
    /// `expected an indented block after <desc> on line <kw_line>`), e.g. `'if'
    /// statement`, `function definition`, `class definition`. `kw_line` is the
    /// line of the compound-statement keyword, not the (later) blank/dedent line.
    fn parse_suite(&mut self, desc: &str, kw_line: u32) -> Result<Vec<Stmt>, String> {
        self.expect_op(":")?;
        if self.at_newline() {
            self.skip_newlines();
            if !matches!(self.cur(), Tok::Indent) {
                return Err(format!(
                    "IndentationError: expected an indented block after {desc} on line {kw_line}"
                ));
            }
            self.advance(); // Indent
            let mut body = Vec::new();
            while !matches!(self.cur(), Tok::Dedent | Tok::Eof) {
                self.parse_statement(&mut body)?;
                self.skip_newlines();
            }
            if matches!(self.cur(), Tok::Dedent) {
                self.advance();
            }
            Ok(body)
        } else {
            // Simple statement(s) on the same line.
            let mut body = Vec::new();
            self.parse_simple_line(&mut body)?;
            Ok(body)
        }
    }

    /// Dispatch one statement (simple or compound) into `out`.
    fn parse_statement(&mut self, out: &mut Vec<Stmt>) -> Result<(), String> {
        let line = self.line();
        // A block consumes its own `Indent` in `parse_suite`, so an `Indent` at a
        // statement boundary is always stray — CPython's `IndentationError:
        // unexpected indent` (the line lives in the traceback's `File` header).
        if matches!(self.cur(), Tok::Indent) {
            return Err("IndentationError: unexpected indent".to_string());
        }
        if let Tok::Name(n) = self.cur().clone() {
            match n.as_str() {
                "if" => return self.parse_if(out, line),
                "while" => return self.parse_while(out, line),
                "for" => return self.parse_for(out, line, false),
                "def" => return self.parse_funcdef(out, line, Vec::new(), false),
                "class" => return self.parse_classdef(out, line, Vec::new()),
                "try" => return self.parse_try(out, line),
                "with" => return self.parse_with(out, line, false),
                "async" => return self.parse_async(out, line),
                // `match` is a soft keyword: only a match statement when the
                // logical line ends in a `:` NEWLINE INDENT `case` shape.
                "match" if self.looks_like_match() => return self.parse_match(out, line),
                _ => {}
            }
        }
        if self.at_op("@") {
            return self.parse_decorated(out, line);
        }
        // `case NAME … :` at statement start (outside a `match`) is the classic
        // Python 3.10+ mistake; CPython reports `invalid syntax. Did you mean
        // 'class'?`. `case` immediately followed by a bare name is never a valid
        // expression (two adjacent names), and the trailing `:` is the block-header
        // shape CPython keys the `class` suggestion on — so this is unambiguous. A
        // real `case = …` / `case.attr` / `case(…)` (non-name next) is untouched.
        if self.looks_like_stray_case() {
            return Err("SyntaxError: invalid syntax. Did you mean 'class'?".to_string());
        }
        self.parse_simple_line(out)
    }

    /// `case NAME:` at statement start — the misused `case` soft keyword (a bare
    /// capture pattern with the block-header `:`). Kept narrow (a single name then
    /// `:`) so only the unambiguous misuse trips it, matching CPython's `class`
    /// suggestion for `case _:` / `case x:`.
    fn looks_like_stray_case(&self) -> bool {
        matches!(self.cur(), Tok::Name(n) if n == "case")
            && matches!(
                self.toks.get(self.pos + 1).map(|t| &t.tok),
                Some(Tok::Name(_))
            )
            && matches!(
                self.toks.get(self.pos + 2).map(|t| &t.tok),
                Some(Tok::Op(o)) if o == ":"
            )
    }

    /// A logical line of one or more `;`-separated simple statements.
    fn parse_simple_line(&mut self, out: &mut Vec<Stmt>) -> Result<(), String> {
        loop {
            self.parse_simple_stmt(out)?;
            if self.eat_op(";") {
                if self.at_newline() || matches!(self.cur(), Tok::Eof) {
                    break;
                }
                continue;
            }
            break;
        }
        if self.at_newline() {
            self.advance();
        }
        Ok(())
    }

    fn parse_simple_stmt(&mut self, out: &mut Vec<Stmt>) -> Result<(), String> {
        let line = self.line();
        if let Tok::Name(n) = self.cur().clone() {
            match n.as_str() {
                "pass" => {
                    self.advance();
                    out.push(Stmt::new(StmtKind::Pass, line));
                    return Ok(());
                }
                "break" => {
                    self.advance();
                    out.push(Stmt::new(StmtKind::Break, line));
                    return Ok(());
                }
                "continue" => {
                    self.advance();
                    out.push(Stmt::new(StmtKind::Continue, line));
                    return Ok(());
                }
                "return" => {
                    self.advance();
                    let v =
                        if self.at_newline() || self.at_op(";") || matches!(self.cur(), Tok::Eof) {
                            None
                        } else {
                            Some(self.parse_exprlist()?)
                        };
                    out.push(Stmt::new(StmtKind::Return(v), line));
                    return Ok(());
                }
                "raise" => return self.parse_raise(out, line),
                "del" => {
                    self.advance();
                    let targets = self.parse_target_list()?;
                    out.push(Stmt::new(StmtKind::Delete(targets), line));
                    return Ok(());
                }
                "assert" => {
                    self.advance();
                    let test = self.parse_expr()?;
                    let msg = if self.eat_op(",") {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    out.push(Stmt::new(StmtKind::Assert { test, msg }, line));
                    return Ok(());
                }
                "global" => {
                    self.advance();
                    let names = self.parse_name_list()?;
                    out.push(Stmt::new(StmtKind::Global(names), line));
                    return Ok(());
                }
                "nonlocal" => {
                    self.advance();
                    let names = self.parse_name_list()?;
                    out.push(Stmt::new(StmtKind::Nonlocal(names), line));
                    return Ok(());
                }
                "import" => return self.parse_import(out, line),
                "from" => return self.parse_from_import(out, line),
                _ => {}
            }
        }
        self.parse_expr_stmt(out, line)
    }

    fn parse_name_list(&mut self) -> Result<Vec<String>, String> {
        let mut names = vec![self.expect_name()?];
        while self.eat_op(",") {
            names.push(self.expect_name()?);
        }
        Ok(names)
    }

    fn parse_target_list(&mut self) -> Result<Vec<Expr>, String> {
        let mut ts = vec![self.parse_expr()?];
        while self.eat_op(",") {
            if self.at_newline() || matches!(self.cur(), Tok::Eof) {
                break;
            }
            ts.push(self.parse_expr()?);
        }
        Ok(ts)
    }

    fn parse_expr_stmt(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        let first = self.parse_exprlist()?;
        // Annotated assignment: target: ann [= value]
        if self.at_op(":") {
            self.advance();
            let annotation = self.parse_expr()?;
            let value = if self.eat_op("=") {
                Some(self.parse_exprlist()?)
            } else {
                None
            };
            out.push(Stmt::new(
                StmtKind::AnnAssign {
                    target: first,
                    annotation,
                    value,
                },
                line,
            ));
            return Ok(());
        }
        // Augmented assignment.
        if let Tok::Op(o) = self.cur().clone() {
            if let Some(op) = augassign_op(&o) {
                self.advance();
                let value = self.parse_exprlist()?;
                out.push(Stmt::new(
                    StmtKind::AugAssign {
                        target: first,
                        op,
                        value,
                    },
                    line,
                ));
                return Ok(());
            }
        }
        // Plain / chained assignment.
        if self.at_op("=") {
            let mut targets = vec![first];
            let mut value = None;
            while self.eat_op("=") {
                let e = self.parse_exprlist()?;
                if let Some(prev) = value.take() {
                    targets.push(prev);
                }
                value = Some(e);
            }
            out.push(Stmt::new(
                StmtKind::Assign {
                    targets,
                    value: value.unwrap(),
                },
                line,
            ));
            return Ok(());
        }
        out.push(Stmt::new(StmtKind::Expr(first), line));
        Ok(())
    }

    // ── compound statements ───────────────────────────────────────────────
    fn parse_if(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        let kw = if matches!(self.cur(), Tok::Name(n) if n == "elif") {
            "'elif' statement"
        } else {
            "'if' statement"
        };
        self.advance(); // if / elif
        let test = self.parse_namedexpr()?;
        let body = self.parse_suite(kw, line)?;
        let mut orelse = Vec::new();
        self.skip_newlines_shallow();
        if self.at_kw("elif") {
            self.parse_if(&mut orelse, self.line())?;
        } else if self.at_kw("else") {
            let else_line = self.line();
            self.advance();
            orelse = self.parse_suite("'else' statement", else_line)?;
        }
        out.push(Stmt::new(StmtKind::If { test, body, orelse }, line));
        Ok(())
    }

    /// Peek past newlines to see if an `elif`/`else`/`except`/`finally` clause
    /// continues the current compound statement; only consume if so.
    fn skip_newlines_shallow(&mut self) {
        // The lexer already closes suites with Dedent, so a continuation clause
        // sits at the same indent with no leading Newline to skip. Nothing to do.
    }

    fn parse_while(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance();
        let test = self.parse_namedexpr()?;
        let body = self.parse_suite("'while' statement", line)?;
        let orelse = if self.at_kw("else") {
            let el = self.line();
            self.advance();
            self.parse_suite("'else' statement", el)?
        } else {
            Vec::new()
        };
        out.push(Stmt::new(StmtKind::While { test, body, orelse }, line));
        Ok(())
    }

    fn parse_for(&mut self, out: &mut Vec<Stmt>, line: u32, is_async: bool) -> Result<(), String> {
        self.advance();
        let target = self.parse_target_tuple()?;
        if !self.eat_kw("in") {
            return Err(format!(
                "SyntaxError: expected 'in' in for (line {})",
                self.line()
            ));
        }
        let iter = self.parse_exprlist()?;
        let body = self.parse_suite("'for' statement", line)?;
        let orelse = if self.at_kw("else") {
            let el = self.line();
            self.advance();
            self.parse_suite("'else' statement", el)?
        } else {
            Vec::new()
        };
        out.push(Stmt::new(
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                is_async,
            },
            line,
        ));
        Ok(())
    }

    /// A for/with/comprehension target: possibly a tuple of names without parens.
    /// Targets parse at postfix level so a trailing `in` is left for the `for`
    /// clause rather than being consumed as an `in` comparison.
    fn parse_target_tuple(&mut self) -> Result<Expr, String> {
        let first = self.parse_target_atom()?;
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_kw("in") || self.at_op("=") || self.at_op(":") {
                    break;
                }
                items.push(self.parse_target_atom()?);
            }
            Ok(Expr::Tuple(items))
        } else {
            Ok(first)
        }
    }

    /// A single assignment/for target: an optionally-starred postfix expression
    /// (name, attribute, subscript, or a parenthesized/bracketed target list).
    fn parse_target_atom(&mut self) -> Result<Expr, String> {
        if self.eat_op("*") {
            return Ok(Expr::Starred(Box::new(self.parse_await_postfix()?)));
        }
        self.parse_await_postfix()
    }

    fn parse_with(&mut self, out: &mut Vec<Stmt>, line: u32, is_async: bool) -> Result<(), String> {
        self.advance();
        let mut items = Vec::new();
        loop {
            let context = self.parse_expr()?;
            let vars = if self.eat_kw("as") {
                Some(self.parse_ternary()?)
            } else {
                None
            };
            items.push(WithItem { context, vars });
            if !self.eat_op(",") {
                break;
            }
        }
        let body = self.parse_suite("'with' statement", line)?;
        out.push(Stmt::new(
            StmtKind::With {
                items,
                body,
                is_async,
            },
            line,
        ));
        Ok(())
    }

    fn parse_async(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance(); // async
        if self.at_kw("def") {
            return self.parse_funcdef(out, line, Vec::new(), true);
        }
        if self.at_kw("for") {
            return self.parse_for(out, line, true);
        }
        if self.at_kw("with") {
            return self.parse_with(out, line, true);
        }
        Err(format!("SyntaxError: invalid 'async' (line {line})"))
    }

    fn parse_decorated(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        let mut decorators = Vec::new();
        while self.eat_op("@") {
            decorators.push(self.parse_namedexpr()?);
            if self.at_newline() {
                self.advance();
            }
            self.skip_newlines();
        }
        if self.eat_kw("async") {
            return self.parse_funcdef(out, line, decorators, true);
        }
        if self.at_kw("def") {
            self.parse_funcdef(out, line, decorators, false)
        } else if self.at_kw("class") {
            self.parse_classdef(out, line, decorators)
        } else {
            Err(format!(
                "SyntaxError: expected def/class after decorator (line {line})"
            ))
        }
    }

    /// PEP 695: parse an optional type-parameter list after a class/def name
    /// (`class C[T]`, `def f[T, *Ts, **P]`) and return the parameter names.
    /// Bounds/constraints/defaults (`[T: int]`, `[T = str]`) are consumed and
    /// discarded. Type parameters are a static-typing construct — pythonrs
    /// evaluates annotations eagerly, so [`bind_type_params`] binds each name to
    /// `object` in the enclosing scope so an annotation like `-> T` resolves
    /// (the runtime does not depend on which concrete type parameters exist).
    fn parse_type_params(&mut self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        if !self.at_op("[") {
            return Ok(names);
        }
        self.advance(); // [
        let mut depth = 1usize;
        // A parameter name is the first identifier of each comma-separated item,
        // after any `*`/`**` prefix; identifiers inside a bound/default (depth > 1,
        // or after a `:`/`=` in the item) are not parameters.
        let mut expect_name = true;
        loop {
            match self.cur().clone() {
                Tok::Op(o) if o == "[" => {
                    depth += 1;
                    expect_name = false;
                }
                Tok::Op(o) if o == "]" => {
                    depth -= 1;
                    if depth == 0 {
                        self.advance();
                        return Ok(names);
                    }
                }
                Tok::Op(o) if depth == 1 && o == "," => expect_name = true,
                Tok::Op(o) if depth == 1 && (o == "*" || o == "**") => {}
                Tok::Op(_) if depth == 1 => expect_name = false, // `:` / `=`
                Tok::Name(n) if depth == 1 && expect_name => {
                    names.push(n);
                    expect_name = false;
                }
                Tok::Eof => {
                    return Err("SyntaxError: unterminated type-parameter list".to_string())
                }
                _ => {}
            }
            self.advance();
        }
    }

    /// Emit `T = object` bindings for PEP 695 type parameters into `out`, ahead of
    /// the class/def they precede, so eagerly-evaluated annotations that reference
    /// them resolve. See [`parse_type_params`].
    fn bind_type_params(&self, out: &mut Vec<Stmt>, params: &[String], line: u32) {
        for name in params {
            out.push(Stmt::new(
                StmtKind::Assign {
                    targets: vec![Expr::Name(name.clone())],
                    value: Expr::Name("object".to_string()),
                },
                line,
            ));
        }
    }

    fn parse_funcdef(
        &mut self,
        out: &mut Vec<Stmt>,
        line: u32,
        decorators: Vec<Expr>,
        is_async: bool,
    ) -> Result<(), String> {
        self.advance(); // def
        let name = self.expect_name()?;
        let type_params = self.parse_type_params()?; // PEP 695 `def f[T](...)`
        self.bind_type_params(out, &type_params, line);
        self.expect_op("(")?;
        let mut params = self.parse_params(")")?;
        self.expect_op(")")?;
        if self.eat_op("->") {
            let ret = self.parse_expr()?; // return annotation, recorded as `"return"`
            params.annotations.push(("return".to_string(), ret));
        }
        let body = self.parse_suite("function definition", line)?;
        out.push(Stmt::new(
            StmtKind::FuncDef {
                name,
                params,
                body,
                decorators,
                is_async,
            },
            line,
        ));
        Ok(())
    }

    /// Parse a formal-parameter list, stopping at `close` (`)` for def, `:` for
    /// lambda).
    fn parse_params(&mut self, close: &str) -> Result<Params, String> {
        let mut p = Params::default();
        let mut seen_star = false;
        loop {
            if self.at_op(close) {
                break;
            }
            if self.eat_op("/") {
                p.posonly = p.names.len();
                let _ = self.eat_op(",");
                continue;
            }
            if self.eat_op("*") {
                if self.at_op(",") || self.at_op(close) {
                    p.star = Some(String::new()); // bare `*`
                } else {
                    let star_name = self.expect_name()?;
                    if close == ")" && self.at_op(":") {
                        self.advance();
                        let ann = self.parse_expr()?;
                        p.annotations.push((star_name.clone(), ann));
                    }
                    p.star = Some(star_name);
                }
                seen_star = true;
                let _ = self.eat_op(",");
                continue;
            }
            if self.eat_op("**") {
                let kw_name = self.expect_name()?;
                if close == ")" && self.at_op(":") {
                    self.advance();
                    let ann = self.parse_expr()?;
                    p.annotations.push((kw_name.clone(), ann));
                }
                p.kwargs = Some(kw_name);
                let _ = self.eat_op(",");
                continue;
            }
            let name = self.expect_name()?;
            // `name: annotation` — recorded for `__annotations__` (only in a
            // `def`, `close == ")"`; a `lambda` has no annotations).
            if close == ")" && self.eat_op(":") {
                let ann = self.parse_expr()?;
                p.annotations.push((name.clone(), ann));
            }
            let default = if self.eat_op("=") {
                Some(self.parse_expr()?)
            } else {
                None
            };
            if seen_star {
                p.kwonly.push(name);
                p.kwonly_defaults.push(default);
            } else {
                p.names.push(name);
                if let Some(d) = default {
                    p.defaults.push(d);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(p)
    }

    fn parse_classdef(
        &mut self,
        out: &mut Vec<Stmt>,
        line: u32,
        decorators: Vec<Expr>,
    ) -> Result<(), String> {
        self.advance(); // class
        let name = self.expect_name()?;
        let type_params = self.parse_type_params()?; // PEP 695 `class C[T](...)`
        self.bind_type_params(out, &type_params, line);
        let mut bases = Vec::new();
        let mut keywords = Vec::new();
        if self.eat_op("(") {
            while !self.at_op(")") {
                if self.eat_op("**") {
                    keywords.push(Keyword {
                        name: None,
                        value: self.parse_expr()?,
                    });
                } else if matches!(self.cur(), Tok::Name(n) if !is_keyword(n))
                    && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == "=")
                {
                    let kn = self.expect_name()?;
                    self.expect_op("=")?;
                    keywords.push(Keyword {
                        name: Some(kn),
                        value: self.parse_expr()?,
                    });
                } else {
                    bases.push(self.parse_expr()?);
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let body = self.parse_suite("class definition", line)?;
        out.push(Stmt::new(
            StmtKind::ClassDef {
                name,
                bases,
                keywords,
                body,
                decorators,
            },
            line,
        ));
        Ok(())
    }

    fn parse_try(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance();
        let body = self.parse_suite("'try' statement", line)?;
        let mut handlers = Vec::new();
        while self.at_kw("except") {
            let except_line = self.line();
            self.advance();
            let star = self.eat_op("*");
            let (typ, name) = if self.at_op(":") {
                (None, None)
            } else {
                let t = self.parse_expr()?;
                let n = if self.eat_kw("as") {
                    Some(self.expect_name()?)
                } else {
                    None
                };
                (Some(t), n)
            };
            let hbody = self.parse_suite("'except' statement", except_line)?;
            handlers.push(ExceptHandler {
                typ,
                name,
                body: hbody,
                star,
            });
        }
        let orelse = if self.at_kw("else") {
            let el = self.line();
            self.advance();
            self.parse_suite("'else' statement", el)?
        } else {
            Vec::new()
        };
        let finalbody = if self.at_kw("finally") {
            let fl = self.line();
            self.advance();
            self.parse_suite("'finally' statement", fl)?
        } else {
            Vec::new()
        };
        out.push(Stmt::new(
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            },
            line,
        ));
        Ok(())
    }

    // ── match / case (PEP 634) ────────────────────────────────────────────
    /// Disambiguate the soft keyword `match`: it starts a match statement only
    /// when the logical line has a top-level `:` immediately followed by
    /// `NEWLINE INDENT case`. Otherwise `match` is an ordinary identifier.
    fn looks_like_match(&self) -> bool {
        let mut i = self.pos + 1;
        // Must be followed by something that can begin the subject expression.
        match self.toks.get(i).map(|t| &t.tok) {
            Some(Tok::Op(o)) if o == "=" || o == ":" || o == "." || o == ";" || o == "," => {
                return false
            }
            Some(Tok::Newline) | Some(Tok::Eof) | None => return false,
            _ => {}
        }
        let mut depth = 0i32;
        while let Some(t) = self.toks.get(i) {
            match &t.tok {
                Tok::Op(o) if o == "(" || o == "[" || o == "{" => depth += 1,
                Tok::Op(o) if o == ")" || o == "]" || o == "}" => depth -= 1,
                Tok::Op(o) if o == ":" && depth == 0 => {
                    let a1 = self.toks.get(i + 1).map(|t| &t.tok);
                    let a2 = self.toks.get(i + 2).map(|t| &t.tok);
                    // A real match block is `: NEWLINE INDENT case`. A bare
                    // `match SUBJECT:` with no block (end of input, or a NEWLINE
                    // not followed by INDENT) is still a match header — CPython
                    // reports the missing block, so `parse_match` must own it.
                    let real_block = matches!(a1, Some(Tok::Newline))
                        && matches!(a2, Some(Tok::Indent))
                        && matches!(self.toks.get(i + 3).map(|t| &t.tok), Some(Tok::Name(n)) if n == "case");
                    let bare_header = matches!(a1, Some(Tok::Eof))
                        || (matches!(a1, Some(Tok::Newline)) && !matches!(a2, Some(Tok::Indent)));
                    return real_block || bare_header;
                }
                Tok::Newline | Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_match(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance(); // match
        let subject = self.parse_exprlist()?;
        self.expect_op(":")?;
        self.skip_newlines();
        if !matches!(self.cur(), Tok::Indent) {
            return Err(format!(
                "IndentationError: expected an indented block after 'match' statement on line {line}"
            ));
        }
        self.advance(); // Indent
        let mut cases = Vec::new();
        while self.at_kw("case") {
            let case_line = self.line();
            self.advance(); // case
            let pattern = self.parse_patterns()?;
            let guard = if self.eat_kw("if") {
                Some(self.parse_namedexpr()?)
            } else {
                None
            };
            let body = self.parse_suite("'case' statement", case_line)?;
            self.skip_newlines();
            cases.push(MatchCase {
                pattern,
                guard,
                body,
            });
        }
        if matches!(self.cur(), Tok::Dedent) {
            self.advance();
        }
        out.push(Stmt::new(StmtKind::Match { subject, cases }, line));
        Ok(())
    }

    /// Top-level pattern for a `case`: an open sequence (`case 1, 2`) or a single
    /// OR-pattern.
    fn parse_patterns(&mut self) -> Result<Pattern, String> {
        let first = self.parse_pattern()?;
        if self.at_op(",") {
            let mut elems = vec![first];
            while self.eat_op(",") {
                if self.at_op(":") || self.at_kw("if") {
                    break;
                }
                elems.push(self.parse_pattern()?);
            }
            let star = elems.iter().position(|p| matches!(p, Pattern::Star(_)));
            Ok(Pattern::Sequence { elems, star })
        } else {
            Ok(first)
        }
    }

    /// A full pattern (PEP 634 `pattern`): an OR-pattern optionally followed by
    /// `as name`. `as` binds looser than `|`, so `1 | 2 as x` is `(1 | 2) as x`.
    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        let p = self.parse_or_pattern()?;
        if self.eat_kw("as") {
            let name = self.expect_name()?;
            Ok(Pattern::As(Box::new(p), name))
        } else {
            Ok(p)
        }
    }

    fn parse_or_pattern(&mut self) -> Result<Pattern, String> {
        let first = self.parse_closed_pattern()?;
        if self.at_op("|") {
            let mut alts = vec![first];
            while self.eat_op("|") {
                alts.push(self.parse_closed_pattern()?);
            }
            Ok(Pattern::Or(alts))
        } else {
            Ok(first)
        }
    }

    fn parse_closed_pattern(&mut self) -> Result<Pattern, String> {
        // `*name` / `*_` (only valid inside a sequence, checked structurally).
        if self.eat_op("*") {
            let name = if self.at_kw("_") {
                self.advance();
                None
            } else {
                Some(self.expect_name()?)
            };
            return Ok(Pattern::Star(name));
        }
        // Bracketed / parenthesized sequence pattern.
        if self.eat_op("[") {
            return self.parse_sequence_pattern("]");
        }
        if self.at_op("(") {
            self.advance();
            // A single parenthesized pattern is a group; commas make a sequence.
            if self.eat_op(")") {
                return Ok(Pattern::Sequence {
                    elems: vec![],
                    star: None,
                });
            }
            let first = self.parse_pattern()?;
            if self.at_op(",") {
                let mut elems = vec![first];
                while self.eat_op(",") {
                    if self.at_op(")") {
                        break;
                    }
                    elems.push(self.parse_pattern()?);
                }
                self.expect_op(")")?;
                let star = elems.iter().position(|p| matches!(p, Pattern::Star(_)));
                return Ok(Pattern::Sequence { elems, star });
            }
            self.expect_op(")")?;
            return Ok(first);
        }
        // Mapping pattern.
        if self.at_op("{") {
            return self.parse_mapping_pattern();
        }
        // Literal patterns.
        if let Some(p) = self.try_literal_pattern()? {
            return Ok(p);
        }
        // Name-based: capture, wildcard, dotted value, or class pattern.
        let name = self.expect_name()?;
        if name == "_" {
            return Ok(Pattern::Wildcard);
        }
        // Build a (possibly dotted) value expression.
        let mut expr = Expr::Name(name);
        let mut dotted = false;
        while self.eat_op(".") {
            let attr = self.expect_name()?;
            expr = Expr::Attribute(Box::new(expr), attr);
            dotted = true;
        }
        if self.at_op("(") {
            return self.parse_class_pattern(expr);
        }
        if dotted {
            Ok(Pattern::Value(expr))
        } else {
            match expr {
                Expr::Name(n) => Ok(Pattern::Capture(n)),
                _ => Ok(Pattern::Value(expr)),
            }
        }
    }

    fn try_literal_pattern(&mut self) -> Result<Option<Pattern>, String> {
        // Signed numbers, strings, True/False/None.
        if self.at_op("-") {
            self.advance();
            let e = self.parse_atom()?;
            return Ok(Some(Pattern::Value(Expr::UnaryOp(UnOp::Neg, Box::new(e)))));
        }
        let lit = match self.cur().clone() {
            Tok::Int(_)
            | Tok::BigInt(_)
            | Tok::Float(_)
            | Tok::Complex(_)
            | Tok::Str(_)
            | Tok::FString(_, _)
            | Tok::Bytes(_) => Some(self.parse_atom()?),
            Tok::Name(n) if n == "None" || n == "True" || n == "False" => Some(self.parse_atom()?),
            _ => None,
        };
        Ok(lit.map(Pattern::Value))
    }

    fn parse_sequence_pattern(&mut self, close: &str) -> Result<Pattern, String> {
        let mut elems = Vec::new();
        while !self.at_op(close) {
            elems.push(self.parse_pattern()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(close)?;
        let star = elems.iter().position(|p| matches!(p, Pattern::Star(_)));
        Ok(Pattern::Sequence { elems, star })
    }

    fn parse_mapping_pattern(&mut self) -> Result<Pattern, String> {
        self.advance(); // {
        let mut keys = Vec::new();
        let mut rest = None;
        while !self.at_op("}") {
            if self.eat_op("**") {
                rest = Some(self.expect_name()?);
                let _ = self.eat_op(",");
                break;
            }
            // key is a literal or dotted value expression.
            let key = self.parse_or()?;
            self.expect_op(":")?;
            let pat = self.parse_pattern()?;
            keys.push((key, pat));
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op("}")?;
        Ok(Pattern::Mapping { keys, rest })
    }

    fn parse_class_pattern(&mut self, cls: Expr) -> Result<Pattern, String> {
        self.expect_op("(")?;
        let mut pos = Vec::new();
        let mut kw = Vec::new();
        while !self.at_op(")") {
            // keyword sub-pattern: name=pattern
            if matches!(self.cur(), Tok::Name(n) if !is_keyword(n))
                && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == "=")
            {
                let kn = self.expect_name()?;
                self.expect_op("=")?;
                kw.push((kn, self.parse_pattern()?));
            } else {
                pos.push(self.parse_pattern()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Pattern::Class { cls, pos, kw })
    }

    fn parse_raise(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance();
        let (exc, cause) = if self.at_newline() || self.at_op(";") || matches!(self.cur(), Tok::Eof)
        {
            (None, None)
        } else {
            let e = self.parse_expr()?;
            let c = if self.eat_kw("from") {
                Some(self.parse_expr()?)
            } else {
                None
            };
            (Some(e), c)
        };
        out.push(Stmt::new(StmtKind::Raise { exc, cause }, line));
        Ok(())
    }

    fn parse_import(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance();
        let mut names = Vec::new();
        loop {
            let mut name = self.expect_name()?;
            while self.eat_op(".") {
                name.push('.');
                name.push_str(&self.expect_name()?);
            }
            let asname = if self.eat_kw("as") {
                Some(self.expect_name()?)
            } else {
                None
            };
            names.push(Alias { name, asname });
            if !self.eat_op(",") {
                break;
            }
        }
        out.push(Stmt::new(StmtKind::Import(names), line));
        Ok(())
    }

    fn parse_from_import(&mut self, out: &mut Vec<Stmt>, line: u32) -> Result<(), String> {
        self.advance(); // from
        let mut level = 0;
        while self.at_op(".") || self.at_op("...") {
            level += if self.at_op("...") { 3 } else { 1 };
            self.advance();
        }
        let module = if self.at_kw("import") {
            None
        } else {
            let mut m = self.expect_name()?;
            while self.eat_op(".") {
                m.push('.');
                m.push_str(&self.expect_name()?);
            }
            Some(m)
        };
        if !self.eat_kw("import") {
            return Err(format!(
                "SyntaxError: expected 'import' (line {})",
                self.line()
            ));
        }
        let mut names = Vec::new();
        if self.eat_op("*") {
            names.push(Alias {
                name: "*".into(),
                asname: None,
            });
        } else {
            let paren = self.eat_op("(");
            loop {
                let name = self.expect_name()?;
                let asname = if self.eat_kw("as") {
                    Some(self.expect_name()?)
                } else {
                    None
                };
                names.push(Alias { name, asname });
                if !self.eat_op(",") {
                    break;
                }
                if paren && self.at_op(")") {
                    break;
                }
            }
            if paren {
                self.expect_op(")")?;
            }
        }
        out.push(Stmt::new(
            StmtKind::ImportFrom {
                module,
                names,
                level,
            },
            line,
        ));
        Ok(())
    }

    // ── expressions ───────────────────────────────────────────────────────

    /// Top-level expression list: builds a Tuple on a trailing/interior comma.
    fn parse_exprlist(&mut self) -> Result<Expr, String> {
        let first = self.parse_star_or_expr()?;
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.stop_exprlist() {
                    break;
                }
                items.push(self.parse_star_or_expr()?);
            }
            Ok(Expr::Tuple(items))
        } else {
            Ok(first)
        }
    }

    fn stop_exprlist(&self) -> bool {
        self.at_newline()
            || matches!(self.cur(), Tok::Eof)
            || self.at_op("=")
            || self.at_op(";")
            || self.at_op(":")
            || self.at_op(")")
            || self.at_op("]")
            || self.at_op("}")
    }

    fn parse_star_or_expr(&mut self) -> Result<Expr, String> {
        if self.eat_op("*") {
            return Ok(Expr::Starred(Box::new(self.parse_expr()?)));
        }
        self.parse_namedexpr()
    }

    /// `namedexpr_test`: test [`:=` test].
    fn parse_namedexpr(&mut self) -> Result<Expr, String> {
        let e = self.parse_ternary()?;
        if self.at_op(":=") {
            self.advance();
            let v = self.parse_ternary()?;
            return Ok(Expr::NamedExpr(Box::new(e), Box::new(v)));
        }
        Ok(e)
    }

    /// alias used where a single (non-tuple) expression is wanted.
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_namedexpr()
    }

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        if self.at_kw("lambda") {
            return self.parse_lambda();
        }
        let body = self.parse_or()?;
        if self.at_kw("if") {
            self.advance();
            let test = self.parse_or()?;
            if !self.eat_kw("else") {
                return Err(format!(
                    "SyntaxError: ternary missing else (line {})",
                    self.line()
                ));
            }
            let orelse = self.parse_ternary()?;
            return Ok(Expr::IfExp {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            });
        }
        Ok(body)
    }

    fn parse_lambda(&mut self) -> Result<Expr, String> {
        self.advance(); // lambda
        let params = self.parse_params(":")?;
        self.expect_op(":")?;
        let body = self.parse_ternary()?;
        Ok(Expr::Lambda {
            params,
            body: Box::new(body),
        })
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_and()?;
        if self.at_kw("or") {
            let mut items = vec![e];
            while self.eat_kw("or") {
                items.push(self.parse_and()?);
            }
            e = Expr::BoolOp(BoolOp::Or, items);
        }
        Ok(e)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_not()?;
        if self.at_kw("and") {
            let mut items = vec![e];
            while self.eat_kw("and") {
                items.push(self.parse_not()?);
            }
            e = Expr::BoolOp(BoolOp::And, items);
        }
        Ok(e)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.eat_kw("not") {
            let e = self.parse_not()?;
            return Ok(Expr::UnaryOp(UnOp::Not, Box::new(e)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let left = self.parse_bitor()?;
        let mut ops = Vec::new();
        loop {
            let op = if self.at_op("<") {
                CmpOp::Lt
            } else if self.at_op(">") {
                CmpOp::Gt
            } else if self.at_op("<=") {
                CmpOp::Le
            } else if self.at_op(">=") {
                CmpOp::Ge
            } else if self.at_op("==") {
                CmpOp::Eq
            } else if self.at_op("!=") {
                CmpOp::Ne
            } else if self.at_kw("in") {
                CmpOp::In
            } else if self.at_kw("is") {
                self.advance();
                if self.eat_kw("not") {
                    ops.push((CmpOp::IsNot, self.parse_bitor()?));
                } else {
                    ops.push((CmpOp::Is, self.parse_bitor()?));
                }
                continue;
            } else if self.at_kw("not") {
                // `not in`
                self.advance();
                if self.eat_kw("in") {
                    ops.push((CmpOp::NotIn, self.parse_bitor()?));
                    continue;
                } else {
                    return Err(format!(
                        "SyntaxError: expected 'in' after 'not' (line {})",
                        self.line()
                    ));
                }
            } else {
                break;
            };
            self.advance();
            ops.push((op, self.parse_bitor()?));
        }
        if ops.is_empty() {
            Ok(left)
        } else {
            // CPython underlines a comparison with a plain `^` span (no `~^~`
            // operator anchor), so a bare `a < b` that spans the whole line is
            // hidden and `x = a < b` shows `^^^^^`.
            let end = self.prev_end_col();
            Ok(spanned(Expr::Compare(Box::new(left), ops), sl, sc, end, 0, 0))
        }
    }

    fn parse_bitor(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_bitxor()?;
        while self.at_op("|") {
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(BinOp::BitOr, Box::new(e), Box::new(self.parse_bitxor()?));
            e = spanned(e2, sl, sc, self.prev_end_col(), opc, ope);
        }
        Ok(e)
    }
    fn parse_bitxor(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_bitand()?;
        while self.at_op("^") {
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(BinOp::BitXor, Box::new(e), Box::new(self.parse_bitand()?));
            e = spanned(e2, sl, sc, self.prev_end_col(), opc, ope);
        }
        Ok(e)
    }
    fn parse_bitand(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_shift()?;
        while self.at_op("&") {
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(BinOp::BitAnd, Box::new(e), Box::new(self.parse_shift()?));
            e = spanned(e2, sl, sc, self.prev_end_col(), opc, ope);
        }
        Ok(e)
    }
    fn parse_shift(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_arith()?;
        loop {
            let op = if self.at_op("<<") {
                BinOp::Shl
            } else if self.at_op(">>") {
                BinOp::Shr
            } else {
                break;
            };
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(op, Box::new(e), Box::new(self.parse_arith()?));
            e = spanned(e2, sl, sc, self.prev_end_col(), opc, ope);
        }
        Ok(e)
    }
    fn parse_arith(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_term()?;
        loop {
            let op = if self.at_op("+") {
                BinOp::Add
            } else if self.at_op("-") {
                BinOp::Sub
            } else {
                break;
            };
            let ( opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(op, Box::new(e), Box::new(self.parse_term()?));
            let end = self.prev_end_col();
            e = spanned(e2, sl, sc, end, opc, ope);
        }
        Ok(e)
    }
    fn parse_term(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_unary()?;
        loop {
            let op = if self.at_op("*") {
                BinOp::Mul
            } else if self.at_op("/") {
                BinOp::Div
            } else if self.at_op("//") {
                BinOp::FloorDiv
            } else if self.at_op("%") {
                BinOp::Mod
            } else if self.at_op("@") {
                BinOp::MatMul
            } else {
                break;
            };
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let e2 = Expr::BinOp(op, Box::new(e), Box::new(self.parse_unary()?));
            let end = self.prev_end_col();
            e = spanned(e2, sl, sc, end, opc, ope);
        }
        Ok(e)
    }
    fn parse_unary(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let unary = |p: &mut Self, op: UnOp| -> Result<Expr, String> {
            p.advance();
            let operand = p.parse_unary()?;
            let end = p.prev_end_col();
            Ok(spanned(
                Expr::UnaryOp(op, Box::new(operand)),
                sl,
                sc,
                end,
                0,
                0,
            ))
        };
        if self.at_op("-") {
            return unary(self, UnOp::Neg);
        }
        if self.at_op("+") {
            return unary(self, UnOp::Pos);
        }
        if self.at_op("~") {
            return unary(self, UnOp::Invert);
        }
        self.parse_power()
    }
    fn parse_power(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let base = self.parse_await_postfix()?;
        if self.at_op("**") {
            let (opc, ope) = (self.col(), self.cur_end_col());
            self.advance();
            let exp = self.parse_unary()?; // right-assoc, binds unary on the right
            let end = self.prev_end_col();
            return Ok(spanned(
                Expr::BinOp(BinOp::Pow, Box::new(base), Box::new(exp)),
                sl,
                sc,
                end,
                opc,
                ope,
            ));
        }
        Ok(base)
    }

    fn parse_await_postfix(&mut self) -> Result<Expr, String> {
        if self.eat_kw("await") {
            let e = self.parse_await_postfix()?;
            return Ok(Expr::Await(Box::new(e)));
        }
        // Span of the whole postfix chain starts at the value's first token; each
        // trailer wraps its result so a call/subscript/attribute that raises
        // underlines from here to its closing bracket / attribute name.
        let (start_line, start_col) = (self.line(), self.col());
        let mut e = self.parse_atom()?;
        loop {
            if self.at_op("(") {
                // Anchor the call's `(...)` bracket region for the `~~~^^^` caret.
                let paren_col = self.col();
                e = self.parse_call(e)?;
                let end = self.prev_end_col();
                e = spanned(e, start_line, start_col, end, paren_col, end);
            } else if self.at_op("[") {
                let bracket_col = self.col();
                self.advance();
                let sub = self.parse_subscript()?;
                self.expect_op("]")?;
                let end = self.prev_end_col();
                e = spanned(
                    Expr::Subscript(Box::new(e), Box::new(sub)),
                    start_line,
                    start_col,
                    end,
                    bracket_col,
                    end,
                );
            } else if self.eat_op(".") {
                let attr = self.expect_name()?;
                let end = self.prev_end_col();
                e = spanned(
                    Expr::Attribute(Box::new(e), attr),
                    start_line,
                    start_col,
                    end,
                    0,
                    0,
                );
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_call(&mut self, func: Expr) -> Result<Expr, String> {
        self.expect_op("(")?;
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        while !self.at_op(")") {
            if self.eat_op("*") {
                args.push(Expr::Starred(Box::new(self.parse_expr()?)));
            } else if self.eat_op("**") {
                keywords.push(Keyword {
                    name: None,
                    value: self.parse_expr()?,
                });
            } else if matches!(self.cur(), Tok::Name(n) if !is_keyword(n))
                && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == "=")
            {
                let kn = self.expect_name()?;
                self.expect_op("=")?;
                keywords.push(Keyword {
                    name: Some(kn),
                    value: self.parse_expr()?,
                });
            } else {
                let e = self.parse_namedexpr()?;
                // Generator expression as sole argument: f(x for x in xs)
                if self.at_comp_for() && args.is_empty() && keywords.is_empty() {
                    let comps = self.parse_comprehension_clauses()?;
                    args.push(Expr::GenExp(Box::new(e), comps));
                } else {
                    args.push(e);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Expr::Call {
            func: Box::new(func),
            args,
            keywords,
        })
    }

    fn parse_subscript(&mut self) -> Result<Expr, String> {
        // A subscript may be a slice, an index, or a tuple of these.
        let parse_one = |p: &mut Self| -> Result<Expr, String> {
            let lo = if p.at_op(":") {
                None
            } else {
                Some(Box::new(p.parse_expr()?))
            };
            if p.at_op(":") {
                p.advance();
                let hi = if p.at_op(":") || p.at_op("]") || p.at_op(",") {
                    None
                } else {
                    Some(Box::new(p.parse_expr()?))
                };
                let step = if p.eat_op(":") {
                    if p.at_op("]") || p.at_op(",") {
                        None
                    } else {
                        Some(Box::new(p.parse_expr()?))
                    }
                } else {
                    None
                };
                Ok(Expr::Slice { lo, hi, step })
            } else {
                Ok(*lo.unwrap())
            }
        };
        let first = parse_one(self)?;
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op("]") {
                    break;
                }
                items.push(parse_one(self)?);
            }
            Ok(Expr::Tuple(items))
        } else {
            Ok(first)
        }
    }

    // ── atoms ─────────────────────────────────────────────────────────────
    fn parse_atom(&mut self) -> Result<Expr, String> {
        let line = self.line();
        match self.cur().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Int(n))
            }
            Tok::BigInt(s) => {
                self.advance();
                Ok(Expr::BigInt(s))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f))
            }
            Tok::Complex(f) => {
                self.advance();
                Ok(Expr::Complex(f))
            }
            Tok::Str(_) | Tok::FString(_, _) | Tok::Bytes(_) => self.parse_string_group(),
            Tok::Name(n) => {
                let (nl, nc, ne) = (self.line(), self.col(), self.cur_end_col());
                self.advance();
                match n.as_str() {
                    "None" => Ok(Expr::None),
                    "True" => Ok(Expr::True),
                    "False" => Ok(Expr::False),
                    "lambda" => {
                        self.pos -= 1;
                        self.parse_lambda()
                    }
                    "yield" => {
                        if self.eat_kw("from") {
                            Ok(Expr::YieldFrom(Box::new(self.parse_expr()?)))
                        } else if self.at_newline()
                            || self.at_op(")")
                            || matches!(self.cur(), Tok::Eof)
                        {
                            Ok(Expr::Yield(None))
                        } else {
                            Ok(Expr::Yield(Some(Box::new(self.parse_exprlist()?))))
                        }
                    }
                    _ if is_keyword(&n) => Err(format!(
                        "SyntaxError: unexpected keyword '{n}' (line {line})"
                    )),
                    // A bare name load carries its span so an undefined-name
                    // traceback underlines exactly the name.
                    _ => Ok(spanned(Expr::Name(n), nl, nc, ne, 0, 0)),
                }
            }
            Tok::Op(o) => match o.as_str() {
                "(" => self.parse_paren(),
                "[" => self.parse_list(),
                "{" => self.parse_brace(),
                "..." => {
                    self.advance();
                    Ok(Expr::Ellipsis)
                }
                // An operator where an atom was expected — CPython's catch-all
                // `invalid syntax` (the token/line live in the traceback header).
                _ => Err("SyntaxError: invalid syntax".to_string()),
            },
            // Any other token (Newline, Op, keyword) where an atom was expected.
            _ => {
                let _ = line;
                Err("SyntaxError: invalid syntax".to_string())
            }
        }
    }

    /// Adjacent string literals concatenate (`"a" "b"` -> `"ab"`).
    fn parse_string_group(&mut self) -> Result<Expr, String> {
        let mut parts: Vec<FStrPart> = Vec::new();
        let mut any_f = false;
        let mut byte_acc: Option<Vec<u8>> = None;
        loop {
            match self.cur().clone() {
                Tok::Str(s) => {
                    self.advance();
                    parts.push(FStrPart::Lit(s));
                }
                Tok::Bytes(b) => {
                    self.advance();
                    byte_acc.get_or_insert_with(Vec::new).extend(b);
                }
                Tok::FString(raw, is_raw) => {
                    self.advance();
                    any_f = true;
                    let mut sub = self.parse_fstring(&raw, is_raw)?;
                    parts.append(&mut sub);
                }
                _ => break,
            }
        }
        if let Some(b) = byte_acc {
            return Ok(Expr::Bytes(b));
        }
        if any_f {
            Ok(Expr::FString(parts))
        } else {
            let mut s = String::new();
            for p in parts {
                if let FStrPart::Lit(l) = p {
                    s.push_str(&l);
                }
            }
            Ok(Expr::Str(s))
        }
    }

    /// Expand an f-string body into literal/expression parts.
    fn parse_fstring(&self, raw: &str, is_raw: bool) -> Result<Vec<FStrPart>, String> {
        let chars: Vec<char> = raw.chars().collect();
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '{' {
                if chars.get(i + 1) == Some(&'{') {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                // `\N{NAME}` named-Unicode escape: the braces belong to the escape,
                // not a replacement field. Absorb `{...}` into the literal so
                // `decode_escapes` resolves the name.
                if crate::lexer::ends_with_named_escape_lead(&lit, is_raw) {
                    lit.push('{');
                    i += 1;
                    while i < chars.len() && chars[i] != '}' {
                        lit.push(chars[i]);
                        i += 1;
                    }
                    if i < chars.len() {
                        lit.push('}');
                        i += 1;
                    }
                    continue;
                }
                if !lit.is_empty() {
                    let decoded = crate::lexer::decode_escapes(&lit, is_raw)?;
                    parts.push(FStrPart::Lit(decoded));
                    lit.clear();
                }
                // Collect balanced field text up to the matching `}`.
                let mut depth = 1;
                i += 1;
                let mut field = String::new();
                while i < chars.len() && depth > 0 {
                    match chars[i] {
                        '{' => {
                            depth += 1;
                            field.push('{');
                        }
                        '}' => {
                            depth -= 1;
                            if depth > 0 {
                                field.push('}');
                            }
                        }
                        other => field.push(other),
                    }
                    i += 1;
                }
                parts.extend(self.build_fstring_field(&field, is_raw)?);
            } else if c == '}' {
                if chars.get(i + 1) == Some(&'}') {
                    lit.push('}');
                    i += 2;
                    continue;
                }
                lit.push('}');
                i += 1;
            } else {
                lit.push(c);
                i += 1;
            }
        }
        if !lit.is_empty() {
            let decoded = crate::lexer::decode_escapes(&lit, is_raw)?;
            parts.push(FStrPart::Lit(decoded));
        }
        Ok(parts)
    }

    fn build_fstring_field(&self, field: &str, is_raw: bool) -> Result<Vec<FStrPart>, String> {
        // `{expr=}` debug form (PEP 501): the source text up to and including the
        // top-level `=` (plus following whitespace) is emitted literally, then
        // the value. A trailing `!conv`/`:spec` still applies; with neither, the
        // value uses `repr`. Reconstruct "expr[!conv][:spec]" without the `=`
        // (and leading whitespace) so the shared conv/spec split below applies.
        let mut debug_prefix: Option<String> = None;
        let work: String = if let Some(eq) = find_debug_eq(field) {
            let after = &field[eq + 1..];
            let ws = after.len() - after.trim_start().len();
            debug_prefix = Some(field[..eq + 1 + ws].to_string());
            format!("{}{}", &field[..eq], after.trim_start())
        } else {
            field.to_string()
        };
        let field: &str = &work;

        // Split off !conv and :spec (top level only).
        let mut expr_src = field;
        let mut spec: Vec<FStrPart> = Vec::new();
        let mut conv: Option<char> = None;
        // format spec — itself a mini joined-string, so a nested replacement field
        // (`{w}` in `{x:{w}.2f}`) is evaluated at runtime and spliced into the spec.
        if let Some(idx) = find_top_level(field, ':') {
            spec = self.parse_fstring(&field[idx + 1..], is_raw)?;
            expr_src = &field[..idx];
        }
        // conversion !s/!r/!a
        if expr_src.len() >= 2 {
            let bytes = expr_src.as_bytes();
            if bytes[expr_src.len() - 2] == b'!' {
                let c = bytes[expr_src.len() - 1] as char;
                if matches!(c, 's' | 'r' | 'a') {
                    conv = Some(c);
                    expr_src = &expr_src[..expr_src.len() - 2];
                }
            }
        }
        // A debug field with neither conversion nor format spec defaults to repr.
        if debug_prefix.is_some() && conv.is_none() && spec.is_empty() {
            conv = Some('r');
        }
        let expr_src = expr_src.trim();
        let sub = parse(&format!("({expr_src})")).map_err(|e| format!("f-string: {e}"))?;
        let expr = match sub.into_iter().next() {
            Some(Stmt {
                kind: StmtKind::Expr(e),
                ..
            }) => e,
            _ => return Err(format!("f-string: invalid expression {{{expr_src}}}")),
        };
        let mut out = Vec::with_capacity(2);
        if let Some(pre) = debug_prefix {
            out.push(FStrPart::Lit(pre));
        }
        out.push(FStrPart::Expr {
            expr: Box::new(expr),
            conv,
            spec,
        });
        Ok(out)
    }

    /// `(...)` — parenthesized expr, tuple, or generator expression.
    fn parse_paren(&mut self) -> Result<Expr, String> {
        self.advance(); // (
        if self.eat_op(")") {
            return Ok(Expr::Tuple(Vec::new()));
        }
        let first = self.parse_star_or_expr()?;
        if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op(")")?;
            return Ok(Expr::GenExp(Box::new(first), comps));
        }
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op(")") {
                    break;
                }
                items.push(self.parse_star_or_expr()?);
            }
            self.expect_op(")")?;
            return Ok(Expr::Tuple(items));
        }
        self.expect_op(")")?;
        Ok(first)
    }

    /// `[...]` — list display or list comprehension.
    fn parse_list(&mut self) -> Result<Expr, String> {
        self.advance(); // [
        if self.eat_op("]") {
            return Ok(Expr::List(Vec::new()));
        }
        let first = self.parse_star_or_expr()?;
        if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op("]")?;
            return Ok(Expr::ListComp(Box::new(first), comps));
        }
        let mut items = vec![first];
        while self.eat_op(",") {
            if self.at_op("]") {
                break;
            }
            items.push(self.parse_star_or_expr()?);
        }
        self.expect_op("]")?;
        Ok(Expr::List(items))
    }

    /// `{...}` — dict/set display or comprehension.
    fn parse_brace(&mut self) -> Result<Expr, String> {
        self.advance(); // {
        if self.eat_op("}") {
            return Ok(Expr::Dict(Vec::new()));
        }
        // `**mapping` spread implies dict.
        if self.eat_op("**") {
            let v = self.parse_expr()?;
            let mut pairs = vec![(None, v)];
            while self.eat_op(",") {
                if self.at_op("}") {
                    break;
                }
                if self.eat_op("**") {
                    pairs.push((None, self.parse_expr()?));
                } else {
                    let k = self.parse_expr()?;
                    self.expect_op(":")?;
                    pairs.push((Some(k), self.parse_expr()?));
                }
            }
            self.expect_op("}")?;
            return Ok(Expr::Dict(pairs));
        }
        let first = self.parse_star_or_expr()?;
        if self.at_op(":") {
            // dict
            self.advance();
            let v = self.parse_expr()?;
            if self.at_comp_for() {
                let comps = self.parse_comprehension_clauses()?;
                self.expect_op("}")?;
                return Ok(Expr::DictComp(Box::new(first), Box::new(v), comps));
            }
            let mut pairs = vec![(Some(first), v)];
            while self.eat_op(",") {
                if self.at_op("}") {
                    break;
                }
                if self.eat_op("**") {
                    pairs.push((None, self.parse_expr()?));
                    continue;
                }
                let k = self.parse_expr()?;
                self.expect_op(":")?;
                pairs.push((Some(k), self.parse_expr()?));
            }
            self.expect_op("}")?;
            Ok(Expr::Dict(pairs))
        } else if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op("}")?;
            Ok(Expr::SetComp(Box::new(first), comps))
        } else {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op("}") {
                    break;
                }
                items.push(self.parse_star_or_expr()?);
            }
            self.expect_op("}")?;
            Ok(Expr::Set(items))
        }
    }

    /// Whether the cursor is at the start of a comprehension clause: a `for`, or
    /// an `async for` (an asynchronous comprehension).
    fn at_comp_for(&self) -> bool {
        self.at_kw("for") || self.at_kw("async")
    }

    fn parse_comprehension_clauses(&mut self) -> Result<Vec<Comprehension>, String> {
        let mut comps = Vec::new();
        while self.at_kw("for") || self.at_kw("async") {
            let is_async = self.eat_kw("async");
            self.advance(); // for
            let target = self.parse_target_tuple()?;
            if !self.eat_kw("in") {
                return Err(format!(
                    "SyntaxError: comprehension missing 'in' (line {})",
                    self.line()
                ));
            }
            let iter = self.parse_or()?;
            let mut ifs = Vec::new();
            while self.at_kw("if") {
                self.advance();
                ifs.push(self.parse_or()?);
            }
            comps.push(Comprehension {
                target: Box::new(target),
                iter: Box::new(iter),
                ifs,
                is_async,
            });
        }
        Ok(comps)
    }
}

/// Find a top-level (not nested in brackets) occurrence of `ch`.
fn find_top_level(s: &str, ch: char) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if c == ch && depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Locate the `=` debug marker in an f-string field: the first top-level
/// standalone `=` that is not part of `==`/`!=`/`<=`/`>=`/`:=` and appears
/// before any top-level `:` (format spec). Tracks bracket depth and string
/// literals so a `=`/`:` inside `f(a=1)`, `d[i:j]`, or `"a=b"` is ignored.
fn find_debug_eq(field: &str) -> Option<usize> {
    let bytes = field.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            // A top-level `:` starts the format spec; a debug `=` must precede it.
            b':' if depth == 0 => return None,
            b'=' if depth == 0 => {
                let next = bytes.get(i + 1).copied();
                let prev = if i > 0 {
                    bytes.get(i - 1).copied()
                } else {
                    None
                };
                let is_eqeq = next == Some(b'=');
                let is_cmp = matches!(
                    prev,
                    Some(b'=') | Some(b'!') | Some(b'<') | Some(b'>') | Some(b':')
                );
                if !is_eqeq && !is_cmp {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn augassign_op(o: &str) -> Option<BinOp> {
    Some(match o {
        "+=" => BinOp::Add,
        "-=" => BinOp::Sub,
        "*=" => BinOp::Mul,
        "/=" => BinOp::Div,
        "//=" => BinOp::FloorDiv,
        "%=" => BinOp::Mod,
        "**=" => BinOp::Pow,
        "&=" => BinOp::BitAnd,
        "|=" => BinOp::BitOr,
        "^=" => BinOp::BitXor,
        "<<=" => BinOp::Shl,
        ">>=" => BinOp::Shr,
        "@=" => BinOp::MatMul,
        _ => return None,
    })
}

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
    let lexed = lex(&src)?;
    let unclosed = lexed.unclosed;
    let mut p = Parser {
        toks: lexed.toks,
        pos: 0,
        deferred: lexed.deferred,
        depth: 0,
        in_function: false,
        loop_depth: 0,
        nesting: 0,
        misplaced: None,
        groups: std::collections::HashMap::new(),
    };
    let err = match p.parse_module() {
        Ok(stmts) => match unclosed {
            Some(e) => e,
            None => return Ok(stmts),
        },
        // CPython's tokenizer reports a bracket still open at end of input
        // only when the parser gets that far: an error earlier in the file
        // wins, one at the end of the input is this one.
        Err(e) => match unclosed {
            Some(u) if p.pos + 2 >= p.toks.len() => u,
            // The parser failed while asking for the token after the bad
            // dedent that cut the stream short (`try:` whose body ends there
            // wants an `except`): in CPython that request is what raises the
            // tokenizer's `IndentationError`.
            _ => match p.deferred.take() {
                Some(d) if matches!(p.cur(), Tok::Eof) => d,
                _ => e,
            },
        },
    };
    // The offending line, as CPython's parser sees it: newline-terminated.
    let lineno = split_syntax_error(&err).1.and_then(|p| p.lineno);
    Err(match lineno.and_then(|l| source_line(&src, l, true)) {
        Some(text) => with_text(err, &text),
        None => err,
    })
}

// ── SyntaxError positions ────────────────────────────────────────────────────
//
// A syntax error travels as an error STRING (`"SyntaxError: msg"`), like every
// other error in the crate. What CPython attaches to one — `lineno`, `offset`,
// `end_lineno`, `end_offset`, `text`, `filename` — rides behind the message as
// a trailer of `\u{1}key=value` fields, which no source text can contain. The
// exception builder (`builtins::synth_exc`) turns them into attributes, the
// traceback renderer draws the `File`/source/caret block from them, and every
// place that shows the raw string strips them with [`split_syntax_error`].

/// Starts each field of a syntax error's position trailer.
pub const SYNTAX_FIELD: char = '\u{1}';

/// Where a syntax error is, in CPython's terms: 1-based line and column of
/// the start and end (`end_offset` exclusive, and 0 or -1 where CPython puts
/// those), the source line, and the file name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyntaxPos {
    pub lineno: Option<i64>,
    pub offset: Option<i64>,
    pub end_lineno: Option<i64>,
    pub end_offset: Option<i64>,
    pub text: Option<String>,
    pub filename: Option<String>,
    /// Raised by the compiler rather than the parser, which gives no source
    /// line: a traceback shows one only when it can read the file.
    pub no_source: bool,
    /// Raised by the symbol table, which builds the exception from its message
    /// alone (`args == (msg,)`) and sets the position only as attributes.
    pub bare_args: bool,
}

/// Attach a position to a syntax error message.
pub fn at_pos(msg: &str, lineno: u32, offset: i64, end_lineno: u32, end_offset: i64) -> String {
    format!("{msg}{SYNTAX_FIELD}pos={lineno}:{offset}:{end_lineno}:{end_offset}")
}

/// Attach the offending source line, unless the error already carries one or
/// carries no position to show it against.
pub fn with_text(err: String, text: &str) -> String {
    let fields = err.find(SYNTAX_FIELD).map_or("", |i| &err[i..]);
    if !fields.contains("\u{1}pos=")
        || fields.contains("\u{1}text=")
        || fields.contains("\u{1}nosrc=")
    {
        return err;
    }
    format!("{err}{SYNTAX_FIELD}text={text}")
}

/// Name the file a positioned syntax error came from (`<string>` for `exec`),
/// unless it already names one.
pub fn with_filename(err: String, filename: &str) -> String {
    let fields = err.find(SYNTAX_FIELD).map_or("", |i| &err[i..]);
    if !fields.contains("\u{1}pos=") || fields.contains("\u{1}file=") {
        return err;
    }
    format!("{err}{SYNTAX_FIELD}file={filename}")
}

/// The message part of an error string, and its position if it has one.
pub fn split_syntax_error(err: &str) -> (&str, Option<SyntaxPos>) {
    let Some(i) = err.find(SYNTAX_FIELD) else {
        return (err, None);
    };
    let mut pos = SyntaxPos::default();
    for field in err[i + 1..].split(SYNTAX_FIELD) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "pos" => {
                let n: Vec<Option<i64>> = value.split(':').map(|s| s.parse().ok()).collect();
                if let [l, o, el, eo] = n[..] {
                    pos.lineno = l;
                    pos.offset = o;
                    pos.end_lineno = el;
                    pos.end_offset = eo;
                }
            }
            "text" => pos.text = Some(value.to_string()),
            "nosrc" => pos.no_source = true,
            "bare" => pos.bare_args = true,
            "file" => pos.filename = Some(value.to_string()),
            _ => {}
        }
    }
    (&err[..i], Some(pos))
}

/// Re-read a syntax error in `eval()` input, which CPython tokenizes WITHOUT
/// the newline it adds to a file or to `exec` input: the offending line has no
/// newline unless the source gave it one, and an error at the end of the input
/// — where that newline would have been — is at offset 0.
pub fn for_eval_input(err: String, src: &str) -> String {
    let (head, Some(mut pos)) = split_syntax_error(&err) else {
        return err;
    };
    let last_line = src.split_inclusive('\n').count() as i64;
    let ends_without_newline = !src.ends_with('\n');
    if let (Some(text), Some(l)) = (&pos.text, pos.lineno) {
        if l == last_line && ends_without_newline {
            let bare = text.trim_end_matches('\n').to_string();
            let at_end = pos.offset == Some(bare.chars().count() as i64 + 1);
            if at_end {
                pos.offset = Some(0);
                pos.end_offset = Some(0);
            }
            pos.text = Some(bare);
        }
    }
    let mut out = head.to_string();
    if let (Some(l), Some(o), Some(el), Some(eo)) =
        (pos.lineno, pos.offset, pos.end_lineno, pos.end_offset)
    {
        out.push_str(&format!("{SYNTAX_FIELD}pos={l}:{o}:{el}:{eo}"));
    }
    if let Some(t) = &pos.text {
        out.push_str(&format!("{SYNTAX_FIELD}text={t}"));
    }
    if let Some(f) = &pos.filename {
        out.push_str(&format!("{SYNTAX_FIELD}file={f}"));
    }
    out
}

/// The line `lineno` of `src` as a syntax error's `text`: with its newline for
/// an error the PARSER raised (CPython's parser sees every line newline-
/// terminated), without it for one the tokenizer raised past the last newline.
pub fn source_line(src: &str, lineno: i64, keep_newline: bool) -> Option<String> {
    let idx = usize::try_from(lineno).ok()?.checked_sub(1)?;
    let line = src.split_inclusive('\n').nth(idx)?;
    Some(if keep_newline && !line.ends_with('\n') {
        format!("{line}\n")
    } else {
        line.to_string()
    })
}

/// CPython's rendering of a positioned syntax error below a traceback
/// (`traceback.TracebackException._format_syntax_error`): the `File` line, the
/// source line stripped of its indentation, and a caret run under
/// `[offset, end_offset)`.
pub fn render_syntax_block(pos: &SyntaxPos, default_file: &str) -> String {
    let file = pos.filename.as_deref().unwrap_or(default_file);
    let mut out = match pos.lineno {
        Some(l) => format!("  File \"{file}\", line {l}\n"),
        None => format!("  File \"{file}\"\n"),
    };
    let Some(text) = &pos.text else {
        return out;
    };
    let rtext = text.trim_end_matches('\n');
    let ltext = rtext.trim_start_matches([' ', '\n', '\x0c']);
    let spaces = (rtext.chars().count() - ltext.chars().count()) as i64;
    out.push_str(&format!("    {ltext}\n"));
    let Some(mut offset) = pos.offset else {
        return out;
    };
    let len = rtext.chars().count() as i64;
    let mut end = if pos.lineno == pos.end_lineno {
        match pos.end_offset {
            Some(e) if e != 0 => e,
            _ => offset,
        }
    } else {
        len + 1
    };
    let text_len = text.chars().count() as i64;
    if offset > text_len {
        offset = len + 1;
    }
    if end > text_len {
        end = len + 1;
    }
    if offset >= end || end < 0 {
        end = offset + 1;
    }
    let col = offset - 1 - spaces;
    let end_col = end - 1 - spaces;
    if col >= 0 {
        let lead: String = ltext
            .chars()
            .take(col as usize)
            .map(|c| if c.is_whitespace() { c } else { ' ' })
            .collect();
        out.push_str(&format!(
            "    {lead}{}\n",
            "^".repeat((end_col - col).max(0) as usize)
        ));
    }
    out
}

/// Check `src` is what `eval` accepts — CPython's `eval_input`: one expression
/// list, then nothing but line breaks. Anything after the expression is the
/// error, at that token; empty input is an error at line 0.
pub fn check_eval_input(src: &str) -> Result<(), String> {
    let lexed = lex(src)?;
    let mut p = Parser {
        toks: lexed.toks,
        pos: 0,
        deferred: None,
        depth: 0,
        in_function: false,
        loop_depth: 0,
        nesting: 0,
        misplaced: None,
        groups: std::collections::HashMap::new(),
    };
    p.skip_newlines();
    if matches!(p.cur(), Tok::Eof) {
        return Err(with_text(
            at_pos("SyntaxError: invalid syntax", 0, 0, 0, 0),
            "",
        ));
    }
    let checked = p.parse_exprlist().and_then(|_| {
        p.skip_newlines();
        if matches!(p.cur(), Tok::Eof) {
            Ok(())
        } else {
            Err(p.err_here("invalid syntax"))
        }
    });
    let lineno = checked
        .as_ref()
        .err()
        .and_then(|e| split_syntax_error(e).1?.lineno);
    checked.map_err(|e| match lineno.and_then(|l| source_line(src, l, false)) {
        Some(text) => with_text(e, &text),
        None => e,
    })
}

/// Deepest expression tree the parser will build before refusing the source.
///
/// This is a stack guard, not a language rule. Nothing here is recursive in the
/// tokenizer's bracket sense — `[`/`(`/`{` are already capped at
/// [`crate::lexer::MAX_PAREN_DEPTH`] — but an operator chain nests just as
/// deeply without a single bracket: `'-'*100000+'1'`, `'a'+'.b'*100000`,
/// `'1'+'+1'*200000`, `'not '*20000+'1'` and `'lambda:'*5000+'1'` each build a
/// tree tens of thousands of levels deep, and the parser, `src/compiler.rs`'s
/// walk and the AST's own `Drop` all recurse over it. Every one of those five
/// aborted the interpreter thread (`fatal runtime error: stack overflow`,
/// SIGABRT) before this cap; CPython answers all five with a catchable
/// exception.
///
/// The value sits above every depth CPython 3.14.6 accepts in those shapes
/// (measured: `'1'+'+1'*20000` and `'a'+'.b'*20000` parse, `*100000` does not)
/// and below the depth at which the 512 MB interpreter stack in
/// `src/main.rs` runs out (measured: the shapes above survive 25 000 levels and
/// abort by 30 000 on a debug build).
const MAX_TREE_DEPTH: u32 = 20_000;

/// What CPython's PEG parser raises when its own stack runs out —
/// `_PyPegen_run_parser`'s `MemoryError`, verified against
/// `python3 -c "exec('-'*100000+'1')"`. It is an ordinary catchable exception
/// there, which is the property this port is matching; CPython picks
/// `RecursionError: Stack overflow (used N kB) during compilation` instead when
/// the parse succeeds and the *compiler* is the stage that runs out, and that
/// split is not reproduced (see BUGS.md).
const TOO_COMPLEX: &str =
    "MemoryError: Parser stack overflowed - Python source too complex to parse";

/// What an argument list has already seen, so a later argument CPython cannot
/// lower is refused while parsing instead of being silently reordered.
///
/// The AST keeps positionals and keywords in two separate lists, so the source
/// order is gone by the time anything else could check it: `f(a=1, 2)` simply
/// became `args=[2], keywords=[a=1]` and called `f` with `(2,) {'a': 1}` where
/// CPython refuses the program outright. All three messages and the precedence
/// between them were measured against CPython 3.14.7.
#[derive(Default)]
struct ArgOrder {
    /// A `name=value` argument has been seen.
    keyword: bool,
    /// A `**mapping` unpacking has been seen.
    kw_unpack: bool,
}

impl ArgOrder {
    /// A plain positional argument. Illegal once any keyword has appeared;
    /// `**` unpacking takes precedence in the wording when both apply.
    fn positional(&self) -> Result<(), String> {
        if self.kw_unpack {
            Err("SyntaxError: positional argument follows keyword argument unpacking".into())
        } else if self.keyword {
            Err("SyntaxError: positional argument follows keyword argument".into())
        } else {
            Ok(())
        }
    }

    /// `*iterable`. Legal after a plain keyword — `f(a=1, *b)` is valid Python —
    /// but never after `**`.
    fn star(&self) -> Result<(), String> {
        if self.kw_unpack {
            Err(
                "SyntaxError: iterable argument unpacking follows keyword argument unpacking"
                    .into(),
            )
        } else {
            Ok(())
        }
    }
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// A tokenizer error held back so an earlier parse error wins. See
    /// [`crate::lexer::Lexed::deferred`].
    deferred: Option<String>,
    /// Levels of expression tree currently under construction. See
    /// [`MAX_TREE_DEPTH`].
    depth: u32,
    /// Whether the statements being read are in a function body, and how many
    /// loop bodies deep they are within it — what `return`, `break` and
    /// `continue` need. A `class` body starts both afresh.
    in_function: bool,
    loop_depth: u32,
    /// How many `def`/`class` bodies enclose the statements being read.
    nesting: u32,
    /// The first `'return' outside function` / `'break' outside loop` /
    /// `'continue' not properly in loop` met. CPython's compiler raises these
    /// only once the whole file has PARSED, so a syntax error anywhere in the
    /// file wins over them; it is reported when the parse succeeds.
    misplaced: Option<String>,
    /// Every parenthesized group read so far, keyed by the token index of its
    /// `(`: the index of its `)` and the (line, column) of the expression
    /// inside. CPython's AST has no node for a group, so the position it gives
    /// `(x)` is `x`'s — see [`Parser::node_start`].
    groups: std::collections::HashMap<usize, (usize, u32, u32)>,
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
    // ── stack guard ───────────────────────────────────────────────────────
    /// Charge one level of expression nesting. Every caller pairs this with a
    /// `self.depth = saved` on the success path; the error path never restores
    /// because it unwinds the whole parse.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_TREE_DEPTH {
            return Err(TOO_COMPLEX.to_string());
        }
        Ok(())
    }

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
    /// The (line, column) CPython's AST gives the expression spanning tokens
    /// `[first, end)`: its first token's, unless the expression is one
    /// parenthesized group, whose position is the inner expression's.
    fn node_start(&self, first: usize, end: usize) -> (u32, u32) {
        match self.groups.get(&first) {
            Some(&(close, line, col)) if close + 1 == end => (line, col),
            _ => (self.toks[first].line, self.toks[first].col),
        }
    }

    /// The rest of a binary operation whose left operand `left` started at
    /// (`sl`, `sc`) and whose operator `op` is the current token: the operator,
    /// the right operand `parse_right` reads, and the caret span over both.
    ///
    /// The `^` anchor is CPython's `traceback._extract_caret_anchors_from_line_segment`
    /// for `ast.BinOp`: it starts at the operator and is one character wider than
    /// a one-character operator when the next character is not blank, `\` or
    /// `#` and lies before the right operand's AST position. That holds only
    /// when the right operand is a parenthesized group, whose position is inside
    /// the parentheses, so `1+("a")` anchors `+(`.
    fn binop_tail(
        &mut self,
        left: Expr,
        op: BinOp,
        (sl, sc): (u32, u32),
        parse_right: fn(&mut Self) -> Result<Expr, String>,
    ) -> Result<Expr, String> {
        self.enter()?;
        let op_tok = self.toks[self.pos].clone();
        self.advance();
        let right_first = self.pos;
        let right = parse_right(self)?;
        let mut anchor_end = op_tok.end_col;
        let next = &self.toks[right_first];
        if op_tok.end_col - op_tok.col == 1
            && next.line == op_tok.line
            && next.col == op_tok.end_col
            && self.node_start(right_first, self.pos) > (op_tok.line, op_tok.end_col)
        {
            anchor_end += 1;
        }
        let e = Expr::BinOp(op, Box::new(left), Box::new(right));
        Ok(spanned(e, sl, sc, self.prev_end_col(), op_tok.col, anchor_end))
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
            // A block header that runs into the end of its line lacks its `:`,
            // which CPython names; anything else found where punctuation was
            // due is its generic message. Either way the position is the token
            // found instead.
            Err(self.err_here(if s == ":" && self.at_newline() {
                "expected ':'"
            } else {
                "invalid syntax"
            }))
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
            _ => Err(self.err_here("invalid syntax")),
        }
    }

    /// A syntax error spanning the current token.
    fn err_here(&self, msg: &str) -> String {
        self.err_span(msg, self.pos, self.pos)
    }

    /// A syntax error spanning tokens `from..=to`, in CPython's terms: the
    /// 1-based column of the first token's start and of the last one's end. A
    /// line break has no width in the source, so CPython underlines one column
    /// there.
    fn err_span(&self, msg: &str, from: usize, to: usize) -> String {
        self.err_span_as("SyntaxError", msg, from, to)
    }

    /// [`Parser::err_span`] for a subclass: `IndentationError`.
    fn err_span_as(&self, class: &str, msg: &str, from: usize, to: usize) -> String {
        let (line, offset, end_line, end_offset) = self.token_span(from, to);
        at_pos(
            &format!("{class}: {msg}"),
            line,
            offset as i64,
            end_line,
            end_offset as i64,
        )
    }

    /// The extent of tokens `from..=to` in `SyntaxError` terms: `(lineno,
    /// offset, end_lineno, end_offset)`, 1-based, end exclusive.
    fn token_span(&self, from: usize, to: usize) -> (u32, u32, u32, u32) {
        let a = &self.toks[from.min(self.toks.len() - 1)];
        let b = &self.toks[to.min(self.toks.len() - 1)];
        let end = match b.tok {
            Tok::Newline | Tok::Eof | Tok::Indent | Tok::Dedent => b.col + 1,
            _ => b.end_col,
        };
        (a.line, a.col + 1, b.line, end + 1)
    }

    /// Wrap a `yield` / `yield from` / `await` that starts at token `start`
    /// and ends at the last token read in its source span, which is where the
    /// compiler and the symbol table report one in the wrong place. A span
    /// records one line, so an expression continued onto another line stays
    /// unwrapped (and its error unpositioned).
    fn span_suspension(&self, e: Expr, start: usize) -> Expr {
        self.span_from(e, start)
    }

    /// Wrap `e`, read from token `start` up to the last token consumed, with that
    /// extent as its caret span. A span records one line, so an expression
    /// continued onto another line is returned unwrapped.
    fn span_from(&self, e: Expr, start: usize) -> Expr {
        let (first, last) = (&self.toks[start], &self.toks[self.pos.saturating_sub(1)]);
        if first.line != last.line {
            return e;
        }
        spanned(e, first.line, first.col, last.end_col, 0, 0)
    }

    /// A simple statement must end the logical line or be followed by `;`.
    /// Anything else after it — `a b`, `x = 1 2` — is where CPython's parser
    /// gives up, except that a bare `print` followed by an expression is the
    /// Python 2 statement it names.
    fn check_stmt_end(&self, stmt_start: usize) -> Result<(), String> {
        if matches!(self.cur(), Tok::Newline | Tok::Eof | Tok::Dedent) || self.at_op(";") {
            return Ok(());
        }
        let legacy = match &self.toks[stmt_start].tok {
            Tok::Name(n) if self.pos == stmt_start + 1 && (n == "print" || n == "exec") => {
                Some(n.clone())
            }
            _ => None,
        };
        if let (Some(n), true) = (legacy, self.starts_expression()) {
            let mut last = self.pos;
            while last + 1 < self.toks.len()
                && !matches!(self.toks[last + 1].tok, Tok::Newline | Tok::Eof)
            {
                last += 1;
            }
            return Err(self.err_span(
                &format!("Missing parentheses in call to '{n}'. Did you mean {n}(...)?"),
                stmt_start,
                last,
            ));
        }
        Err(self.err_here("invalid syntax"))
    }

    /// Two expressions side by side inside brackets — `f(a b)`, `[1 2]` — are
    /// CPython's `invalid_expression`: `invalid syntax. Perhaps you forgot a
    /// comma?` underlining both. Not when the first is a name directly before
    /// a string (a mistyped prefix) or the legacy `print`/`exec` statement.
    fn comma_hint(&mut self, a_start: usize) -> Result<(), String> {
        if !self.starts_expression() {
            return Ok(());
        }
        let a = self.toks[a_start].tok.clone();
        let single_name = self.pos == a_start + 1 && matches!(a, Tok::Name(_));
        if single_name && matches!(self.cur(), Tok::Str(..)) {
            return Ok(());
        }
        let legacy = match &a {
            Tok::Name(n) if single_name && (n == "print" || n == "exec") => Some(n.clone()),
            _ => None,
        };
        let b_start = self.pos;
        let b_end = match self.parse_expr() {
            Ok(_) => self.pos - 1,
            Err(_) => b_start,
        };
        let msg = match legacy {
            // `invalid_legacy_expression`: the Python 2 statement, named.
            Some(n) => format!("Missing parentheses in call to '{n}'. Did you mean {n}(...)?"),
            None => "invalid syntax. Perhaps you forgot a comma?".to_string(),
        };
        Err(self.err_span(&msg, a_start, b_end))
    }

    /// CPython's `invalid_assignment` / `invalid_named_expression`: the first
    /// target, left to right, that is not a name, attribute, subscript or a
    /// list/tuple/star of those. A lone `target = value` whose target is an
    /// ordinary expression is the likely typo for `==`, and says so.
    fn check_assign_targets(
        &self,
        targets: &[Expr],
        spans: &[(usize, usize)],
    ) -> Result<(), String> {
        let single = targets.len() == 1;
        // `*a, 1 = x`: the LAST item of a bare tuple target is itself read as
        // `1 = x`, which is the `==` typo CPython's parser meets first.
        if let (true, Some(Expr::Tuple(items))) = (single, targets.first().map(unspan)) {
            let (a, b) = spans[0];
            let bare = !matches!(&self.toks[a].tok, Tok::Op(o) if o == "(");
            if let (true, Some(last), Some(&(la, lb))) =
                (bare, items.last(), self.split_commas(a, b).last())
            {
                let last = unspan(last);
                let plain = !matches!(
                    last,
                    Expr::Name(_)
                        | Expr::Attribute(..)
                        | Expr::Subscript(..)
                        | Expr::Starred(_)
                        | Expr::Tuple(_)
                        | Expr::List(_)
                        | Expr::GenExp(..)
                        | Expr::True
                        | Expr::False
                        | Expr::None
                        | Expr::BoolOp(..)
                        | Expr::UnaryOp(UnOp::Not, _)
                        | Expr::Compare(..)
                        | Expr::IfExp { .. }
                        | Expr::Lambda { .. }
                        | Expr::NamedExpr(..)
                );
                if plain {
                    return Err(self.err_span(
                        &format!(
                            "cannot assign to {} here. Maybe you meant '==' instead of '='?",
                            expr_name(last)
                        ),
                        la,
                        lb,
                    ));
                }
            }
        }
        for (t, &(a, b)) in targets.iter().zip(spans) {
            let Some((what, (ia, ib))) = self.invalid_target(t, a, b) else {
                continue;
            };
            let e = unspan(t);
            let comparable = !matches!(
                e,
                Expr::Tuple(_)
                    | Expr::List(_)
                    | Expr::GenExp(..)
                    | Expr::True
                    | Expr::False
                    | Expr::None
                    | Expr::BoolOp(..)
                    | Expr::UnaryOp(UnOp::Not, _)
                    | Expr::Compare(..)
                    | Expr::IfExp { .. }
                    | Expr::Lambda { .. }
                    | Expr::NamedExpr(..)
            );
            if matches!(e, Expr::Yield(_) | Expr::YieldFrom(_)) {
                return Err(self.err_span("assignment to yield expression not possible", a, b));
            }
            let msg = if single && comparable {
                format!("cannot assign to {what} here. Maybe you meant '==' instead of '='?")
            } else {
                format!("cannot assign to {what}")
            };
            return Err(self.err_span(&msg, ia, ib));
        }
        Ok(())
    }

    /// The part of `e` — spanning tokens `a..=b` — that cannot be assigned to
    /// or deleted, with CPython's name for it (`_PyPegen_get_invalid_target`).
    fn invalid_target(
        &self,
        e: &Expr,
        a: usize,
        b: usize,
    ) -> Option<(&'static str, (usize, usize))> {
        match unspan(e) {
            Expr::Name(_) | Expr::Attribute(..) | Expr::Subscript(..) => None,
            Expr::Starred(inner) => self.invalid_target(inner, a + 1, b),
            Expr::Tuple(items) | Expr::List(items) => {
                let (ia, ib) = self.strip_brackets(a, b);
                let parts = self.split_commas(ia, ib);
                items
                    .iter()
                    .zip(parts)
                    .find_map(|(item, (x, y))| self.invalid_target(item, x, y))
            }
            other => Some((expr_name(other), (a, b))),
        }
    }

    /// Tokens `a..=b` without one enclosing pair of brackets, if they have one.
    fn strip_brackets(&self, a: usize, b: usize) -> (usize, usize) {
        let open = matches!(&self.toks[a].tok, Tok::Op(o) if o == "(" || o == "[");
        let close = matches!(&self.toks[b].tok, Tok::Op(o) if o == ")" || o == "]");
        if open && close && b > a + 1 && self.split_commas(a, b).len() == 1 {
            (a + 1, b - 1)
        } else {
            (a, b)
        }
    }

    /// The comma-separated items of tokens `a..=b`, at bracket depth 0, as
    /// token ranges. A trailing comma adds no item.
    fn split_commas(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut start = a;
        for i in a..=b.min(self.toks.len() - 1) {
            match &self.toks[i].tok {
                Tok::Op(o) if matches!(o.as_str(), "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(o.as_str(), ")" | "]" | "}") => depth -= 1,
                Tok::Op(o) if o == "," && depth == 0 => {
                    if i > start {
                        out.push((start, i - 1));
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start <= b {
            out.push((start, b));
        }
        out
    }

    /// Whether the current token can begin an expression.
    fn starts_expression(&self) -> bool {
        match self.cur() {
            Tok::Name(n) => {
                !is_keyword(n)
                    || matches!(
                        n.as_str(),
                        "None" | "True" | "False" | "not" | "lambda" | "await"
                    )
            }
            Tok::Op(o) => matches!(o.as_str(), "(" | "[" | "{" | "-" | "+" | "~" | "..."),
            Tok::Newline | Tok::Indent | Tok::Dedent | Tok::Eof => false,
            _ => true,
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
        // Everything the tokenizer did produce parsed: the bad dedent that cut
        // it short IS the error, so report it now.
        match self.deferred.take().or_else(|| self.misplaced.take()) {
            Some(e) => Err(e),
            None => Ok(stmts),
        }
    }

    /// Record a control statement outside what it controls, spanning the
    /// statement from `start` to the last token read. See
    /// [`Parser::misplaced`]. Its text is left for the traceback to read from
    /// the file, as CPython's compiler leaves it.
    fn note_misplaced(&mut self, msg: &str, start: usize) {
        if self.misplaced.is_none() {
            let e = self.err_span(msg, start, self.pos.saturating_sub(1));
            self.misplaced = Some(format!("{e}{SYNTAX_FIELD}nosrc=1"));
        }
    }

    /// Read a loop body: `break` and `continue` are legal in it.
    fn parse_loop_body(&mut self, desc: &str, line: u32) -> Result<Vec<Stmt>, String> {
        self.loop_depth += 1;
        let body = self.parse_suite(desc, line);
        self.loop_depth -= 1;
        body
    }

    /// Read a `def` or `class` body, which starts a new function context (or,
    /// for a class, none) with no enclosing loop.
    fn parse_scope_body(
        &mut self,
        desc: &str,
        line: u32,
        function: bool,
    ) -> Result<Vec<Stmt>, String> {
        let saved = (self.in_function, self.loop_depth);
        self.in_function = function;
        self.loop_depth = 0;
        self.nesting += 1;
        let body = self.parse_suite(desc, line);
        self.nesting -= 1;
        (self.in_function, self.loop_depth) = saved;
        body
    }

    /// A suite after a `:` — either a one-line simple statement or an indented
    /// block.
    /// `desc` names the construct for the missing-block error (CPython's
    /// `expected an indented block after <desc> on line <kw_line>`), e.g. `'if'
    /// statement`, `function definition`, `class definition`. `kw_line` is the
    /// line of the compound-statement keyword, not the (later) blank/dedent line.
    fn parse_suite(&mut self, desc: &str, kw_line: u32) -> Result<Vec<Stmt>, String> {
        // `def`, `try`, `else` and `finally` take their `:` as a FORCED token
        // (`&&':'` in CPython's grammar): whatever stands in its place is
        // reported as the missing colon.
        let forced = matches!(
            desc,
            "function definition" | "'try' statement" | "'else' statement" | "'finally' statement"
        );
        if forced && !self.at_op(":") {
            return Err(self.err_here("expected ':'"));
        }
        self.expect_op(":")?;
        if self.at_newline() {
            self.skip_newlines();
            if !matches!(self.cur(), Tok::Indent) {
                return Err(self.err_span_as(
                    "IndentationError",
                    &format!("expected an indented block after {desc} on line {kw_line}"),
                    self.pos,
                    self.pos,
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
            // CPython's position here is the indentation's WIDTH (so one short
            // of the first character's 1-based column) and an end of -1.
            let first = &self.toks[(self.pos + 1).min(self.toks.len() - 1)];
            return Err(at_pos(
                "IndentationError: unexpected indent",
                first.line,
                first.col as i64,
                first.line,
                -1,
            ));
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
            let stmt_start = self.pos;
            self.parse_simple_stmt(out)?;
            self.check_stmt_end(stmt_start)?;
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
                    let start = self.pos;
                    self.advance();
                    if self.loop_depth == 0 {
                        self.note_misplaced("'break' outside loop", start);
                    }
                    out.push(Stmt::new(StmtKind::Break, line));
                    return Ok(());
                }
                "continue" => {
                    let start = self.pos;
                    self.advance();
                    if self.loop_depth == 0 {
                        self.note_misplaced("'continue' not properly in loop", start);
                    }
                    out.push(Stmt::new(StmtKind::Continue, line));
                    return Ok(());
                }
                "return" => {
                    let start = self.pos;
                    self.advance();
                    let v =
                        if self.at_newline() || self.at_op(";") || matches!(self.cur(), Tok::Eof) {
                            None
                        } else {
                            Some(self.parse_exprlist()?)
                        };
                    if !self.in_function {
                        self.note_misplaced("'return' outside function", start);
                    }
                    out.push(Stmt::new(StmtKind::Return(v), line));
                    return Ok(());
                }
                "raise" => return self.parse_raise(out, line),
                "del" => {
                    self.advance();
                    let start = self.pos;
                    let targets = self.parse_target_list()?;
                    let items = self.split_commas(start, self.pos - 1);
                    for (t, (a, b)) in targets.iter().zip(items) {
                        if let Some((what, (a, b))) = self.invalid_target(t, a, b) {
                            return Err(self.err_span(&format!("cannot delete {what}"), a, b));
                        }
                    }
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
                    let start = self.pos;
                    self.advance();
                    let names = self.parse_name_list()?;
                    let mut stmt = Stmt::new(StmtKind::Global(names), line);
                    stmt.span = Some(self.token_span(start, self.pos - 1));
                    out.push(stmt);
                    return Ok(());
                }
                "nonlocal" => {
                    let start = self.pos;
                    self.advance();
                    let names = self.parse_name_list()?;
                    if self.nesting == 0 && self.misplaced.is_none() {
                        self.note_misplaced(
                            "nonlocal declaration not allowed at module level",
                            start,
                        );
                        // A symbol-table error: see `SyntaxPos::bare_args`.
                        if let Some(e) = &mut self.misplaced {
                            e.push_str(&format!("{SYNTAX_FIELD}bare=1"));
                        }
                    }
                    let mut stmt = Stmt::new(StmtKind::Nonlocal(names), line);
                    stmt.span = Some(self.token_span(start, self.pos - 1));
                    out.push(stmt);
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
        let first_start = self.pos;
        let first = self.parse_exprlist()?;
        // An assignment expression is not a statement unless parenthesized:
        // `a := 1` stops at the `:=`.
        if matches!(first, Expr::NamedExpr(..))
            && matches!(self.toks[first_start].tok, Tok::Name(_))
        {
            return Err(self.err_span("invalid syntax", first_start + 1, first_start + 1));
        }
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
        let first_end = self.pos.saturating_sub(1);
        // Augmented assignment.
        if let Tok::Op(o) = self.cur().clone() {
            if let Some(op) = augassign_op(&o) {
                // Only a name, an attribute or a subscript can be augmented.
                let target = unspan(&first);
                if !matches!(
                    target,
                    Expr::Name(_) | Expr::Attribute(..) | Expr::Subscript(..)
                ) {
                    return Err(self.err_span(
                        &format!(
                            "'{}' is an illegal expression for augmented assignment",
                            expr_name(target)
                        ),
                        first_start,
                        first_end,
                    ));
                }
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
            let mut spans = vec![(first_start, first_end)];
            let mut value = None;
            while self.eat_op("=") {
                let start = self.pos;
                let e = self.parse_exprlist()?;
                if let Some(prev) = value.take() {
                    targets.push(prev);
                }
                spans.push((start, self.pos - 1));
                value = Some(e);
            }
            self.check_assign_targets(&targets, &spans)?;
            // A tuple/list target raises on a wrong item count, and CPython
            // carets the whole target (`a, b = [1]` → `^^^^`). Parenthesized
            // ones already carry a span; a bare `a, b` gets its token extent.
            for (t, &(from, to)) in targets.iter_mut().zip(&spans) {
                if matches!(t, Expr::Tuple(_) | Expr::List(_)) {
                    let (a, b) = (&self.toks[from], &self.toks[to]);
                    if a.line == b.line {
                        let bare = std::mem::replace(t, Expr::Tuple(Vec::new()));
                        *t = spanned(bare, a.line, a.col, b.end_col, 0, 0);
                    }
                }
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
        let body = self.parse_loop_body("'while' statement", line)?;
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
        let body = self.parse_loop_body("'for' statement", line)?;
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
        let start = self.pos;
        let first = self.parse_target_atom()?;
        let target = if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_kw("in") || self.at_op("=") || self.at_op(":") {
                    break;
                }
                items.push(self.parse_target_atom()?);
            }
            Expr::Tuple(items)
        } else {
            first
        };
        Ok(self.span_unpack_target(target, start))
    }

    /// An unpacking target (`a, b` / `(a, b)` / `[a, b]`) raises on a wrong
    /// item count, and CPython carets the whole target, parentheses included
    /// (`for a, b in …` → `^^^^`). Any other target is returned as it is.
    fn span_unpack_target(&self, target: Expr, start: usize) -> Expr {
        match target {
            Expr::Tuple(_) | Expr::List(_) => self.span_from(target, start),
            other => other,
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

    /// A parenthesized with-item list — `with (a as x, b as y):`, CPython 3.10+
    /// (PEP 617 rewrote the grammar in PEG, which can backtrack over the
    /// `(`-tuple ambiguity). The alternative is tried FIRST and wins whenever
    /// the group closes immediately before the `:`, so `with (a, b):` is TWO
    /// context managers, not one tuple. Anything else — `with (a, b)[0]:`,
    /// `with (x for x in y):`, `with (a) as x:`, `with ():` — fails the shape
    /// test, the cursor is restored, and the plain expression path parses it.
    ///
    /// Returns `Ok(None)` with the cursor exactly where it started when the
    /// group is not an item list; a parse error inside the group is not an
    /// error here, it is a non-match (the fallback path reports the real one).
    fn parenthesized_with_items(&mut self) -> Option<Vec<WithItem>> {
        if !self.at_op("(") {
            return None;
        }
        let save = self.pos;
        self.advance();
        let mut items = Vec::new();
        let shaped = loop {
            if self.at_op(")") {
                // Closing paren: end of a `a, b,` trailing-comma list. An empty
                // group is the `()` tuple literal, not an item list.
                break !items.is_empty();
            }
            let Ok(context) = self.parse_expr() else {
                break false;
            };
            let vars = if self.eat_kw("as") {
                let start = self.pos;
                match self.parse_ternary() {
                    Ok(v) => Some(self.span_unpack_target(v, start)),
                    Err(_) => break false,
                }
            } else {
                None
            };
            items.push(WithItem { context, vars });
            if !self.eat_op(",") {
                break true;
            }
        };
        if shaped && self.eat_op(")") && self.at_op(":") {
            return Some(items);
        }
        self.pos = save;
        None
    }

    fn parse_with(&mut self, out: &mut Vec<Stmt>, line: u32, is_async: bool) -> Result<(), String> {
        self.advance();
        let items = match self.parenthesized_with_items() {
            Some(items) => items,
            None => {
                let mut items = Vec::new();
                loop {
                    let context = self.parse_expr()?;
                    let vars = if self.eat_kw("as") {
                        let start = self.pos;
                        let v = self.parse_ternary()?;
                        Some(self.span_unpack_target(v, start))
                    } else {
                        None
                    };
                    items.push(WithItem { context, vars });
                    if !self.eat_op(",") {
                        break;
                    }
                }
                items
            }
        };
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
        let _ = line;
        Err(self.err_here("invalid syntax"))
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
            // CPython names nothing here — its grammar has no rule left to
            // try after a decorator that no `def`/`class` follows, so it
            // reports the generic message. Checked against 3.14.6 for `@dec`
            // alone, `@dec` + a statement, and `@dec` + a blank line.
            {
                let _ = line;
                Err("SyntaxError: invalid syntax".to_string())
            }
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
                Tok::Eof => return Err("SyntaxError: unterminated type-parameter list".to_string()),
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
        let body = self.parse_scope_body("function definition", line, true)?;
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
    /// A parameter list — `def`'s up to `)`, a lambda's up to `:` — with the
    /// ordering rules CPython's `invalid_parameters` family reports, each at
    /// the parameter that breaks it. A repeated name is the compiler's
    /// `duplicate argument`, raised once the whole file has parsed.
    fn parse_params(&mut self, close: &str) -> Result<Params, String> {
        let mut p = Params::default();
        let mut seen_star = false;
        let mut seen_default = false;
        let mut seen_kwargs = false;
        let mut names_seen: Vec<String> = Vec::new();
        let mut check_dup = |parser: &mut Self, name: &str, at: usize| {
            if names_seen.iter().any(|n| n == name) {
                if parser.misplaced.is_none() {
                    let e = parser.err_span(
                        &format!("duplicate argument '{name}' in function definition"),
                        at,
                        at,
                    );
                    parser.misplaced =
                        Some(format!("{e}{SYNTAX_FIELD}nosrc=1{SYNTAX_FIELD}bare=1"));
                }
            } else {
                names_seen.push(name.to_string());
            }
        };
        loop {
            if self.at_op(close) {
                break;
            }
            if seen_kwargs {
                return Err(self.err_here("arguments cannot follow var-keyword argument"));
            }
            if self.eat_op("/") {
                p.posonly = p.names.len();
                let _ = self.eat_op(",");
                continue;
            }
            if self.at_op("*") {
                if seen_star {
                    return Err(self.err_here("* argument may appear only once"));
                }
                let star_at = self.pos;
                self.advance();
                if self.at_op(close)
                    || self.at_op(",")
                        && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == close || o == "**")
                {
                    // A `def` names the `*`; a lambda's rule raises at the
                    // token it had read past it.
                    let at = if close == ")" { star_at } else { self.pos };
                    return Err(self.err_span("named arguments must follow bare *", at, at));
                }
                if self.at_op(",") {
                    p.star = Some(String::new()); // bare `*`
                } else {
                    let name_at = self.pos;
                    let star_name = self.expect_name()?;
                    check_dup(self, &star_name, name_at);
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
                let name_at = self.pos;
                let kw_name = self.expect_name()?;
                check_dup(self, &kw_name, name_at);
                seen_kwargs = true;
                if close == ")" && self.at_op(":") {
                    self.advance();
                    let ann = self.parse_expr()?;
                    p.annotations.push((kw_name.clone(), ann));
                }
                p.kwargs = Some(kw_name);
                let _ = self.eat_op(",");
                continue;
            }
            let name_at = self.pos;
            let name = self.expect_name()?;
            check_dup(self, &name, name_at);
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
            if default.is_some() {
                seen_default = !seen_star;
            } else if seen_default && !seen_star {
                return Err(self.err_span(
                    "parameter without a default follows parameter with a default",
                    name_at,
                    name_at,
                ));
            }
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
            let mut order = ArgOrder::default();
            while !self.at_op(")") {
                if self.eat_op("**") {
                    order.kw_unpack = true;
                    keywords.push(Keyword {
                        name: None,
                        value: self.parse_expr()?,
                    });
                } else if matches!(self.cur(), Tok::Name(n) if !is_keyword(n))
                    && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == "=")
                {
                    order.keyword = true;
                    let kn = self.expect_name()?;
                    self.expect_op("=")?;
                    keywords.push(Keyword {
                        name: Some(kn),
                        value: self.parse_expr()?,
                    });
                } else {
                    order.positional()?;
                    bases.push(self.parse_expr()?);
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let body = self.parse_scope_body("class definition", line, false)?;
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
                let start = self.pos;
                let mut t = self.parse_expr()?;
                // PEP 758 (3.14): `except A, B:` catches either, as the
                // parenthesized tuple does — but with `as` the parentheses are
                // still required.
                if self.at_op(",") {
                    let mut types = vec![t];
                    while self.eat_op(",") && !self.at_op(":") && !self.at_kw("as") {
                        types.push(self.parse_expr()?);
                    }
                    if self.eat_kw("as") {
                        self.expect_name()?;
                        return Err(self.err_span(
                            "multiple exception types must be parenthesized when using 'as'",
                            start,
                            self.pos - 1,
                        ));
                    }
                    t = Expr::Tuple(types);
                }
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
        // A `try` block that catches nothing and cleans up nothing is not a
        // statement CPython's grammar admits; pythonrs used to parse it and run
        // the body. An `else` there does not help, and is where it is reported:
        // at whatever follows the body, or — at the end of the input — just
        // past the last line.
        if handlers.is_empty() && !self.at_kw("finally") {
            const MSG: &str = "expected 'except' or 'finally' block";
            if matches!(self.cur(), Tok::Eof | Tok::Dedent) {
                if let Some(nl) = self.toks[..self.pos]
                    .iter()
                    .rev()
                    .find(|t| matches!(t.tok, Tok::Newline))
                {
                    return Err(at_pos(
                        &format!("SyntaxError: {MSG}"),
                        nl.line,
                        nl.col as i64 + 1,
                        nl.line,
                        -1,
                    ));
                }
            }
            return Err(self.err_here(MSG));
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
            | Tok::TString(_, _)
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
        self.names_after_import()?;
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

    /// `import` with nothing after it on the line: CPython's
    /// `invalid_import`/`invalid_import_from_targets` wording, at the end of
    /// the line with no width.
    fn names_after_import(&self) -> Result<(), String> {
        if !matches!(self.cur(), Tok::Newline | Tok::Eof) {
            return Ok(());
        }
        let t = &self.toks[self.pos];
        Err(at_pos(
            "SyntaxError: Expected one or more names after 'import'",
            t.line,
            t.col as i64 + 1,
            t.line,
            t.col as i64 + 1,
        ))
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
            return Err(self.err_here("invalid syntax"));
        }
        self.names_after_import()?;
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
        let saved = self.depth;
        self.enter()?;
        if self.at_kw("lambda") {
            let e = self.parse_lambda()?;
            self.depth = saved;
            return Ok(e);
        }
        let body_start = self.pos;
        let body = self.parse_or()?;
        if self.at_kw("if") {
            self.advance();
            let test = self.parse_or()?;
            if !self.eat_kw("else") {
                // The whole `body if test`, as CPython's `invalid_expression`
                // rule underlines it.
                return Err(self.err_span(
                    "expected 'else' after 'if' expression",
                    body_start,
                    self.pos - 1,
                ));
            }
            let orelse = self.parse_ternary()?;
            self.depth = saved;
            return Ok(Expr::IfExp {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            });
        }
        self.depth = saved;
        Ok(body)
    }

    fn parse_lambda(&mut self) -> Result<Expr, String> {
        self.advance(); // lambda
        let params = self.parse_params(":")?;
        self.expect_op(":")?;
        // A lambda body is an expression, and an unparenthesized `yield` is not
        // one there.
        if self.at_kw("yield") {
            return Err(self.err_here("invalid syntax"));
        }
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
            let saved = self.depth;
            self.enter()?;
            let e = self.parse_not()?;
            self.depth = saved;
            return Ok(Expr::UnaryOp(UnOp::Not, Box::new(e)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
        let (sl, sc) = (self.line(), self.col());
        let left = self.parse_bitor()?;
        let mut ops = Vec::new();
        loop {
            self.enter()?;
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
        self.depth = saved;
        if ops.is_empty() {
            Ok(left)
        } else {
            // CPython underlines a comparison with a plain `^` span (no `~^~`
            // operator anchor), so a bare `a < b` that spans the whole line is
            // hidden and `x = a < b` shows `^^^^^`.
            let end = self.prev_end_col();
            Ok(spanned(
                Expr::Compare(Box::new(left), ops),
                sl,
                sc,
                end,
                0,
                0,
            ))
        }
    }

    fn parse_bitor(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_bitxor()?;
        while self.at_op("|") {
            e = self.binop_tail(e, BinOp::BitOr, (sl, sc), Self::parse_bitxor)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_bitxor(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_bitand()?;
        while self.at_op("^") {
            e = self.binop_tail(e, BinOp::BitXor, (sl, sc), Self::parse_bitand)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_bitand(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
        let (sl, sc) = (self.line(), self.col());
        let mut e = self.parse_shift()?;
        while self.at_op("&") {
            e = self.binop_tail(e, BinOp::BitAnd, (sl, sc), Self::parse_shift)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_shift(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
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
            e = self.binop_tail(e, op, (sl, sc), Self::parse_arith)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_arith(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
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
            e = self.binop_tail(e, op, (sl, sc), Self::parse_term)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_term(&mut self) -> Result<Expr, String> {
        let saved = self.depth;
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
            e = self.binop_tail(e, op, (sl, sc), Self::parse_unary)?;
        }
        self.depth = saved;
        Ok(e)
    }
    fn parse_unary(&mut self) -> Result<Expr, String> {
        let (sl, sc) = (self.line(), self.col());
        let unary = |p: &mut Self, op: UnOp| -> Result<Expr, String> {
            let saved = p.depth;
            p.enter()?;
            p.advance();
            let operand = p.parse_unary()?;
            p.depth = saved;
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
        let saved = self.depth;
        let start = self.pos;
        if self.eat_kw("await") {
            self.enter()?;
            let e = self.parse_await_postfix()?;
            self.depth = saved;
            return Ok(self.span_suspension(Expr::Await(Box::new(e)), start));
        }
        // Span of the whole postfix chain starts at the value's first token; each
        // trailer wraps its result so a call/subscript/attribute that raises
        // underlines from here to its closing bracket / attribute name.
        let (start_line, start_col) = (self.line(), self.col());
        let mut e = self.parse_atom()?;
        loop {
            if self.at_op("(") {
                self.enter()?;
                // Anchor the call's `(...)` bracket region for the `~~~^^^` caret.
                let paren_col = self.col();
                e = self.parse_call(e)?;
                let end = self.prev_end_col();
                e = spanned(e, start_line, start_col, end, paren_col, end);
            } else if self.at_op("[") {
                self.enter()?;
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
            } else if self.at_op(".") {
                self.enter()?;
                self.advance();
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
        self.depth = saved;
        Ok(e)
    }

    fn parse_call(&mut self, func: Expr) -> Result<Expr, String> {
        self.expect_op("(")?;
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        let mut order = ArgOrder::default();
        while !self.at_op(")") {
            if self.eat_op("*") {
                order.star()?;
                args.push(Expr::Starred(Box::new(self.parse_expr()?)));
            } else if self.eat_op("**") {
                order.kw_unpack = true;
                keywords.push(Keyword {
                    name: None,
                    value: self.parse_expr()?,
                });
            } else if matches!(self.cur(), Tok::Name(n) if !is_keyword(n))
                && matches!(&self.toks[self.pos + 1].tok, Tok::Op(o) if o == "=")
            {
                order.keyword = true;
                let kn = self.expect_name()?;
                self.expect_op("=")?;
                let start = self.pos;
                let value = self.parse_expr()?;
                self.comma_hint(start)?;
                keywords.push(Keyword {
                    name: Some(kn),
                    value,
                });
            } else {
                order.positional()?;
                let start = self.pos;
                let e = self.parse_namedexpr()?;
                // Generator expression as sole argument: f(x for x in xs). With
                // anything else in the call it must have its own parentheses.
                if self.at_comp_for() {
                    let comps = self.parse_comprehension_clauses()?;
                    let alone = args.is_empty() && keywords.is_empty() && self.at_op(")");
                    if !alone {
                        return Err(self.err_span(
                            "Generator expression must be parenthesized",
                            start,
                            self.pos - 1,
                        ));
                    }
                    args.push(Expr::GenExp(Box::new(e), comps));
                } else {
                    self.comma_hint(start)?;
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
            let lo_start = p.pos;
            let lo = if p.at_op(":") {
                None
            } else {
                Some(Box::new(p.parse_expr()?))
            };
            if lo.is_some() {
                p.comma_hint(lo_start)?;
            }
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
            Tok::Str(_) | Tok::FString(_, _) | Tok::TString(_, _) | Tok::Bytes(_) => {
                self.parse_string_group()
            }
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
                        let start = self.pos - 1;
                        let e = if self.eat_kw("from") {
                            Expr::YieldFrom(Box::new(self.parse_expr()?))
                        } else if self.at_newline()
                            || self.at_op(")")
                            || self.at_op("=")
                            || self.at_op(";")
                            || matches!(self.cur(), Tok::Eof)
                        {
                            Expr::Yield(None)
                        } else {
                            Expr::Yield(Some(Box::new(self.parse_exprlist()?)))
                        };
                        Ok(self.span_suspension(e, start))
                    }
                    // A reserved word where an atom was expected — a dangling
                    // `except:` / `else:` at statement level, `x = while`, or
                    // `print(pass)`. CPython names none of them: its parser has
                    // no rule left to try and reports the generic message. Every
                    // one of the 27 reserved words was checked against 3.14.6;
                    // all give exactly this.
                    // The keyword itself — already consumed — is what is wrong.
                    _ if is_keyword(&n) => {
                        let _ = line;
                        Err(self.err_span("invalid syntax", self.pos - 1, self.pos - 1))
                    }
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
                _ => Err(self.err_here("invalid syntax")),
            },
            // Any other token (Newline, Op, keyword) where an atom was expected.
            _ => {
                let _ = line;
                Err(self.err_here("invalid syntax"))
            }
        }
    }

    /// Adjacent string literals concatenate (`"a" "b"` -> `"ab"`).
    fn parse_string_group(&mut self) -> Result<Expr, String> {
        let mut parts: Vec<FStrPart> = Vec::new();
        let mut any_f = false;
        let mut any_t = false;
        let mut any_plain = false;
        let mut byte_acc: Option<Vec<u8>> = None;
        loop {
            match self.cur().clone() {
                Tok::Str(s) => {
                    self.advance();
                    any_plain = true;
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
                Tok::TString(raw, is_raw) => {
                    self.advance();
                    any_t = true;
                    let mut sub = self.parse_fstring(&raw, is_raw)?;
                    parts.append(&mut sub);
                }
                _ => break,
            }
        }
        // A group must be all-bytes or all-text, and a t-string joins only other
        // t-strings — the pieces produce different types, so there is nothing to
        // concatenate. Both messages are CPython 3.14.6's, and the t-string check
        // runs first because CPython reports `t'a' b'b'` as the t-string error.
        //
        // pythonrs used to accept every one of these silently and produce a
        // value: `'a' b'b'` evaluated to `b'b'` (the text half was dropped on the
        // floor by the early return below) and `t'a' f'b'` to a `Template`.
        if any_t && (any_f || any_plain || byte_acc.is_some()) {
            return Err(
                "SyntaxError: cannot mix t-string literals with string or bytes literals"
                    .to_string(),
            );
        }
        if byte_acc.is_some() && (any_plain || any_f || any_t) {
            return Err("SyntaxError: cannot mix bytes and nonbytes literals".to_string());
        }
        if let Some(b) = byte_acc {
            return Ok(Expr::Bytes(b));
        }
        if any_t {
            Ok(Expr::TString(parts))
        } else if any_f {
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
            src: expr_src.to_string(),
            conv,
            spec,
        });
        Ok(out)
    }

    /// `(...)` — parenthesized expr, tuple, or generator expression.
    fn parse_paren(&mut self) -> Result<Expr, String> {
        let open = self.pos;
        self.advance(); // (
        if self.eat_op(")") {
            return Ok(Expr::Tuple(Vec::new()));
        }
        let start = self.pos;
        let first = self.parse_star_or_expr()?;
        if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op(")")?;
            return Ok(Expr::GenExp(Box::new(first), comps));
        }
        self.comma_hint(start)?;
        if self.at_op(",") {
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op(")") {
                    break;
                }
                let start = self.pos;
                items.push(self.parse_star_or_expr()?);
                self.comma_hint(start)?;
            }
            self.expect_op(")")?;
            return Ok(Expr::Tuple(items));
        }
        self.expect_op(")")?;
        let (line, col) = self.node_start(start, self.pos - 1);
        self.groups.insert(open, (self.pos - 1, line, col));
        Ok(first)
    }

    /// `[...]` — list display or list comprehension.
    fn parse_list(&mut self) -> Result<Expr, String> {
        self.advance(); // [
        if self.eat_op("]") {
            return Ok(Expr::List(Vec::new()));
        }
        let start = self.pos;
        let first = self.parse_star_or_expr()?;
        if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op("]")?;
            return Ok(Expr::ListComp(Box::new(first), comps));
        }
        self.comma_hint(start)?;
        let mut items = vec![first];
        while self.eat_op(",") {
            if self.at_op("]") {
                break;
            }
            let start = self.pos;
            items.push(self.parse_star_or_expr()?);
            self.comma_hint(start)?;
        }
        self.expect_op("]")?;
        Ok(Expr::List(items))
    }

    /// `{...}` — dict/set display or comprehension, with the whole display as
    /// its caret span. A display raises on an unhashable element (`{[1]: 2}`),
    /// and CPython underlines the display; a comprehension raises from inside
    /// its own hidden function, so it is left unwrapped.
    fn parse_brace(&mut self) -> Result<Expr, String> {
        let (line, start) = (self.line(), self.col());
        let e = self.parse_brace_display()?;
        Ok(match e {
            Expr::Dict(_) | Expr::Set(_) => spanned(e, line, start, self.prev_end_col(), 0, 0),
            other => other,
        })
    }

    fn parse_brace_display(&mut self) -> Result<Expr, String> {
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
        let first_start = self.pos;
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
                let k_start = self.pos;
                let k = self.parse_expr()?;
                if !self.at_op(":") {
                    // `invalid_double_starred_kvpairs`: a key with no value,
                    // located at the key with no end.
                    let t = &self.toks[k_start];
                    return Err(at_pos(
                        "SyntaxError: ':' expected after dictionary key",
                        t.line,
                        t.col as i64 + 1,
                        t.line,
                        0,
                    ));
                }
                self.advance();
                pairs.push((Some(k), self.parse_expr()?));
            }
            self.expect_op("}")?;
            Ok(Expr::Dict(pairs))
        } else if self.at_comp_for() {
            let comps = self.parse_comprehension_clauses()?;
            self.expect_op("}")?;
            Ok(Expr::SetComp(Box::new(first), comps))
        } else {
            self.comma_hint(first_start)?;
            let mut items = vec![first];
            while self.eat_op(",") {
                if self.at_op("}") {
                    break;
                }
                let start = self.pos;
                items.push(self.parse_star_or_expr()?);
                self.comma_hint(start)?;
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

/// `e` without the source-span wrapper the parser puts on some expressions.
fn unspan(e: &Expr) -> &Expr {
    match e {
        Expr::Spanned(inner, _) => unspan(inner),
        other => other,
    }
}

/// What CPython calls an expression in a message (`_PyPegen_get_expr_name`).
fn expr_name(e: &Expr) -> &'static str {
    match unspan(e) {
        Expr::None => "None",
        Expr::True => "True",
        Expr::False => "False",
        Expr::Ellipsis => "ellipsis",
        Expr::Int(_)
        | Expr::BigInt(_)
        | Expr::Float(_)
        | Expr::Complex(_)
        | Expr::Str(_)
        | Expr::Bytes(_) => "literal",
        Expr::FString(_) => "f-string expression",
        Expr::TString(_) => "t-string expression",
        Expr::Name(_) => "name",
        Expr::List(_) => "list",
        Expr::Tuple(_) => "tuple",
        Expr::Set(_) => "set display",
        Expr::Dict(_) => "dict literal",
        Expr::Starred(_) => "starred",
        Expr::Compare(..) => "comparison",
        Expr::IfExp { .. } => "conditional expression",
        Expr::Call { .. } => "function call",
        Expr::Attribute(..) => "attribute",
        Expr::Subscript(..) => "subscript",
        Expr::Slice { .. } => "slice",
        Expr::Lambda { .. } => "lambda",
        Expr::ListComp(..) => "list comprehension",
        Expr::SetComp(..) => "set comprehension",
        Expr::DictComp(..) => "dict comprehension",
        Expr::GenExp(..) => "generator expression",
        Expr::Yield(_) | Expr::YieldFrom(_) => "yield expression",
        Expr::Await(_) => "await expression",
        Expr::NamedExpr(..) => "named expression",
        _ => "expression",
    }
}

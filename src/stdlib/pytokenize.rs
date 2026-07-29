//! `_tokenize` — the tokenizer `tokenize.py` drives.
//!
//! `tokenize.py` is a thin wrapper: it calls `_tokenize.TokenizerIter(readline,
//! extra_tokens=…)` and re-labels each 5-tuple as a `TokenInfo`. Everything that
//! decides what a token IS lives on this side. `traceback` imports `tokenize`, so
//! `logging`, `unittest` and `hashlib` all reach the interpreter through here.
//!
//! This is NOT the compiler's lexer. That one exists to feed a parser: it drops
//! comments, folds an f-string into a single token, and never emits INDENT or
//! DEDENT as values. `tokenize` is a source-fidelity tool — every character of the
//! input has to be attributable to some token, comments and blank lines included,
//! and PEP 701 requires an f-string to come apart into its literal and expression
//! pieces. Sharing one scanner between the two would make each worse.
//!
//! Token type numbers are `token.py`'s, generated from CPython's `Grammar/Tokens`.
//! They are part of the contract: callers compare against `token.NAME`,
//! `token.OP`, `token.FSTRING_START` by number.

use crate::host::{PyHost, PyObj};
use fusevm::Value;

const ENDMARKER: i64 = 0;
const NAME: i64 = 1;
const NUMBER: i64 = 2;
const STRING: i64 = 3;
const NEWLINE: i64 = 4;
const INDENT: i64 = 5;
const DEDENT: i64 = 6;
const OP: i64 = 55;
const FSTRING_START: i64 = 59;
const FSTRING_MIDDLE: i64 = 60;
const FSTRING_END: i64 = 61;
const TSTRING_START: i64 = 62;
const TSTRING_MIDDLE: i64 = 63;
const TSTRING_END: i64 = 64;
const COMMENT: i64 = 65;
const NL: i64 = 66;
const ERRORTOKEN: i64 = 67;

/// Operators longest-first, so `**=` is never mis-scanned as `**` then `=`.
const OPERATORS: &[&str] = &[
    "**=", "//=", ">>=", "<<=", "...", "!=", ">=", "<=", "==", "->", ":=", "+=", "-=", "*=", "/=",
    "%=", "&=", "|=", "^=", "@=", "**", "//", ">>", "<<", "+", "-", "*", "/", "%", "@", "&", "|",
    "^", "~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":", ".", ";", "=", "!",
];

/// One emitted token: type, text, (start row, col), (end row, col), source line.
struct Tok {
    kind: i64,
    text: String,
    start: (usize, usize),
    end: (usize, usize),
    line: String,
}

/// What the scanner is in the middle of.
///
/// PEP 701 makes f-strings recursive: the expression inside `{…}` is ordinary
/// Python, which may itself contain an f-string. A stack — rather than a flag —
/// is what makes `f"{f'{x}'}"` fall out for free.
enum Mode {
    /// Ordinary code. `depth` counts the `([{` opened since this frame began; a
    /// `}` at depth 0 closes the enclosing replacement field rather than being an
    /// operator, and the same is true of the `:` that starts a format spec.
    Code { depth: usize, in_field: bool },
    /// The literal run of an f-string or t-string.
    Str(StrCtx),
    /// The literal run of a format spec, which ends at the field's `}`.
    Spec(StrCtx),
}

/// The literal-scanning context shared by an f-string body and a format spec.
struct StrCtx {
    quote: char,
    triple: bool,
    /// t-strings emit their own token types; everything else is identical.
    template: bool,
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || !c.is_ascii()
}

fn ident_continue(c: char) -> bool {
    ident_start(c) || c.is_ascii_digit()
}

/// A string prefix (`r`, `b`, `f`, `t`, `u` and the two-letter combinations),
/// case-insensitive, as CPython accepts them.
fn string_prefix(s: &str) -> Option<(bool, bool)> {
    let lower = s.to_ascii_lowercase();
    let (interpolated, template) = match lower.as_str() {
        "r" | "b" | "u" | "rb" | "br" => (false, false),
        "f" | "rf" | "fr" => (true, false),
        "t" | "rt" | "tr" => (true, true),
        _ => return None,
    };
    Some((interpolated, template))
}

/// The scanner: a cursor over the source lines plus the mode stack.
struct Scanner {
    lines: Vec<String>,
    /// Each line pre-split into characters, since every position is a code-point
    /// index and the source is walked character by character.
    chars: Vec<Vec<char>>,
    row: usize,
    col: usize,
    out: Vec<Tok>,
    modes: Vec<Mode>,
    indents: Vec<usize>,
    extra: bool,
    /// Whether the logical line so far holds anything but whitespace/comment; a
    /// line without content ends in NL rather than NEWLINE.
    content: bool,
    continued: bool,
}

impl Scanner {
    fn new(src: &str, extra: bool) -> Self {
        let lines = split_lines(src);
        let chars = lines.iter().map(|l| l.chars().collect()).collect();
        Scanner {
            lines,
            chars,
            row: 0,
            col: 0,
            out: Vec::new(),
            modes: vec![Mode::Code {
                depth: 0,
                in_field: false,
            }],
            indents: vec![0],
            extra,
            content: false,
            continued: false,
        }
    }

    fn at_end(&self) -> bool {
        self.row >= self.lines.len()
    }

    fn cur(&self) -> Option<char> {
        self.chars.get(self.row)?.get(self.col).copied()
    }

    fn peek(&self, n: usize) -> Option<char> {
        self.chars.get(self.row)?.get(self.col + n).copied()
    }

    fn line_len(&self) -> usize {
        self.chars.get(self.row).map(|c| c.len()).unwrap_or(0)
    }

    /// The `line` field for a token spanning `(srow..=erow)`: the source lines it
    /// covers, in full and joined — which for a single-line token is just that
    /// line, and for a multi-line string is the whole span.
    fn span_line(&self, srow: usize, erow: usize) -> String {
        if srow == erow {
            return self.lines.get(srow).cloned().unwrap_or_default();
        }
        self.lines[srow..=erow.min(self.lines.len() - 1)].concat()
    }

    fn push(&mut self, kind: i64, text: String, start: (usize, usize), end: (usize, usize)) {
        let line = self.span_line(start.0, end.0);
        self.out.push(Tok {
            kind,
            text,
            start: (start.0 + 1, start.1),
            end: (end.0 + 1, end.1),
            line,
        });
    }

    /// Advance one character, wrapping to the next line.
    fn bump(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        }
        if self.col >= self.line_len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn depth(&self) -> usize {
        match self.modes.last() {
            Some(Mode::Code { depth, .. }) => *depth,
            _ => 0,
        }
    }

    /// Whether the scanner is anywhere inside an f-string, at any nesting level.
    /// Leading whitespace produces no INDENT there, and a `#` is literal text.
    fn in_fstring(&self) -> bool {
        self.modes
            .iter()
            .any(|m| matches!(m, Mode::Str(_) | Mode::Spec(_)))
    }
}

/// Tokenize `src` into CPython's token stream.
///
/// `extra_tokens` is `tokenize`'s name for "also report what a parser would throw
/// away": COMMENT, NL, and the trailing ENDMARKER.
fn tokenize(src: &str, extra_tokens: bool) -> Result<Vec<Tok>, String> {
    let mut s = Scanner::new(src, extra_tokens);
    while !s.at_end() {
        match s.modes.last() {
            Some(Mode::Str(_)) => scan_literal(&mut s, false)?,
            Some(Mode::Spec(_)) => scan_literal(&mut s, true)?,
            _ => scan_code(&mut s)?,
        }
    }
    finish(&mut s);
    Ok(s.out)
}

/// Scan in ordinary-code mode until the mode changes or the line runs out.
fn scan_code(s: &mut Scanner) -> Result<(), String> {
    // Start of a physical line at statement level: measure indentation.
    if s.col == 0 && s.depth() == 0 && !s.continued && !s.in_fstring() {
        measure_indent(s)?;
    }
    s.continued = false;

    while !s.at_end() {
        if !matches!(s.modes.last(), Some(Mode::Code { .. })) {
            return Ok(());
        }
        let Some(c) = s.cur() else {
            s.row += 1;
            s.col = 0;
            return Ok(());
        };
        let (row, col) = (s.row, s.col);
        match c {
            ' ' | '\t' | '\x0c' => s.col += 1,
            '\r' | '\n' => {
                let text: String = s.chars[row][col..].iter().collect();
                let end = s.line_len();
                let in_field = matches!(s.modes.last(), Some(Mode::Code { in_field: true, .. }));
                let kind = if s.depth() > 0 || in_field || !s.content {
                    NL
                } else {
                    NEWLINE
                };
                if kind == NEWLINE || s.extra {
                    s.push(kind, text, (row, col), (row, end));
                }
                s.content = false;
                s.row += 1;
                s.col = 0;
                return Ok(());
            }
            '#' => {
                let stop = s.line_len()
                    - s.chars[row][col..]
                        .iter()
                        .rev()
                        .take_while(|c| **c == '\n' || **c == '\r')
                        .count();
                if s.extra {
                    let text: String = s.chars[row][col..stop].iter().collect();
                    s.push(COMMENT, text, (row, col), (row, stop));
                }
                s.col = stop;
            }
            '\\' if s.peek(1).map_or(true, |n| n == '\n' || n == '\r') => {
                s.continued = true;
                s.row += 1;
                s.col = 0;
                return Ok(());
            }
            _ if c.is_ascii_digit()
                || (c == '.' && s.peek(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                let end = scan_number(&s.chars[row], col);
                let text: String = s.chars[row][col..end].iter().collect();
                s.push(NUMBER, text, (row, col), (row, end));
                s.col = end;
                s.content = true;
            }
            '"' | '\'' => scan_string_start(s, "")?,
            _ if ident_start(c) => {
                let mut end = col;
                while end < s.line_len() && ident_continue(s.chars[row][end]) {
                    end += 1;
                }
                let word: String = s.chars[row][col..end].iter().collect();
                // A string prefix belongs to the literal, not to a name of its own.
                if string_prefix(&word).is_some()
                    && matches!(s.chars[row].get(end), Some('"') | Some('\''))
                {
                    s.col = end;
                    scan_string_start(s, &word)?;
                    continue;
                }
                s.push(NAME, word, (row, col), (row, end));
                s.col = end;
                s.content = true;
            }
            _ => {
                let rest: String = s.chars[row][col..].iter().collect();
                match OPERATORS.iter().find(|op| rest.starts_with(**op)) {
                    Some(op) => {
                        let n = op.chars().count();
                        // A `}` or `:` at depth 0 inside a replacement field is
                        // structural, not an operator: it closes the field or
                        // opens the format spec.
                        if s.depth() == 0
                            && matches!(s.modes.last(), Some(Mode::Code { in_field: true, .. }))
                        {
                            if *op == "}" {
                                s.push(OP, "}".into(), (row, col), (row, col + 1));
                                s.col += 1;
                                s.modes.pop();
                                return Ok(());
                            }
                            if *op == ":" {
                                s.push(OP, ":".into(), (row, col), (row, col + 1));
                                s.col += 1;
                                let ctx = enclosing_ctx(s);
                                s.modes.push(Mode::Spec(ctx));
                                return Ok(());
                            }
                        }
                        match *op {
                            "(" | "[" | "{" => bump_depth(s, 1),
                            ")" | "]" | "}" => bump_depth(s, -1),
                            _ => {}
                        }
                        s.push(OP, (*op).to_string(), (row, col), (row, col + n));
                        s.col += n;
                        s.content = true;
                    }
                    None => {
                        s.push(ERRORTOKEN, c.to_string(), (row, col), (row, col + 1));
                        s.col += 1;
                        s.content = true;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The f-string context a nested format spec belongs to: quote style and whether
/// it is a template, both inherited from the literal that opened the field.
fn enclosing_ctx(s: &Scanner) -> StrCtx {
    for m in s.modes.iter().rev() {
        if let Mode::Str(c) | Mode::Spec(c) = m {
            return StrCtx {
                quote: c.quote,
                triple: c.triple,
                template: c.template,
            };
        }
    }
    StrCtx {
        quote: '"',
        triple: false,
        template: false,
    }
}

fn bump_depth(s: &mut Scanner, delta: i64) {
    if let Some(Mode::Code { depth, .. }) = s.modes.last_mut() {
        *depth = if delta > 0 {
            *depth + 1
        } else {
            depth.saturating_sub(1)
        };
    }
}

/// Handle a string literal opening at the cursor. A plain string is scanned whole
/// and emitted as one STRING; an f-string emits FSTRING_START and pushes a
/// literal-scanning mode.
fn scan_string_start(s: &mut Scanner, prefix: &str) -> Result<(), String> {
    let (row, col) = (s.row, s.col);
    let start = col - prefix.chars().count();
    let quote = s.cur().ok_or("SyntaxError: unterminated string literal")?;
    let triple = s.peek(1) == Some(quote) && s.peek(2) == Some(quote);
    let qlen = if triple { 3 } else { 1 };
    let (interpolated, template) = string_prefix(prefix).unwrap_or((false, false));

    if !interpolated {
        let (erow, ecol) = scan_plain_string(s, row, col, quote, triple)
            .ok_or("SyntaxError: unterminated string literal")?;
        let text = slice_span(&s.chars, row, start, erow, ecol);
        s.push(STRING, text, (row, start), (erow, ecol));
        s.row = erow;
        s.col = ecol;
        if s.col >= s.line_len() {
            s.row += 1;
            s.col = 0;
        }
        s.content = true;
        return Ok(());
    }

    let open: String = s.chars[row][start..col + qlen].iter().collect();
    let kind = if template {
        TSTRING_START
    } else {
        FSTRING_START
    };
    s.push(kind, open, (row, start), (row, col + qlen));
    s.col = col + qlen;
    s.content = true;
    s.modes.push(Mode::Str(StrCtx {
        quote,
        triple,
        template,
    }));
    Ok(())
}

/// Scan the literal run of an f-string body (`spec` false) or of a format spec
/// (`spec` true), emitting FSTRING_MIDDLE for the text between structural marks.
fn scan_literal(s: &mut Scanner, spec: bool) -> Result<(), String> {
    let (quote, triple, template) = match s.modes.last() {
        Some(Mode::Str(c)) | Some(Mode::Spec(c)) => (c.quote, c.triple, c.template),
        _ => return Ok(()),
    };
    let middle_kind = if template {
        TSTRING_MIDDLE
    } else {
        FSTRING_MIDDLE
    };
    let end_kind = if template { TSTRING_END } else { FSTRING_END };

    let (srow, scol) = (s.row, s.col);
    let mut text = String::new();
    loop {
        if s.at_end() {
            return Err("SyntaxError: unterminated f-string literal".into());
        }
        let Some(c) = s.cur() else {
            // End of a physical line inside a triple-quoted literal: the newline
            // is part of the literal text.
            s.row += 1;
            s.col = 0;
            continue;
        };
        match c {
            // An escape carries its next character along, whatever it is.
            '\\' => {
                text.push(c);
                s.bump();
                if let Some(n) = s.cur() {
                    text.push(n);
                    s.bump();
                }
            }
            // `{{`/`}}` are one literal brace. CPython reports the token as
            // spanning only the FIRST of the pair.
            '{' if s.peek(1) == Some('{') => {
                let (r, c0) = (s.row, s.col);
                s.push(middle_kind, "{".into(), (r, c0), (r, c0 + 1));
                s.col += 2;
                return Ok(());
            }
            '}' if s.peek(1) == Some('}') => {
                let (r, c0) = (s.row, s.col);
                s.push(middle_kind, "}".into(), (r, c0), (r, c0 + 1));
                s.col += 2;
                return Ok(());
            }
            '{' => {
                if !text.is_empty() {
                    let (r, c0) = (s.row, s.col);
                    s.push(middle_kind, text, (srow, scol), (r, c0));
                }
                let (r, c0) = (s.row, s.col);
                s.push(OP, "{".into(), (r, c0), (r, c0 + 1));
                s.col += 1;
                s.modes.push(Mode::Code {
                    depth: 0,
                    in_field: true,
                });
                return Ok(());
            }
            // In spec mode a `}` closes the whole replacement field: flush the
            // literal run — even an empty one, which is what CPython emits — then
            // leave both the spec and the field's code frame.
            '}' if spec => {
                let (r, c0) = (s.row, s.col);
                s.push(middle_kind, text, (srow, scol), (r, c0));
                s.push(OP, "}".into(), (r, c0), (r, c0 + 1));
                s.col += 1;
                s.modes.pop();
                s.modes.pop();
                return Ok(());
            }
            _ if c == quote && !spec => {
                let closes = if triple {
                    s.peek(1) == Some(quote) && s.peek(2) == Some(quote)
                } else {
                    true
                };
                if closes {
                    let (r, c0) = (s.row, s.col);
                    if !text.is_empty() {
                        s.push(middle_kind, text, (srow, scol), (r, c0));
                    }
                    let n = if triple { 3 } else { 1 };
                    let close: String = std::iter::repeat(quote).take(n).collect();
                    s.push(end_kind, close, (r, c0), (r, c0 + n));
                    s.col += n;
                    if s.col >= s.line_len() {
                        s.row += 1;
                        s.col = 0;
                    }
                    s.modes.pop();
                    return Ok(());
                }
                text.push(c);
                s.bump();
            }
            _ => {
                text.push(c);
                s.bump();
            }
        }
    }
}

/// Measure the indentation of the line at the cursor, emitting INDENT/DEDENT.
fn measure_indent(s: &mut Scanner) -> Result<(), String> {
    let row = s.row;
    let mut width = 0usize;
    let mut col = 0usize;
    while col < s.line_len() {
        match s.chars[row][col] {
            ' ' => width += 1,
            // A tab advances to the next multiple of 8, which is how CPython
            // compares indentation widths.
            '\t' => width = width / 8 * 8 + 8,
            '\x0c' => width = 0,
            _ => break,
        }
        col += 1;
    }
    // A blank or comment-only line changes no indentation at all.
    if col >= s.line_len() || matches!(s.chars[row][col], '#' | '\n' | '\r') {
        s.col = col;
        return Ok(());
    }
    if width > *s.indents.last().unwrap() {
        s.indents.push(width);
        let text: String = s.chars[row][..col].iter().collect();
        s.push(INDENT, text, (row, 0), (row, col));
    } else {
        while width < *s.indents.last().unwrap() {
            s.indents.pop();
            s.push(DEDENT, String::new(), (row, col), (row, col));
        }
        if width != *s.indents.last().unwrap() {
            return Err(format!(
                "IndentationError: unindent does not match any outer indentation level (line {})",
                row + 1
            ));
        }
    }
    s.col = col;
    Ok(())
}

/// Close the stream: a DEDENT per open level, then ENDMARKER.
fn finish(s: &mut Scanner) {
    let last = s.lines.len();
    while s.indents.len() > 1 {
        s.indents.pop();
        s.out.push(Tok {
            kind: DEDENT,
            text: String::new(),
            start: (last + 1, 0),
            end: (last + 1, 0),
            line: String::new(),
        });
    }
    if s.extra {
        s.out.push(Tok {
            kind: ENDMARKER,
            text: String::new(),
            start: (last + 1, 0),
            end: (last + 1, 0),
            line: String::new(),
        });
    }
}

/// Find the end of a non-interpolated string literal opening at `(row, col)`.
fn scan_plain_string(
    s: &Scanner,
    row: usize,
    col: usize,
    quote: char,
    triple: bool,
) -> Option<(usize, usize)> {
    let qlen = if triple { 3 } else { 1 };
    let mut r = row;
    let mut c = col + qlen;
    loop {
        let cur = s.chars.get(r)?;
        if c >= cur.len() {
            // A single-quoted literal may not cross a line break; an escaped
            // break was already consumed by the `\\` arm below.
            if !triple {
                return None;
            }
            r += 1;
            c = 0;
            continue;
        }
        match cur[c] {
            '\\' => c += 2,
            ch if ch == quote => {
                if !triple {
                    return Some((r, c + 1));
                }
                if cur.get(c + 1) == Some(&quote) && cur.get(c + 2) == Some(&quote) {
                    return Some((r, c + 3));
                }
                c += 1;
            }
            _ => c += 1,
        }
    }
}

/// The source between two (row, col) positions, lines joined as written.
fn slice_span(chars: &[Vec<char>], srow: usize, scol: usize, erow: usize, ecol: usize) -> String {
    if srow == erow {
        return chars[srow][scol..ecol].iter().collect();
    }
    let mut s: String = chars[srow][scol..].iter().collect();
    for l in &chars[srow + 1..erow] {
        s.extend(l.iter());
    }
    s.extend(chars[erow][..ecol].iter());
    s
}

/// Scan a numeric literal, returning the index just past it. Accepts the radix
/// prefixes, underscores, exponents, and the imaginary suffix.
fn scan_number(chars: &[char], mut i: usize) -> usize {
    let n = chars.len();
    if chars[i] == '0' && i + 1 < n && matches!(chars[i + 1], 'x' | 'X' | 'o' | 'O' | 'b' | 'B') {
        i += 2;
        while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        return i;
    }
    while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
        i += 1;
    }
    if i < n && chars[i] == '.' {
        i += 1;
        while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
            i += 1;
        }
    }
    if i < n && matches!(chars[i], 'e' | 'E') {
        let mut j = i + 1;
        if j < n && matches!(chars[j], '+' | '-') {
            j += 1;
        }
        if j < n && chars[j].is_ascii_digit() {
            i = j;
            while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
                i += 1;
            }
        }
    }
    if i < n && matches!(chars[i], 'j' | 'J') {
        i += 1;
    }
    i
}

/// Split `src` into lines, each KEEPING its terminator — token positions and the
/// `line` field both quote the source verbatim.
fn split_lines(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in src.chars() {
        cur.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Build the 5-tuple `tokenize.TokenInfo._make` expects: `(type, string, (srow,
/// scol), (erow, ecol), line)`.
fn tok_tuple(h: &mut PyHost, t: &Tok) -> Value {
    let text = h.new_str(t.text.clone());
    let line = h.new_str(t.line.clone());
    let start = h.new_tuple(vec![
        Value::Int(t.start.0 as i64),
        Value::Int(t.start.1 as i64),
    ]);
    let end = h.new_tuple(vec![Value::Int(t.end.0 as i64), Value::Int(t.end.1 as i64)]);
    h.new_tuple(vec![Value::Int(t.kind), text, start, end, line])
}

/// `_tokenize.TokenizerIter(readline, encoding=None, extra_tokens=False)`.
///
/// CPython's tokenizer pulls lines lazily from `readline`; this one drains the
/// source first and scans it whole. The difference is observable only for a
/// `readline` with side effects past the first error, which `tokenize` never does.
pub fn tokenizer_iter(
    h: &mut PyHost,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let extra = kwargs
        .iter()
        .find(|(k, _)| k == "extra_tokens")
        .map(|(_, v)| h.truthy(v))
        .unwrap_or(false);
    let src = args
        .first()
        .and_then(|v| h.as_str(v))
        .ok_or_else(|| crate::host::type_error("TokenizerIter() expected a string"))?;
    let toks = tokenize(&src, extra)?;
    let items: Vec<Value> = toks.iter().map(|t| tok_tuple(h, t)).collect();
    Ok(h.new_list(items))
}

/// Read every line `readline` will produce and join them. `tokenize` hands us a
/// file's `readline`, and CPython's C tokenizer drains it the same way.
pub fn drain_readline(readline: &Value) -> Result<String, String> {
    let mut src = String::new();
    loop {
        let line = crate::host::invoke(readline, vec![], vec![])?;
        match crate::host::with_host(|h| h.as_str(&line)) {
            Some(s) if !s.is_empty() => src.push_str(&s),
            // An empty string (or a non-string) ends the stream, per the
            // `readline` protocol.
            _ => break,
        }
    }
    Ok(src)
}

/// The `_tokenize` namespace: one class, which `tokenize.py` instantiates.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    vec![(
        "TokenizerIter".to_string(),
        h.alloc(PyObj::Builtin("_tokenize.TokenizerIter".into())),
    )]
}

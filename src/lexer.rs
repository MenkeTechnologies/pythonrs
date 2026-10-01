//! Python tokenizer.
//!
//! Turns source into a flat token stream with the significant-indentation
//! contract CPython uses: logical lines end in `Newline`, and a change in
//! leading indentation emits `Indent`/`Dedent` (skipped inside brackets and on
//! blank/comment-only lines). Bracket depth (`()[]{}`) and backslash-newline
//! suppress newlines for implicit/explicit line continuation. f-strings are
//! emitted as a single `FString` token carrying the raw inner text; the parser
//! expands the `{...}` fields with the expression grammar.

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Name(String),
    Int(i64),
    /// Integer literal too wide for `i64`, kept as decimal text.
    BigInt(String),
    Float(f64),
    /// Imaginary literal (`3j`) — the real magnitude; host builds the complex.
    Complex(f64),
    Str(String),
    Bytes(Vec<u8>),
    /// `(raw_inner_text, is_raw)` for an f-string; fields parsed by the parser.
    FString(String, bool),
    /// PEP 750 template string (`t"..."`); same body syntax as an f-string.
    TString(String, bool),
    /// An operator or delimiter, e.g. `+`, `==`, `**`, `(`, `:`, `,`, `->`.
    Op(String),
    Newline,
    Indent,
    Dedent,
    Eof,
}

/// A token plus its 1-based source line and 0-based character columns (start
/// inclusive, end exclusive) within that line. Columns feed traceback carets;
/// they are only meaningful for on-line tokens (structural Newline/Indent/Dedent/
/// Eof carry stale columns that no consumer reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
    pub end_col: u32,
}

/// A tokenized module: the stream, plus an indentation error the parser is to
/// report only if it consumes that far. See [`Lexed::deferred`].
pub struct Lexed {
    pub toks: Vec<Token>,
    /// An `unindent does not match any outer indentation level` found while
    /// tokenizing, held back rather than raised.
    ///
    /// CPython's tokenizer is pulled lazily by the parser, so a syntax error on
    /// an EARLIER line wins over a bad dedent on a later one:
    ///
    /// ```text
    /// match -3:
    ///         print('bad')     <- line 2: not a `case`, SyntaxError
    ///     case _:              <- line 3: dedent matches nothing
    /// ```
    ///
    /// CPython reports `SyntaxError: invalid syntax` at line 2. pythonrs
    /// tokenizes the whole module up front, so the line-3 tokenizer error used
    /// to pre-empt it with `IndentationError`. Tokenizing now STOPS at the bad
    /// dedent and parks the message here: an earlier parse error surfaces
    /// first, and if the truncated stream parses cleanly the parser raises this
    /// instead — which restores the original error for the case where the
    /// dedent really is the only problem.
    pub deferred: Option<String>,
    /// A bracket still open at end of input: `'(' was never closed`, pointing
    /// at the bracket. The parser reports it when it runs out of input there,
    /// and its own error otherwise.
    pub unclosed: Option<String>,
}

struct Lexer {
    src: Vec<char>,
    pos: usize,
    line: u32,
    depth: i32,
    indents: Vec<usize>,
    out: Vec<Token>,
    /// See [`Lexed::deferred`]. Set by `handle_indent`; stops `run`.
    deferred: Option<String>,
    /// Char index where the current line begins — subtracted from a token's
    /// start/end char index to get its 0-based character column (for carets).
    line_start: usize,
    /// Char index where the token currently being scanned begins.
    tok_start: usize,
    /// The brackets currently open: the character, its line, and its 0-based
    /// column — what `'(' was never closed` and a mismatched closer name.
    open: Vec<(char, u32, u32)>,
}

/// CPython's `MAXLEVEL` (`Parser/lexer/state.h`): the tokenizer tracks at most
/// 200 simultaneously-open brackets, counting `(`, `[` and `{` together.
const MAX_PAREN_DEPTH: i32 = 200;

/// Multi-char operators, longest first so the scanner is greedy.
const OPS3: &[&str] = &["**=", "//=", ">>=", "<<=", "...", "!=="];
const OPS2: &[&str] = &[
    "**", "//", ">>", "<<", "<=", ">=", "==", "!=", "->", ":=", "+=", "-=", "*=", "/=", "%=", "&=",
    "|=", "^=", "@=",
];

/// Tokenize `src` into a token stream ending in `Eof`.
pub fn lex(src: &str) -> Result<Lexed, String> {
    let mut lx = Lexer {
        src: src.chars().collect(),
        pos: 0,
        line: 1,
        depth: 0,
        indents: vec![0],
        out: Vec::new(),
        deferred: None,
        line_start: 0,
        tok_start: 0,
        open: Vec::new(),
    };
    lx.run()?;
    let unclosed = lx.open.first().map(|&(c, line, col)| {
        // The line the bracket is on, newline-terminated only when nothing but
        // blank lines follows it — as CPython's tokenizer leaves its buffer.
        let rest_blank = src
            .split_inclusive('\n')
            .skip(line as usize)
            .all(|l| l.trim().is_empty());
        let mut text = lx.line_text(line);
        if rest_blank {
            text.push('\n');
        }
        crate::parser::with_text(
            crate::parser::at_pos(
                &format!("SyntaxError: '{c}' was never closed"),
                line,
                col as i64 + 1,
                line,
                0,
            ),
            &text,
        )
    });
    Ok(Lexed {
        toks: lx.out,
        deferred: lx.deferred,
        unclosed,
    })
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.src.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.src.get(self.pos).copied();
        if let Some(ch) = c {
            self.pos += 1;
            if ch == '\n' {
                self.line += 1;
                self.line_start = self.pos;
            }
        }
        c
    }
    /// Source line `line` (1-based), without its newline.
    fn line_text(&self, line: u32) -> String {
        let mut cur = 1;
        let mut out = String::new();
        for &c in &self.src {
            if c == '\n' {
                if cur == line {
                    break;
                }
                cur += 1;
                continue;
            }
            if cur == line {
                out.push(c);
            }
        }
        out
    }

    /// A tokenizer error at `line`/`col` (0-based column), carrying the line's
    /// text without its newline, as CPython's tokenizer reports it.
    fn tok_err(&self, msg: &str, line: u32, col: usize, end_col: i64) -> String {
        crate::parser::with_text(
            crate::parser::at_pos(msg, line, col as i64 + 1, line, end_col),
            &self.line_text(line),
        )
    }

    fn push(&mut self, tok: Tok) {
        self.out.push(Token {
            tok,
            line: self.line,
            col: (self.tok_start.saturating_sub(self.line_start)) as u32,
            end_col: (self.pos.saturating_sub(self.line_start)) as u32,
        });
    }

    fn run(&mut self) -> Result<(), String> {
        let mut at_line_start = true;
        loop {
            if at_line_start && self.depth == 0 {
                let blank = self.handle_indent()?;
                if self.deferred.is_some() {
                    // Bad dedent: stop here so the parser sees only the lines
                    // before it. See `Lexed::deferred`.
                    break;
                }
                if blank {
                    // Blank/comment-only line consumed; stay at line start.
                    continue;
                }
                at_line_start = false;
            }
            match self.peek() {
                None => break,
                Some('\n') => {
                    // A Newline token sits where the line break is, so an error
                    // reported against it names this line, not the next.
                    let (line, col) = (self.line, (self.pos - self.line_start) as u32);
                    self.bump();
                    if self.depth == 0 {
                        // Collapse runs of blank physical lines to one Newline.
                        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                            self.out.push(Token {
                                tok: Tok::Newline,
                                line,
                                col,
                                end_col: col + 1,
                            });
                        }
                        at_line_start = true;
                    }
                }
                Some(c) if c == ' ' || c == '\t' || c == '\r' => {
                    self.bump();
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('\\') if self.peek2() == Some('\n') => {
                    self.bump();
                    self.bump();
                }
                Some('\\') if self.peek2() == Some('\r') => {
                    self.bump();
                    self.bump();
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                }
                Some(_) => self.scan_token()?,
            }
        }
        // Terminate a trailing logical line. A stream cut short by a bad dedent
        // already ends in the Newline of the last complete line followed by the
        // Dedents that line closed, so appending another Newline there would put
        // one AFTER the Dedents and derail the parser.
        if self.deferred.is_none()
            && !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None)
        {
            self.tok_start = self.pos;
            self.push(Tok::Newline);
        }
        while self.indents.len() > 1 {
            self.indents.pop();
            self.push(Tok::Dedent);
        }
        self.push(Tok::Eof);
        Ok(())
    }

    /// Measure a fresh logical line's indentation and emit Indent/Dedent.
    /// Returns true if the line was blank or comment-only (skip it entirely).
    fn handle_indent(&mut self) -> Result<bool, String> {
        let mut col = 0usize;
        let start = self.pos;
        loop {
            match self.peek() {
                Some(' ') => {
                    col += 1;
                    self.pos += 1;
                }
                Some('\t') => {
                    col += 8 - (col % 8);
                    self.pos += 1;
                }
                Some('\r') => {
                    self.pos += 1;
                }
                _ => break,
            }
        }
        match self.peek() {
            None => return Ok(false),
            Some('\n') => {
                self.bump();
                return Ok(true);
            }
            Some('#') => {
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.pos += 1;
                }
                return Ok(true);
            }
            _ => {}
        }
        let top = *self.indents.last().unwrap();
        if col > top {
            self.indents.push(col);
            self.push(Tok::Indent);
        } else if col < top {
            while col < *self.indents.last().unwrap() {
                self.indents.pop();
                self.push(Tok::Dedent);
            }
            if col != *self.indents.last().unwrap() {
                // CPython positions it just past the end of the offending line
                // (offset = its length + 1, no end offset).
                let len = self.src[start..]
                    .iter()
                    .take_while(|c| !matches!(c, '\n' | '\r'))
                    .count();
                self.deferred = Some(crate::parser::at_pos(
                    "IndentationError: unindent does not match any outer indentation level",
                    self.line,
                    len as i64 + 1,
                    self.line,
                    -1,
                ));
                return Ok(false);
            }
        }
        Ok(false)
    }

    fn scan_token(&mut self) -> Result<(), String> {
        // Record where this token begins so `push` can derive its column. The
        // scan_* helpers each emit exactly one token, so this start holds until
        // the corresponding `push`.
        self.tok_start = self.pos;
        let c = self.peek().unwrap();
        // String / prefixed string / f-string / bytes.
        if c == '"' || c == '\'' {
            return self.scan_string(String::new());
        }
        if c.is_ascii_alphabetic() || c == '_' {
            // Distinguish a string prefix (r, b, f, u and combos) from an ident.
            if let Some(consumed) = self.try_string_prefix()? {
                return self.scan_string(consumed);
            }
            return self.scan_name();
        }
        // PEP 3131: an identifier may start with any Unicode letter (`é`, `π`,
        // `变量`). Only ASCII was accepted, so every non-ASCII name was a bare
        // `SyntaxError: invalid syntax`.
        if c.is_alphabetic() {
            return self.scan_name();
        }
        if c.is_ascii_digit()
            || (c == '.' && self.peek2().map(|d| d.is_ascii_digit()).unwrap_or(false))
        {
            return self.scan_number();
        }
        self.scan_op()
    }

    /// If the identifier at `pos` is a string prefix immediately followed by a
    /// quote, consume it and return the lowercased prefix; else return None.
    fn try_string_prefix(&mut self) -> Result<Option<String>, String> {
        let save = self.pos;
        let mut pre = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() && pre.len() < 2 {
                pre.push(c.to_ascii_lowercase());
                self.pos += 1;
            } else {
                break;
            }
        }
        let is_prefix = matches!(
            pre.as_str(),
            "r" | "b" | "f" | "u" | "t" | "rb" | "br" | "rf" | "fr" | "bf" | "fb" | "rt" | "tr"
        );
        if is_prefix && matches!(self.peek(), Some('"') | Some('\'')) {
            Ok(Some(pre))
        } else {
            self.pos = save;
            Ok(None)
        }
    }

    fn scan_name(&mut self) -> Result<(), String> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || crate::builtins::is_other_id_continue(c) {
                s.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        self.push(Tok::Name(s));
        Ok(())
    }

    fn scan_string(&mut self, prefix: String) -> Result<(), String> {
        let is_raw = prefix.contains('r');
        let is_bytes = prefix.contains('b');
        let is_t = prefix.contains('t');
        // A t-string body scans exactly like an f-string body: same replacement
        // fields, same PEP 701 nesting, same `{{`/`}}` escapes. Only what the
        // parser BUILDS from it differs.
        let is_f = prefix.contains('f') || is_t;
        // Where the literal starts — prefix included — which is what an
        // unterminated one is reported against.
        let (start_line, start_col) = (self.line, self.tok_start - self.line_start);
        let quote = self.bump().unwrap();
        let triple = self.peek() == Some(quote) && self.peek2() == Some(quote);
        if triple {
            self.bump();
            self.bump();
        }
        // `_PyTokenizer_syntaxerror`'s wording, naming the kind of literal and
        // the line the tokenizer had reached.
        let unterminated = |lx: &Self| {
            let kind = match (triple, is_t, is_f) {
                (true, true, _) => "triple-quoted t-string",
                (true, false, true) => "triple-quoted f-string",
                (true, false, false) => "triple-quoted string",
                (false, true, _) => "t-string",
                (false, false, true) => "f-string",
                (false, false, false) => "string",
            };
            // A final newline ends the last line rather than starting another,
            // so input that runs out there was detected on the line before.
            let detected = if lx.pos >= lx.src.len() && lx.src.last() == Some(&'\n') {
                lx.line - 1
            } else {
                lx.line
            };
            lx.tok_err(
                &format!("SyntaxError: unterminated {kind} literal (detected at line {detected})"),
                start_line,
                start_col,
                start_col as i64 + 1,
            )
        };
        let mut raw = String::new();
        // For an f-string, a quote inside a `{…}` replacement field does not end
        // the literal (PEP 701 nested strings), so track replacement-field depth.
        let mut brace_depth: i32 = 0;
        loop {
            // Inside an f-string replacement field, a quote opens a NESTED string
            // literal (same or different quotes); scan it whole so its content —
            // including the outer quote char — never terminates the outer f-string.
            if is_f && brace_depth > 0 {
                if let Some(q) = self.peek() {
                    if q == '\'' || q == '"' {
                        raw.push(q);
                        self.bump();
                        let nested_triple = self.peek() == Some(q) && self.peek2() == Some(q);
                        if nested_triple {
                            raw.push(q);
                            raw.push(q);
                            self.bump();
                            self.bump();
                        }
                        loop {
                            match self.peek() {
                                None => {
                                    return Err(format!(
                                        "SyntaxError: unterminated string (line {})",
                                        self.line
                                    ))
                                }
                                Some('\\') => {
                                    raw.push('\\');
                                    self.bump();
                                    if let Some(n) = self.peek() {
                                        raw.push(n);
                                        self.bump();
                                    }
                                }
                                Some(c) if c == q => {
                                    if nested_triple {
                                        if self.peek2() == Some(q)
                                            && self.src.get(self.pos + 2).copied() == Some(q)
                                        {
                                            raw.push(q);
                                            raw.push(q);
                                            raw.push(q);
                                            self.bump();
                                            self.bump();
                                            self.bump();
                                            break;
                                        }
                                        raw.push(c);
                                        self.bump();
                                    } else {
                                        raw.push(q);
                                        self.bump();
                                        break;
                                    }
                                }
                                Some(c) => {
                                    if c == '\n' {
                                        self.line += 1;
                                    }
                                    raw.push(c);
                                    self.bump();
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            match self.peek() {
                None => return Err(unterminated(self)),
                Some(c) if c == quote => {
                    if triple {
                        if self.peek2() == Some(quote)
                            && self.src.get(self.pos + 2).copied() == Some(quote)
                        {
                            self.bump();
                            self.bump();
                            self.bump();
                            break;
                        } else {
                            raw.push(c);
                            self.bump();
                        }
                    } else {
                        self.bump();
                        break;
                    }
                }
                Some('\\') => {
                    // Keep escapes verbatim; decode below (raw keeps them literal).
                    raw.push('\\');
                    self.bump();
                    if let Some(n) = self.peek() {
                        raw.push(n);
                        self.bump();
                    }
                }
                // f-string replacement-field braces. `{{`/`}}` are literal braces
                // ONLY in the literal text between fields (depth 0). Inside a
                // field they are two structural braces: `f"{x:{w}}"` closes the
                // nested width field and then the outer one, and reading that
                // `}}` as an escape left the outer field open — the scan ran past
                // the closing quote and reported an unterminated string.
                Some('{') if is_f => {
                    raw.push('{');
                    self.bump();
                    if brace_depth == 0 && self.peek() == Some('{') {
                        raw.push('{');
                        self.bump();
                    } else {
                        brace_depth += 1;
                    }
                }
                Some('}') if is_f => {
                    raw.push('}');
                    self.bump();
                    if brace_depth == 0 && self.peek() == Some('}') {
                        raw.push('}');
                        self.bump();
                    } else if brace_depth > 0 {
                        brace_depth -= 1;
                    }
                }
                Some('\n') if !triple => return Err(unterminated(self)),
                Some(c) => {
                    raw.push(c);
                    self.bump();
                }
            }
        }
        if is_t {
            self.push(Tok::TString(raw, is_raw));
        } else if is_f {
            self.push(Tok::FString(raw, is_raw));
        } else if is_bytes {
            let decoded = decode_bytes_escapes(&raw, is_raw)?;
            // Each decoded code point is one byte (latin-1): `\xff` -> 0xFF, not
            // its two-byte UTF-8 encoding.
            let bytes: Vec<u8> = decoded.chars().map(|c| c as u32 as u8).collect();
            self.push(Tok::Bytes(bytes));
        } else {
            let decoded = decode_escapes(&raw, is_raw)?;
            self.push(Tok::Str(decoded));
        }
        Ok(())
    }

    fn scan_number(&mut self) -> Result<(), String> {
        let mut s = String::new();
        let mut is_float = false;
        let mut is_complex = false;
        // Radix prefixes.
        if self.peek() == Some('0') {
            if let Some(r) = self.peek2() {
                if matches!(r, 'x' | 'X' | 'o' | 'O' | 'b' | 'B') {
                    self.bump();
                    self.bump();
                    let radix = match r.to_ascii_lowercase() {
                        'x' => 16,
                        'o' => 8,
                        _ => 2,
                    };
                    let kind = match radix {
                        16 => "hexadecimal",
                        8 => "octal",
                        _ => "binary",
                    };
                    // Reported at the character at `at` (a char index), as the
                    // tokenizer's `verify_end_of_number` does.
                    let bad = |lx: &Self, at: usize| {
                        let col = at - lx.line_start;
                        lx.tok_err(
                            &format!("SyntaxError: invalid {kind} literal"),
                            lx.line,
                            col,
                            col as i64 + 1,
                        )
                    };
                    let mut digits = String::new();
                    while let Some(c) = self.peek() {
                        if c == '_' {
                            // A separator sits between two digits, or right after
                            // the prefix (`0x_f`): never last, never doubled.
                            match self.src.get(self.pos + 1) {
                                Some(n) if n.is_digit(radix) => self.pos += 1,
                                _ => return Err(bad(self, self.pos)),
                            }
                        } else if c.is_digit(radix) {
                            digits.push(c);
                            self.pos += 1;
                        } else if c.is_ascii_digit() {
                            // `0b2`, `0o8`: a decimal digit the radix lacks.
                            let col = self.pos - self.line_start;
                            return Err(self.tok_err(
                                &format!("SyntaxError: invalid digit '{c}' in {kind} literal"),
                                self.line,
                                col,
                                col as i64 + 1,
                            ));
                        } else {
                            break;
                        }
                    }
                    if digits.is_empty() {
                        return Err(bad(self, self.pos - 1));
                    }
                    match i64::from_str_radix(&digits, radix) {
                        Ok(n) => self.push(Tok::Int(n)),
                        // Overflows i64 (`0xFFFFFFFFFFFFFFFF`, a wide `0o…`/`0b…`):
                        // promote to a bignum, storing its decimal form so it takes
                        // the same `Tok::BigInt` path as an oversized decimal.
                        Err(_) => {
                            let big = num_bigint::BigInt::parse_bytes(digits.as_bytes(), radix)
                                .ok_or_else(|| {
                                    format!("SyntaxError: bad int literal (line {})", self.line)
                                })?;
                            self.push(Tok::BigInt(big.to_string()));
                        }
                    }
                    return Ok(());
                }
            }
        }
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => {
                    s.push(c);
                    self.pos += 1;
                }
                // A separator sits between two decimal digits — not after the
                // point or the exponent marker, not last, not doubled.
                '_' => {
                    let after_digit = s.ends_with(|p: char| p.is_ascii_digit());
                    let before_digit =
                        matches!(self.src.get(self.pos + 1), Some(n) if n.is_ascii_digit());
                    if !(after_digit && before_digit) {
                        let col = self.pos - self.line_start;
                        return Err(self.tok_err(
                            "SyntaxError: invalid decimal literal",
                            self.line,
                            col,
                            col as i64 + 1,
                        ));
                    }
                    self.pos += 1;
                }
                '.' => {
                    // A second `.` after the number is already a float (a
                    // decimal point or an exponent was seen) is attribute
                    // access, not part of the literal: `0.1.is_integer()` lexes
                    // as `0.1` then `.is_integer`, matching CPython.
                    if is_float {
                        break;
                    }
                    is_float = true;
                    s.push(c);
                    self.pos += 1;
                }
                'e' | 'E' => {
                    is_float = true;
                    s.push('e');
                    self.pos += 1;
                    if matches!(self.peek(), Some('+') | Some('-')) {
                        s.push(self.peek().unwrap());
                        self.pos += 1;
                    }
                }
                'j' | 'J' => {
                    is_complex = true;
                    self.pos += 1;
                    break;
                }
                _ => break,
            }
        }
        if is_complex {
            let v: f64 = s
                .parse()
                .map_err(|_| format!("SyntaxError: bad complex (line {})", self.line))?;
            self.push(Tok::Complex(v));
        } else if is_float {
            let v: f64 = s
                .parse()
                .map_err(|_| format!("SyntaxError: bad float (line {})", self.line))?;
            self.push(Tok::Float(v));
        } else {
            // `0777`: a decimal integer may not have leading zeros unless it is
            // all zeros. Underlined from the literal's start through the zeros.
            if s.len() > 1 && s.starts_with('0') && !s.trim_start_matches('0').is_empty() {
                let start = self.tok_start - self.line_start;
                let zeros = s.len() - s.trim_start_matches('0').len();
                return Err(self.tok_err(
                    "SyntaxError: leading zeros in decimal integer literals are not permitted; \
                     use an 0o prefix for octal integers",
                    self.line,
                    start,
                    (start + zeros) as i64 + 1,
                ));
            }
            // A decimal literal is converted like `int(str)` and is bounded by
            // the same `sys.get_int_max_str_digits()` (hex/octal/binary are not).
            let limit = crate::host::int_max_str_digits();
            if limit > 0 && s.len() > limit {
                let msg = crate::host::int_max_str_digits_error(limit, Some(s.len()));
                return Err(format!(
                    "SyntaxError{} - Consider hexadecimal for huge integer literals to avoid \
                     decimal conversion limits. (line {})",
                    msg.trim_start_matches("ValueError"),
                    self.line
                ));
            }
            match s.parse::<i64>() {
                Ok(n) => self.push(Tok::Int(n)),
                Err(_) => self.push(Tok::BigInt(s)),
            }
        }
        Ok(())
    }

    fn scan_op(&mut self) -> Result<(), String> {
        let rest: String = self.src[self.pos..(self.pos + 3).min(self.src.len())]
            .iter()
            .collect();
        for op in OPS3 {
            if rest.starts_with(op) {
                self.pos += 3;
                self.push(Tok::Op((*op).to_string()));
                return Ok(());
            }
        }
        let two: String = self.src[self.pos..(self.pos + 2).min(self.src.len())]
            .iter()
            .collect();
        for op in OPS2 {
            if two.starts_with(op) {
                self.pos += 2;
                self.push(Tok::Op((*op).to_string()));
                return Ok(());
            }
        }
        let c = self.bump().unwrap();
        match c {
            '(' | '[' | '{' => {
                self.depth += 1;
                self.open
                    .push((c, self.line, (self.pos - 1 - self.line_start) as u32));
                // CPython's tokenizer holds the open brackets in a fixed
                // `tok->parenstack[MAXLEVEL]` with `MAXLEVEL == 200`
                // (`Parser/lexer/state.h`), and `tok_get_normal_mode` refuses the
                // 201st open with `too many nested parentheses` — one counter
                // shared by `(`, `[` and `{`, so `([{` * 67 trips it too.
                // Without the cap a deep literal recursed the parser, the
                // compiler and the AST's own `Drop` until the interpreter
                // thread's stack ran out: `exec('('*10000)` aborted the process
                // (SIGABRT, exit 134) where CPython raises a catchable
                // `SyntaxError`.
                if self.depth > MAX_PAREN_DEPTH {
                    return Err("SyntaxError: too many nested parentheses".to_string());
                }
            }
            ')' | ']' | '}' => {
                let col = self.pos - 1 - self.line_start;
                let opener = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                match self.open.pop() {
                    None => {
                        return Err(self.tok_err(
                            &format!("SyntaxError: unmatched '{c}'"),
                            self.line,
                            col,
                            col as i64 + 1,
                        ))
                    }
                    Some((o, line, _)) if o != opener => {
                        let at = if line == self.line {
                            String::new()
                        } else {
                            format!(" on line {line}")
                        };
                        return Err(self.tok_err(
                            &format!(
                                "SyntaxError: closing parenthesis '{c}' does not match \
                                 opening parenthesis '{o}'{at}"
                            ),
                            self.line,
                            col,
                            col as i64 + 1,
                        ));
                    }
                    Some(_) => {}
                }
                self.depth = (self.depth - 1).max(0)
            }
            _ => {}
        }
        if "+-*/%@&|^~<>=(){}[]:,.;".contains(c) {
            self.push(Tok::Op(c.to_string()));
            Ok(())
        } else {
            // A stray character is rejected the way CPython's tokenizer
            // reports it, positioned at that character: an ASCII one (`!`
            // outside `!=`, `$`, `?`, backtick, …) is `invalid syntax`, any
            // other is `invalid character '€' (U+20AC)`.
            let col = self.pos - 1 - self.line_start;
            let msg = if c.is_ascii() {
                "SyntaxError: invalid syntax".to_string()
            } else {
                format!("SyntaxError: invalid character '{c}' (U+{:04X})", c as u32)
            };
            Err(self.tok_err(&msg, self.line, col, col as i64 + 1))
        }
    }
}

/// Decode Python string escapes. Raw strings keep backslashes literal.
pub fn decode_escapes(raw: &str, is_raw: bool) -> Result<String, String> {
    decode_escapes_mode(raw, is_raw, false)
}

/// Escape decoding for a BYTES literal. Differs from the text form in exactly
/// the two ways CPython's `PyBytes_DecodeEscape` does: `\u`/`\U`/`\N` are not
/// escapes at all (the backslash and letter stay literal), and a short `\x`
/// reports `(value error) invalid \x escape at position N` rather than the
/// `unicodeescape` wording.
pub fn decode_bytes_escapes(raw: &str, is_raw: bool) -> Result<String, String> {
    decode_escapes_mode(raw, is_raw, true)
}

fn decode_escapes_mode(raw: &str, is_raw: bool, bytes_mode: bool) -> Result<String, String> {
    if is_raw {
        return Ok(raw.to_string());
    }
    let mut out = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            let e = chars[i];
            match e {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                // Octal escape `\ooo` (1-3 octal digits).
                '0'..='7' => {
                    let mut oct = String::new();
                    oct.push(e);
                    while oct.len() < 3 && matches!(chars.get(i + 1), Some('0'..='7')) {
                        i += 1;
                        oct.push(chars[i]);
                    }
                    if let Ok(n) = u32::from_str_radix(&oct, 8) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                    }
                }
                'a' => out.push('\u{07}'),
                'b' => out.push('\u{08}'),
                'f' => out.push('\u{0C}'),
                'v' => out.push('\u{0B}'),
                '\n' => {} // line continuation inside string
                // `\xXX`, `\uXXXX`, `\UXXXXXXXX` — a FIXED digit count. Fewer hex
                // digits than the escape demands is a `SyntaxError` in CPython,
                // not a shorter escape: pythonrs read as many as were there and
                // silently produced a different string, so `'\x2'` evaluated to
                // `'\x02'`, `'\u12'` to `'\x12'` and `'\xzz'` to `'zz'`.
                // `\u`, `\U` and `\N` are TEXT escapes only; inside a bytes
                // literal CPython leaves them as the two literal characters.
                // pythonrs decoded them anyway, so `b'ሴ'` — six bytes in
                // CPython — came out as the single byte `b'4'`.
                'u' | 'U' | 'N' if bytes_mode => {
                    out.push('\\');
                    out.push(e);
                }
                'x' | 'u' | 'U' => {
                    let want = match e {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let start = byte_offset(&chars, i - 1);
                    // A bytes literal reports the same defect as a `value error`
                    // out of `PyBytes_DecodeEscape`, with a different wording and
                    // only the start position.
                    if bytes_mode {
                        let digits = chars[i + 1..]
                            .iter()
                            .take(want)
                            .take_while(|c| c.is_ascii_hexdigit())
                            .count();
                        if digits < want {
                            return Err(format!(
                                "SyntaxError: (value error) invalid \\x escape at position {start}"
                            ));
                        }
                    }
                    let digits: String = chars[i + 1..]
                        .iter()
                        .take(want)
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    if digits.len() < want {
                        // CPython's span covers the backslash, the escape letter,
                        // and however many hex digits it did manage to read.
                        return Err(unicode_escape_err(
                            start,
                            start + 1 + digits.len(),
                            EscErr::Truncated(e),
                        ));
                    }
                    let n = u32::from_str_radix(&digits, 16).unwrap_or(u32::MAX);
                    match char::from_u32(n) {
                        Some(ch) => out.push(ch),
                        // Past U+10FFFF (only reachable through `\U`).
                        None => {
                            return Err(unicode_escape_err(
                                start,
                                start + 1 + want,
                                EscErr::IllegalChar,
                            ))
                        }
                    }
                    i += want;
                }
                // Named Unicode escape `\N{NAME}` (e.g. `\N{LATIN SMALL LETTER E WITH ACUTE}`).
                'N' => {
                    // Byte offset of the `\` (positions in CPython's error are byte-based).
                    let start = byte_offset(&chars, i - 1);
                    if chars.get(i + 1) != Some(&'{') {
                        // `\N` not followed by `{` → malformed (covers just `\N`).
                        return Err(unicode_escape_err(start, start + 1, EscErr::MalformedName));
                    }
                    let name_start = i + 2;
                    let mut j = name_start;
                    while j < chars.len() && chars[j] != '}' {
                        j += 1;
                    }
                    if j >= chars.len() {
                        // No closing `}` → malformed (covers to end of the literal).
                        let end = raw.len().saturating_sub(1);
                        return Err(unicode_escape_err(start, end, EscErr::MalformedName));
                    }
                    if j == name_start {
                        // Empty `\N{}` → malformed (covers `\N{`).
                        let end = byte_offset(&chars, i + 1);
                        return Err(unicode_escape_err(start, end, EscErr::MalformedName));
                    }
                    let name: String = chars[name_start..j].iter().collect();
                    // CPython matches names case-insensitively but NOT loosely — leading/
                    // trailing whitespace or `_`/`-` swaps must fail. `unicode_names2` does
                    // UAX#44 loose matching, so round-trip through the canonical name and
                    // require it to equal the uppercased input exactly.
                    let upper = name.to_ascii_uppercase();
                    let resolved = unicode_names2::character(&upper).filter(|&ch| {
                        unicode_names2::name(ch).is_some_and(|n| n.to_string() == upper)
                    });
                    match resolved {
                        Some(ch) => {
                            out.push(ch);
                            i = j; // land on `}`; the `i += 1` below steps past it.
                        }
                        None => {
                            // Unknown name → covers `\N{NAME}` through the closing `}`.
                            let end = byte_offset(&chars, j);
                            return Err(unicode_escape_err(start, end, EscErr::UnknownName));
                        }
                    }
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// True if `lit` ends with an active `\N` escape lead — a trailing `N` preceded by
/// an odd run of backslashes. Used by the f-string parser so `\N{NAME}`'s braces are
/// treated as part of the named-Unicode escape rather than a replacement field.
/// Always false for raw strings (backslashes are literal there).
pub fn ends_with_named_escape_lead(lit: &str, is_raw: bool) -> bool {
    if is_raw {
        return false;
    }
    let chars: Vec<char> = lit.chars().collect();
    if chars.last() != Some(&'N') {
        return false;
    }
    let mut backslashes = 0;
    let mut idx = chars.len() - 1;
    while idx > 0 && chars[idx - 1] == '\\' {
        backslashes += 1;
        idx -= 1;
    }
    backslashes % 2 == 1
}

/// Byte offset of the char at `idx` within a `char` slice.
fn byte_offset(chars: &[char], idx: usize) -> usize {
    chars[..idx].iter().map(|c| c.len_utf8()).sum()
}

/// Which `unicodeescape` failure to report. The four spellings are CPython's own
/// (`Objects/unicodeobject.c`, `unicode_decode_call_errorhandler_writer`
/// callers), reproduced verbatim.
pub(crate) enum EscErr {
    /// `\N{...}` that is not a well-formed named escape.
    MalformedName,
    /// `\N{...}` whose name is not in the Unicode database.
    UnknownName,
    /// `\x`/`\u`/`\U` with fewer hex digits than it requires; carries the escape
    /// letter so the message names the right form.
    Truncated(char),
    /// A `\U` code point past U+10FFFF.
    IllegalChar,
}

/// Format CPython's `unicodeescape` error. `start`/`end` are the inclusive byte
/// positions CPython reports.
fn unicode_escape_err(start: usize, end: usize, kind: EscErr) -> String {
    let truncated;
    let reason: &str = match kind {
        EscErr::UnknownName => "unknown Unicode character name",
        EscErr::MalformedName => "malformed \\N character escape",
        EscErr::IllegalChar => "illegal Unicode character",
        EscErr::Truncated(e) => {
            let form = match e {
                'x' => "\\xXX",
                'u' => "\\uXXXX",
                _ => "\\UXXXXXXXX",
            };
            truncated = format!("truncated {form} escape");
            &truncated
        }
    };
    // CPython raises this as a `SyntaxError` whose `str()` is the parenthesised
    // `(unicode error) …` text, so the reported line reads
    // `SyntaxError: (unicode error) …`. The prefix was missing here, and the
    // escape-decoding errors were the only lexer diagnostics printed without an
    // exception name in front of them.
    format!(
        "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes in position {}-{}: {}",
        start, end, reason
    )
}

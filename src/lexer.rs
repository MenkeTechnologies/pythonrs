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

struct Lexer {
    src: Vec<char>,
    pos: usize,
    line: u32,
    depth: i32,
    indents: Vec<usize>,
    out: Vec<Token>,
    /// Char index where the current line begins — subtracted from a token's
    /// start/end char index to get its 0-based character column (for carets).
    line_start: usize,
    /// Char index where the token currently being scanned begins.
    tok_start: usize,
}

/// Multi-char operators, longest first so the scanner is greedy.
const OPS3: &[&str] = &["**=", "//=", ">>=", "<<=", "...", "!=="];
const OPS2: &[&str] = &[
    "**", "//", ">>", "<<", "<=", ">=", "==", "!=", "->", ":=", "+=", "-=", "*=", "/=", "%=", "&=",
    "|=", "^=", "@=",
];

/// Tokenize `src` into a token stream ending in `Eof`.
pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut lx = Lexer {
        src: src.chars().collect(),
        pos: 0,
        line: 1,
        depth: 0,
        indents: vec![0],
        out: Vec::new(),
        line_start: 0,
        tok_start: 0,
    };
    lx.run()?;
    Ok(lx.out)
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
                if self.handle_indent()? {
                    // Blank/comment-only line consumed; stay at line start.
                    continue;
                }
                at_line_start = false;
            }
            match self.peek() {
                None => break,
                Some('\n') => {
                    self.bump();
                    if self.depth == 0 {
                        // Collapse runs of blank physical lines to one Newline.
                        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                            self.push(Tok::Newline);
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
        // Terminate a trailing logical line.
        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
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
                let _ = start;
                return Err(format!("IndentationError: unindent does not match any outer indentation level (line {})", self.line));
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
            "r" | "b" | "f" | "u" | "rb" | "br" | "rf" | "fr" | "bf" | "fb"
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
            if c.is_alphanumeric() || c == '_' {
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
        let is_f = prefix.contains('f');
        let quote = self.bump().unwrap();
        let triple = self.peek() == Some(quote) && self.peek2() == Some(quote);
        if triple {
            self.bump();
            self.bump();
        }
        let mut raw = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(format!(
                        "SyntaxError: unterminated string (line {})",
                        self.line
                    ))
                }
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
                Some('\n') if !triple => {
                    return Err(format!(
                        "SyntaxError: EOL while scanning string literal (line {})",
                        self.line
                    ));
                }
                Some(c) => {
                    raw.push(c);
                    self.bump();
                }
            }
        }
        if is_f {
            self.push(Tok::FString(raw, is_raw));
        } else if is_bytes {
            let decoded = decode_escapes(&raw, is_raw)?;
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
                    let mut digits = String::new();
                    while let Some(c) = self.peek() {
                        if c == '_' {
                            self.pos += 1;
                        } else if c.is_digit(radix) {
                            digits.push(c);
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    let n = i64::from_str_radix(&digits, radix).map_err(|_| {
                        format!("SyntaxError: bad int literal (line {})", self.line)
                    })?;
                    self.push(Tok::Int(n));
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
                '_' => {
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
            '(' | '[' | '{' => self.depth += 1,
            ')' | ']' | '}' => self.depth = (self.depth - 1).max(0),
            _ => {}
        }
        if "+-*/%@&|^~<>=(){}[]:,.;".contains(c) {
            self.push(Tok::Op(c.to_string()));
            Ok(())
        } else {
            // A stray character (`!` outside `!=`, `$`, `?`, backtick, …) is
            // rejected the way CPython reports it: a bare `SyntaxError: invalid
            // syntax` (the line/caret live in the traceback's `File` header, not
            // the message), so the final error line matches `python3` exactly.
            let _ = c;
            Err("SyntaxError: invalid syntax".to_string())
        }
    }
}

/// Decode Python string escapes. Raw strings keep backslashes literal.
pub fn decode_escapes(raw: &str, is_raw: bool) -> Result<String, String> {
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
                'x' => {
                    let h: String = chars[i + 1..(i + 3).min(chars.len())].iter().collect();
                    if let Ok(n) = u32::from_str_radix(&h, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                        i += 2;
                    }
                }
                'u' => {
                    let h: String = chars[i + 1..(i + 5).min(chars.len())].iter().collect();
                    if let Ok(n) = u32::from_str_radix(&h, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                        i += 4;
                    }
                }
                'U' => {
                    let h: String = chars[i + 1..(i + 9).min(chars.len())].iter().collect();
                    if let Ok(n) = u32::from_str_radix(&h, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                        i += 8;
                    }
                }
                // Named Unicode escape `\N{NAME}` (e.g. `\N{LATIN SMALL LETTER E WITH ACUTE}`).
                'N' => {
                    // Byte offset of the `\` (positions in CPython's error are byte-based).
                    let start = byte_offset(&chars, i - 1);
                    if chars.get(i + 1) != Some(&'{') {
                        // `\N` not followed by `{` → malformed (covers just `\N`).
                        return Err(unicode_escape_err(start, start + 1, false));
                    }
                    let name_start = i + 2;
                    let mut j = name_start;
                    while j < chars.len() && chars[j] != '}' {
                        j += 1;
                    }
                    if j >= chars.len() {
                        // No closing `}` → malformed (covers to end of the literal).
                        let end = raw.len().saturating_sub(1);
                        return Err(unicode_escape_err(start, end, false));
                    }
                    if j == name_start {
                        // Empty `\N{}` → malformed (covers `\N{`).
                        let end = byte_offset(&chars, i + 1);
                        return Err(unicode_escape_err(start, end, false));
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
                            return Err(unicode_escape_err(start, end, true));
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

/// Format CPython's `unicodeescape` error for a `\N{...}` escape. `unknown` picks
/// between the unknown-name and malformed-escape messages; `start`/`end` are the
/// inclusive byte positions CPython reports.
fn unicode_escape_err(start: usize, end: usize, unknown: bool) -> String {
    let reason = if unknown {
        "unknown Unicode character name"
    } else {
        "malformed \\N character escape"
    };
    format!(
        "(unicode error) 'unicodeescape' codec can't decode bytes in position {}-{}: {}",
        start, end, reason
    )
}

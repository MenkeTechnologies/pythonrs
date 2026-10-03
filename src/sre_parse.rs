//! The pattern checker in front of the regex engines: a port of CPython's
//! `re/_parser.py` grammar (`parse` and everything it calls) plus the one check
//! `re/_compiler.py` makes on the parsed tree (a look-behind must be
//! fixed-width).
//!
//! The engines in [`crate::regexpr`] accept a different language from CPython's
//! — `a**`, `(?P<a>x)(?P<a>y)` and `(?<=a|bc)` compile there and are errors in
//! CPython — and their own errors are worded for that language
//! (`unclosed group`). So every pattern is first parsed the way `sre_parse`
//! parses it: what it rejects is rejected here with its message and position,
//! raised as the `re.PatternError` `_constants.PatternError(msg, pattern, pos)`
//! builds, and only a pattern it accepts reaches an engine.
//!
//! The parse builds the subpattern tree only as far as the checks need it:
//! which item a repeat applies to, and each item's width (`getwidth`), which a
//! back-reference inside a look-behind and the look-behind itself are measured
//! by. `_parse_sub`'s two rewrites of an alternation (moving a common prefix out
//! of the branches, turning single-character branches into a set) are left
//! out: neither changes a width or what any check sees.
//!
//! Positions count characters of a `str` pattern and bytes of a `bytes` one —
//! the caller hands a bytes pattern over latin-1 decoded, one char per byte, as
//! `Tokenizer` itself decodes it.

use std::cell::RefCell;

/// `_sre.MAXREPEAT`: a repeat bound must be below it.
const MAXREPEAT: u128 = 4_294_967_295;
/// `_sre.MAXGROUPS`.
const MAXGROUPS: u128 = 1_073_741_823;
/// `_compiler.MAXCODE` (a 4-byte code word).
const MAXCODE: u128 = 4_294_967_295;
/// `_parser.MAXWIDTH`, the cap on a reported width.
const MAXWIDTH: u128 = 1 << 64;

const FLAG_LOCALE: i64 = 4;
const FLAG_VERBOSE: i64 = 64;
const FLAG_DEBUG: i64 = 128;
const FLAG_UNICODE: i64 = 32;
const FLAG_ASCII: i64 = 256;
const TYPE_FLAGS: i64 = FLAG_ASCII | FLAG_LOCALE | FLAG_UNICODE;
const GLOBAL_FLAGS: i64 = FLAG_DEBUG;

/// `_parser.FLAGS`: an inline flag letter's bit.
fn flag_bit(c: char) -> Option<i64> {
    Some(match c {
        'i' => 2,
        'L' => FLAG_LOCALE,
        'm' => 8,
        's' => 16,
        'x' => FLAG_VERBOSE,
        'a' => FLAG_ASCII,
        'u' => FLAG_UNICODE,
        _ => return None,
    })
}

/// Why a pattern was refused, as the exception CPython raises for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// `re.PatternError(msg, pattern, pos)`; `pos` is `None` for the
    /// compiler's look-behind errors, which carry no position.
    Pattern { msg: String, pos: Option<usize> },
    /// A repeat bound at or above `MAXREPEAT`.
    Overflow,
    /// `fix_flags`: flags that cannot go with this kind of pattern.
    Value(String),
}

type Res<T> = Result<T, Refusal>;

/// One token: a character, or a backslash and the character after it.
#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Char(char),
    Esc(char),
}

impl Tok {
    fn len(self) -> usize {
        match self {
            Tok::Char(_) => 1,
            Tok::Esc(_) => 2,
        }
    }

    fn push_to(self, s: &mut String) {
        if let Tok::Esc(_) = self {
            s.push('\\');
        }
        s.push(match self {
            Tok::Char(c) | Tok::Esc(c) => c,
        });
    }

    fn text(self) -> String {
        let mut s = String::new();
        self.push_to(&mut s);
        s
    }

    /// The token is the single character `c` (`this == c`).
    fn is(self, c: char) -> bool {
        self == Tok::Char(c)
    }

    /// The token is one of the single characters in `set` (`this in set`).
    fn is_in(self, set: &str) -> bool {
        matches!(self, Tok::Char(c) if set.contains(c))
    }
}

/// `_parser.Tokenizer`.
struct Tokenizer<'a> {
    chars: &'a [char],
    istext: bool,
    index: usize,
    next: Option<Tok>,
}

impl<'a> Tokenizer<'a> {
    fn new(chars: &'a [char], istext: bool) -> Res<Self> {
        let mut t = Tokenizer {
            chars,
            istext,
            index: 0,
            next: None,
        };
        t.advance()?;
        Ok(t)
    }

    /// `__next`: read the token at `index`.
    fn advance(&mut self) -> Res<()> {
        let mut index = self.index;
        let Some(&c) = self.chars.get(index) else {
            self.next = None;
            return Ok(());
        };
        let tok = if c == '\\' {
            index += 1;
            match self.chars.get(index) {
                Some(&e) => Tok::Esc(e),
                None => {
                    return Err(Refusal::Pattern {
                        msg: "bad escape (end of pattern)".into(),
                        pos: Some(self.chars.len() - 1),
                    })
                }
            }
        } else {
            Tok::Char(c)
        };
        self.index = index + 1;
        self.next = Some(tok);
        Ok(())
    }

    fn matches(&mut self, c: char) -> Res<bool> {
        if self.next == Some(Tok::Char(c)) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn get(&mut self) -> Res<Option<Tok>> {
        let this = self.next;
        self.advance()?;
        Ok(this)
    }

    /// The next token's character when it is a plain one.
    fn next_char(&self) -> Option<char> {
        match self.next {
            Some(Tok::Char(c)) => Some(c),
            _ => None,
        }
    }

    /// `getwhile(n, charset)`, appending to `out`.
    fn getwhile(&mut self, n: usize, charset: fn(char) -> bool, out: &mut String) -> Res<()> {
        for _ in 0..n {
            match self.next_char() {
                Some(c) if charset(c) => {
                    out.push(c);
                    self.advance()?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn getuntil(&mut self, terminator: char, name: &str) -> Res<String> {
        let mut result = String::new();
        loop {
            let c = self.next;
            self.advance()?;
            let Some(c) = c else {
                if result.is_empty() {
                    return Err(self.error(&format!("missing {name}"), 0));
                }
                let n = result.chars().count();
                return Err(self.error(&format!("missing {terminator}, unterminated name"), n));
            };
            if c.is(terminator) {
                if result.is_empty() {
                    return Err(self.error(&format!("missing {name}"), 1));
                }
                break;
            }
            c.push_to(&mut result);
        }
        Ok(result)
    }

    fn tell(&self) -> usize {
        self.index - self.next.map_or(0, Tok::len)
    }

    fn seek(&mut self, index: usize) -> Res<()> {
        self.index = index;
        self.advance()
    }

    /// `error(msg, offset)`: the refusal at `tell() - offset`, its message
    /// backslash-escaped to ASCII for a bytes pattern.
    fn error(&self, msg: &str, offset: usize) -> Refusal {
        let msg = if self.istext {
            msg.to_string()
        } else {
            crate::host::ascii_of(msg)
        };
        Refusal::Pattern {
            msg,
            pos: Some(self.tell().saturating_sub(offset)),
        }
    }

    fn checkgroupname(&self, name: &str, offset: usize) -> Res<()> {
        match group_name_error(name, self.istext) {
            Some(msg) => Err(self.error(&msg, name.chars().count() + offset)),
            None => Ok(()),
        }
    }
}

/// `Tokenizer.checkgroupname`'s message for a group name that is not one —
/// non-ASCII in a bytes pattern (named with `%a`), or not an identifier —
/// or `None` for a good name. Shared with the template parser.
pub fn group_name_error(name: &str, istext: bool) -> Option<String> {
    if !(istext || name.is_ascii()) {
        return Some(format!(
            "bad character in group name {}",
            crate::host::ascii_of(&repr(name))
        ));
    }
    if !crate::builtins::is_identifier(name) {
        return Some(format!("bad character in group name {}", repr(name)));
    }
    None
}

fn repr(s: &str) -> String {
    crate::host::quote_str(s)
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn is_octdigit(c: char) -> bool {
    matches!(c, '0'..='7')
}

fn is_hexdigit(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// A run of decimal digits as a number, saturating far above every bound it
/// is compared with.
fn decimal(digits: &str) -> u128 {
    digits.bytes().fold(0u128, |n, d| {
        n.saturating_mul(10).saturating_add(u128::from(d - b'0'))
    })
}

/// An item of a parsed subpattern, reduced to what the checks look at.
#[derive(Clone)]
enum Node {
    /// `ANY`, `LITERAL`, `NOT_LITERAL`, `IN`, `RANGE`, `CATEGORY`: one character.
    Unit,
    /// `AT`: an anchor, which nothing can repeat.
    At,
    /// `MAX_REPEAT`/`MIN_REPEAT`/`POSSESSIVE_REPEAT`.
    Repeat {
        min: u128,
        max: u128,
        item: Vec<Node>,
    },
    /// `SUBPATTERN`: a group, capturing (`group`) or carrying scoped flags.
    Group {
        group: Option<usize>,
        add_flags: i64,
        del_flags: i64,
        p: Vec<Node>,
    },
    AtomicGroup(Vec<Node>),
    Branch(Vec<Vec<Node>>),
    GroupRef(usize),
    GroupRefExists {
        yes: Vec<Node>,
        no: Option<Vec<Node>>,
    },
    /// `ASSERT`/`ASSERT_NOT`; `dir < 0` is a look-behind.
    Assert {
        dir: i8,
        p: Vec<Node>,
    },
    Failure,
}

impl Node {
    fn is_repeat(&self) -> bool {
        matches!(self, Node::Repeat { .. })
    }
}

/// `_parser.State`.
#[derive(Default)]
struct State {
    flags: i64,
    groupdict: Vec<(String, usize)>,
    /// `groupwidths[g]`: the closed group's `(min, max)` width; `None` while
    /// open. Index 0 is the whole pattern.
    groupwidths: Vec<Option<(u128, u128)>>,
    lookbehindgroups: Option<usize>,
    /// Group numbers a conditional `(?(N)…)` named, with where, in order.
    grouprefpos: Vec<(u128, usize)>,
}

impl State {
    fn groups(&self) -> usize {
        self.groupwidths.len()
    }

    fn group_named(&self, name: &str) -> Option<usize> {
        self.groupdict
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, g)| g)
    }

    /// `opengroup(name)`; the error is the bare message, which the caller
    /// positions.
    fn opengroup(&mut self, name: Option<&str>) -> Result<usize, String> {
        let gid = self.groups();
        self.groupwidths.push(None);
        if self.groups() as u128 > MAXGROUPS {
            return Err("too many groups".into());
        }
        if let Some(name) = name {
            if let Some(ogid) = self.group_named(name) {
                return Err(format!(
                    "redefinition of group name {} as group {gid}; was group {ogid}",
                    repr(name)
                ));
            }
            self.groupdict.push((name.to_string(), gid));
        }
        Ok(gid)
    }

    fn closegroup(&mut self, gid: usize, p: &[Node]) {
        let w = self.getwidth(p);
        self.groupwidths[gid] = Some(w);
    }

    fn checkgroup(&self, gid: usize) -> bool {
        gid < self.groups() && self.groupwidths[gid].is_some()
    }

    fn checklookbehindgroup(&self, gid: usize, source: &Tokenizer) -> Res<()> {
        if let Some(lookbehind) = self.lookbehindgroups {
            if !self.checkgroup(gid) {
                return Err(source.error("cannot refer to an open group", 0));
            }
            if gid >= lookbehind {
                return Err(source.error(
                    "cannot refer to group defined in the same lookbehind subpattern",
                    0,
                ));
            }
        }
        Ok(())
    }

    /// `SubPattern.getwidth`: the `(min, max)` characters `p` can match.
    fn getwidth(&self, p: &[Node]) -> (u128, u128) {
        let (mut lo, mut hi) = (0u128, 0u128);
        for node in p {
            match node {
                Node::Branch(items) => {
                    let (mut i, mut j) = (MAXWIDTH, 0);
                    for item in items {
                        let (l, h) = self.getwidth(item);
                        i = i.min(l);
                        j = j.max(h);
                    }
                    lo += i;
                    hi += j;
                }
                Node::AtomicGroup(p) | Node::Group { p, .. } => {
                    let (i, j) = self.getwidth(p);
                    lo += i;
                    hi += j;
                }
                Node::Repeat { min, max, item } => {
                    let (i, j) = self.getwidth(item);
                    lo += i * min;
                    if *max == MAXREPEAT && j != 0 {
                        hi = MAXWIDTH;
                    } else {
                        hi += j * max;
                    }
                }
                Node::Unit => {
                    lo += 1;
                    hi += 1;
                }
                Node::GroupRef(g) => {
                    let (i, j) = self.groupwidths[*g].unwrap_or((0, 0));
                    lo += i;
                    hi += j;
                }
                Node::GroupRefExists { yes, no } => {
                    let (mut i, mut j) = self.getwidth(yes);
                    match no {
                        Some(no) => {
                            let (l, h) = self.getwidth(no);
                            i = i.min(l);
                            j = j.max(h);
                        }
                        None => i = 0,
                    }
                    lo += i;
                    hi += j;
                }
                Node::At | Node::Assert { .. } | Node::Failure => {}
            }
            lo = lo.min(MAXWIDTH);
            hi = hi.min(MAXWIDTH);
        }
        (lo.min(MAXWIDTH), hi.min(MAXWIDTH))
    }
}

/// What a character-set escape stands for.
enum ClassCode {
    Literal(u32),
    In,
}

/// `\N{name}`'s character, as `unicodedata.lookup` finds it.
fn named_char(source: &mut Tokenizer) -> Res<u32> {
    if !source.matches('{')? {
        return Err(source.error("missing {", 0));
    }
    let charname = source.getuntil('}', "character name")?;
    match crate::lexer::lookup_char_name(&charname) {
        Some(c) => Ok(c as u32),
        None => {
            let msg = format!("undefined character name {}", repr(&charname));
            Err(source.error(&msg, charname.chars().count() + 4))
        }
    }
}

/// The `\x`/`\u`/`\U`/`\N` escapes `_class_escape` and `_escape` share:
/// `Ok(Some(code point))`, `Ok(None)` when `c` is none of them (or `\u`/`\U`/
/// `\N` in a bytes pattern), or the error.
fn hex_or_named_escape(source: &mut Tokenizer, c: char, escape: &mut String) -> Res<Option<u32>> {
    let digits = match c {
        'x' => 2,
        'u' if source.istext => 4,
        'U' if source.istext => 8,
        'N' if source.istext => return named_char(source).map(Some),
        _ => return Ok(None),
    };
    source.getwhile(digits, is_hexdigit, escape)?;
    let n = escape.chars().count();
    if n != digits + 2 {
        return Err(source.error(&format!("incomplete escape {escape}"), n));
    }
    let value = u32::from_str_radix(&escape[2..], 16).unwrap_or(u32::MAX);
    // `chr(c)` raising ValueError lands on the final `bad escape`.
    if c == 'U' && value > 0x10FFFF {
        return Err(source.error(&format!("bad escape {escape}"), n));
    }
    Ok(Some(value))
}

/// `ESCAPES`: the escapes that stand for one control character.
fn simple_escape(c: char) -> Option<u32> {
    Some(match c {
        'a' => 7,
        'b' => 8,
        'f' => 12,
        'n' => 10,
        'r' => 13,
        't' => 9,
        'v' => 11,
        '\\' => 92,
        _ => return None,
    })
}

/// `_class_escape`: an escape inside `[...]`.
fn class_escape(source: &mut Tokenizer, c: char) -> Res<ClassCode> {
    if let Some(v) = simple_escape(c) {
        return Ok(ClassCode::Literal(v));
    }
    if "dDsSwW".contains(c) {
        return Ok(ClassCode::In);
    }
    let mut escape = format!("\\{c}");
    if let Some(v) = hex_or_named_escape(source, c, &mut escape)? {
        return Ok(ClassCode::Literal(v));
    }
    if is_octdigit(c) {
        source.getwhile(2, is_octdigit, &mut escape)?;
        let v = u32::from_str_radix(&escape[1..], 8).unwrap_or(0);
        if v > 0o377 {
            let msg = format!("octal escape value {escape} outside of range 0-0o377");
            return Err(source.error(&msg, escape.chars().count()));
        }
        return Ok(ClassCode::Literal(v));
    }
    if !is_digit(c) && !c.is_ascii_alphabetic() && escape.chars().count() == 2 {
        return Ok(ClassCode::Literal(c as u32));
    }
    Err(source.error(&format!("bad escape {escape}"), escape.chars().count()))
}

/// `_escape`: an escape outside a set.
fn escape(source: &mut Tokenizer, c: char, state: &State) -> Res<Node> {
    match c {
        'A' | 'b' | 'B' | 'z' | 'Z' => return Ok(Node::At),
        'd' | 'D' | 's' | 'S' | 'w' | 'W' => return Ok(Node::Unit),
        _ => {}
    }
    if simple_escape(c).is_some() {
        return Ok(Node::Unit);
    }
    let mut escape = format!("\\{c}");
    if hex_or_named_escape(source, c, &mut escape)?.is_some() {
        return Ok(Node::Unit);
    }
    if c == '0' {
        source.getwhile(2, is_octdigit, &mut escape)?;
        return Ok(Node::Unit);
    }
    if is_digit(c) {
        // An octal escape or a decimal group reference.
        if let Some(d) = source.next_char().filter(|&d| is_digit(d)) {
            source.advance()?;
            escape.push(d);
            if is_octdigit(c) && is_octdigit(d) && source.next_char().is_some_and(is_octdigit) {
                escape.push(source.next_char().unwrap_or('0'));
                source.advance()?;
                let v = u32::from_str_radix(&escape[1..], 8).unwrap_or(0);
                if v > 0o377 {
                    let msg = format!("octal escape value {escape} outside of range 0-0o377");
                    return Err(source.error(&msg, escape.chars().count()));
                }
                return Ok(Node::Unit);
            }
        }
        let n = escape.chars().count();
        let group = decimal(&escape[1..]);
        if group < state.groups() as u128 {
            let group = group as usize;
            if !state.checkgroup(group) {
                return Err(source.error("cannot refer to an open group", n));
            }
            state.checklookbehindgroup(group, source)?;
            return Ok(Node::GroupRef(group));
        }
        return Err(source.error(&format!("invalid group reference {group}"), n - 1));
    }
    if !c.is_ascii_alphabetic() {
        return Ok(Node::Unit);
    }
    Err(source.error(&format!("bad escape {escape}"), escape.chars().count()))
}

/// `_parse_sub`: an alternation.
fn parse_sub(
    source: &mut Tokenizer,
    state: &mut State,
    mut verbose: bool,
    nested: usize,
) -> Res<Vec<Node>> {
    let mut items: Vec<Vec<Node>> = Vec::new();
    loop {
        let first = nested == 0 && items.is_empty();
        items.push(parse(source, state, verbose, nested + 1, first)?);
        if !source.matches('|')? {
            break;
        }
        if nested == 0 {
            verbose = state.flags & FLAG_VERBOSE != 0;
        }
    }
    if items.len() == 1 {
        return Ok(items.pop().unwrap_or_default());
    }
    Ok(vec![Node::Branch(items)])
}

/// `_parse`: a sequence of items, up to `|`, `)` or the end.
fn parse(
    source: &mut Tokenizer,
    state: &mut State,
    mut verbose: bool,
    nested: usize,
    first: bool,
) -> Res<Vec<Node>> {
    let mut subpattern: Vec<Node> = Vec::new();
    while let Some(this) = source.next {
        if this.is_in("|)") {
            break;
        }
        source.advance()?;
        if verbose {
            if this.is_in(" \t\n\r\x0b\x0c") {
                continue;
            }
            if this.is('#') {
                while let Some(t) = source.get()? {
                    if t.is('\n') {
                        break;
                    }
                }
                continue;
            }
        }
        let c = match this {
            Tok::Esc(c) => {
                subpattern.push(escape(source, c, state)?);
                continue;
            }
            Tok::Char(c) => c,
        };
        match c {
            '[' => {
                parse_set(source)?;
                subpattern.push(Node::Unit);
            }
            '*' | '+' | '?' | '{' => {
                if !parse_repeat(source, &mut subpattern, c)? {
                    subpattern.push(Node::Unit);
                }
            }
            '.' => subpattern.push(Node::Unit),
            '(' => {
                if let Some(node) =
                    parse_group(source, state, &mut verbose, nested, first, &subpattern)?
                {
                    subpattern.push(node);
                }
            }
            '^' | '$' => subpattern.push(Node::At),
            _ => subpattern.push(Node::Unit),
        }
    }
    // Unpack non-capturing groups.
    let mut unpacked = Vec::with_capacity(subpattern.len());
    for node in subpattern {
        match node {
            Node::Group {
                group: None,
                add_flags: 0,
                del_flags: 0,
                p,
            } => unpacked.extend(p),
            other => unpacked.push(other),
        }
    }
    Ok(unpacked)
}

/// A `[...]` set, the `[` already read.
fn parse_set(source: &mut Tokenizer) -> Res<()> {
    let here = source.tell() - 1;
    let mut nonempty = false;
    source.matches('^')?;
    loop {
        let Some(this) = source.get()? else {
            return Err(source.error("unterminated character set", source.tell() - here));
        };
        if this.is(']') && nonempty {
            break;
        }
        let code1 = match this {
            Tok::Esc(c) => class_escape(source, c)?,
            Tok::Char(c) => ClassCode::Literal(c as u32),
        };
        if source.matches('-')? {
            let Some(that) = source.get()? else {
                return Err(source.error("unterminated character set", source.tell() - here));
            };
            if that.is(']') {
                // `[a-]`: the `-` is a literal, and the set ends.
                break;
            }
            let code2 = match that {
                Tok::Esc(c) => class_escape(source, c)?,
                Tok::Char(c) => ClassCode::Literal(c as u32),
            };
            let bad = || {
                let msg = format!("bad character range {}-{}", this.text(), that.text());
                source.error(&msg, this.len() + 1 + that.len())
            };
            match (code1, code2) {
                (ClassCode::Literal(lo), ClassCode::Literal(hi)) if hi >= lo => {}
                _ => return Err(bad()),
            }
        }
        nonempty = true;
    }
    Ok(())
}

/// A repeat `c` applying to the last item. `Ok(false)` when a `{` turned out
/// not to start one and is a literal.
fn parse_repeat(source: &mut Tokenizer, subpattern: &mut Vec<Node>, c: char) -> Res<bool> {
    let here = source.tell();
    let (min, max) = match c {
        '?' => (0, 1),
        '*' => (0, MAXREPEAT),
        '+' => (1, MAXREPEAT),
        _ => {
            if source.next == Some(Tok::Char('}')) {
                return Ok(false);
            }
            let (mut lo, mut hi) = (String::new(), String::new());
            while let Some(d) = source.next_char().filter(|&d| is_digit(d)) {
                lo.push(d);
                source.advance()?;
            }
            if source.matches(',')? {
                while let Some(d) = source.next_char().filter(|&d| is_digit(d)) {
                    hi.push(d);
                    source.advance()?;
                }
            } else {
                hi = lo.clone();
            }
            if !source.matches('}')? {
                source.seek(here)?;
                return Ok(false);
            }
            let mut min = 0;
            let mut max = MAXREPEAT;
            if !lo.is_empty() {
                min = decimal(&lo);
                if min >= MAXREPEAT {
                    return Err(Refusal::Overflow);
                }
            }
            if !hi.is_empty() {
                max = decimal(&hi);
                if max >= MAXREPEAT {
                    return Err(Refusal::Overflow);
                }
                if max < min {
                    return Err(
                        source.error("min repeat greater than max repeat", source.tell() - here)
                    );
                }
            }
            (min, max)
        }
    };
    let at = source.tell() - here + 1;
    match subpattern.last() {
        None | Some(Node::At) => return Err(source.error("nothing to repeat", at)),
        Some(n) if n.is_repeat() => return Err(source.error("multiple repeat", at)),
        Some(_) => {}
    }
    let item = match subpattern.pop() {
        Some(Node::Group {
            group: None,
            add_flags: 0,
            del_flags: 0,
            p,
        }) => p,
        Some(node) => vec![node],
        None => Vec::new(),
    };
    // `?` makes it lazy and `+` possessive; neither changes what is checked.
    if !source.matches('?')? {
        source.matches('+')?;
    }
    subpattern.push(Node::Repeat { min, max, item });
    Ok(true)
}

/// A `(` construct, the `(` already read. `Ok(None)` for one that adds no
/// item (a comment, global flags).
fn parse_group(
    source: &mut Tokenizer,
    state: &mut State,
    verbose: &mut bool,
    nested: usize,
    first: bool,
    subpattern: &[Node],
) -> Res<Option<Node>> {
    let start = source.tell() - 1;
    let mut capture = true;
    let mut atomic = false;
    let mut name: Option<String> = None;
    let (mut add_flags, mut del_flags) = (0, 0);
    let unterminated = |source: &Tokenizer| {
        source.error("missing ), unterminated subpattern", source.tell() - start)
    };
    if source.matches('?')? {
        let Some(char) = source.get()? else {
            return Err(source.error("unexpected end of pattern", 0));
        };
        match char {
            Tok::Char('P') => {
                if source.matches('<')? {
                    let n = source.getuntil('>', "group name")?;
                    source.checkgroupname(&n, 1)?;
                    name = Some(n);
                } else if source.matches('=')? {
                    let n = source.getuntil(')', "group name")?;
                    source.checkgroupname(&n, 1)?;
                    let len = n.chars().count();
                    let Some(gid) = state.group_named(&n) else {
                        return Err(
                            source.error(&format!("unknown group name {}", repr(&n)), len + 1)
                        );
                    };
                    if !state.checkgroup(gid) {
                        return Err(source.error("cannot refer to an open group", len + 1));
                    }
                    state.checklookbehindgroup(gid, source)?;
                    return Ok(Some(Node::GroupRef(gid)));
                } else {
                    let Some(char) = source.get()? else {
                        return Err(source.error("unexpected end of pattern", 0));
                    };
                    let msg = format!("unknown extension ?P{}", char.text());
                    return Err(source.error(&msg, char.len() + 2));
                }
            }
            Tok::Char(':') => capture = false,
            Tok::Char('#') => {
                loop {
                    if source.next.is_none() {
                        return Err(
                            source.error("missing ), unterminated comment", source.tell() - start)
                        );
                    }
                    if source.get()?.is_some_and(|t| t.is(')')) {
                        break;
                    }
                }
                return Ok(None);
            }
            Tok::Char(c @ ('=' | '!' | '<')) => {
                let mut kind = c;
                let mut dir = 1;
                let mut outer_lookbehind = None;
                if c == '<' {
                    let Some(char) = source.get()? else {
                        return Err(source.error("unexpected end of pattern", 0));
                    };
                    if !char.is_in("=!") {
                        let msg = format!("unknown extension ?<{}", char.text());
                        return Err(source.error(&msg, char.len() + 2));
                    }
                    kind = if char.is('=') { '=' } else { '!' };
                    dir = -1;
                    outer_lookbehind = Some(state.lookbehindgroups);
                    if state.lookbehindgroups.is_none() {
                        state.lookbehindgroups = Some(state.groups());
                    }
                }
                let p = parse_sub(source, state, *verbose, nested + 1)?;
                if outer_lookbehind == Some(None) {
                    state.lookbehindgroups = None;
                }
                if !source.matches(')')? {
                    return Err(unterminated(source));
                }
                if kind == '!' && p.is_empty() {
                    return Ok(Some(Node::Failure));
                }
                return Ok(Some(Node::Assert { dir, p }));
            }
            Tok::Char('(') => {
                let condname = source.getuntil(')', "group name")?;
                let len = condname.chars().count();
                let condgroup = if !condname.chars().all(|c| c.is_ascii_digit()) {
                    source.checkgroupname(&condname, 1)?;
                    match state.group_named(&condname) {
                        Some(g) => g as u128,
                        None => {
                            let msg = format!("unknown group name {}", repr(&condname));
                            return Err(source.error(&msg, len + 1));
                        }
                    }
                } else {
                    let g = decimal(&condname);
                    if g == 0 {
                        return Err(source.error("bad group number", len + 1));
                    }
                    if g >= MAXGROUPS {
                        return Err(source.error(&format!("invalid group reference {g}"), len + 1));
                    }
                    if !state.grouprefpos.iter().any(|&(n, _)| n == g) {
                        state.grouprefpos.push((g, source.tell() - len - 1));
                    }
                    g
                };
                state.checklookbehindgroup(condgroup as usize, source)?;
                let yes = parse(source, state, *verbose, nested + 1, false)?;
                let no = if source.matches('|')? {
                    let no = parse(source, state, *verbose, nested + 1, false)?;
                    if source.next == Some(Tok::Char('|')) {
                        return Err(
                            source.error("conditional backref with more than two branches", 0)
                        );
                    }
                    Some(no)
                } else {
                    None
                };
                if !source.matches(')')? {
                    return Err(unterminated(source));
                }
                return Ok(Some(Node::GroupRefExists { yes, no }));
            }
            Tok::Char('>') => {
                capture = false;
                atomic = true;
            }
            Tok::Char(c) if c == '-' || flag_bit(c).is_some() => {
                match parse_flags(source, state, c)? {
                    None => {
                        if !first || !subpattern.is_empty() {
                            return Err(source.error(
                                "global flags not at the start of the expression",
                                source.tell() - start,
                            ));
                        }
                        *verbose = state.flags & FLAG_VERBOSE != 0;
                        return Ok(None);
                    }
                    Some((add, del)) => {
                        add_flags = add;
                        del_flags = del;
                        capture = false;
                    }
                }
            }
            other => {
                let msg = format!("unknown extension ?{}", other.text());
                return Err(source.error(&msg, other.len() + 1));
            }
        }
    }
    let group = if capture {
        let n = name.as_deref();
        match state.opengroup(n) {
            Ok(g) => Some(g),
            Err(msg) => return Err(source.error(&msg, n.map_or(0, |n| n.chars().count()) + 1)),
        }
    } else {
        None
    };
    let sub_verbose = (*verbose || add_flags & FLAG_VERBOSE != 0) && del_flags & FLAG_VERBOSE == 0;
    let p = parse_sub(source, state, sub_verbose, nested + 1)?;
    if !source.matches(')')? {
        return Err(unterminated(source));
    }
    if let Some(g) = group {
        state.closegroup(g, &p);
    }
    Ok(Some(if atomic {
        Node::AtomicGroup(p)
    } else {
        Node::Group {
            group,
            add_flags,
            del_flags,
            p,
        }
    }))
}

/// `_parse_flags`: `None` for global flags `(?i)`, else the scoped
/// `(add, del)` of `(?i-s:...)`.
fn parse_flags(
    source: &mut Tokenizer,
    state: &mut State,
    mut char: char,
) -> Res<Option<(i64, i64)>> {
    let (mut add_flags, mut del_flags) = (0, 0);
    // The token after the flag letters, as `_parse_flags` reads it.
    let mut tok = Tok::Char(char);
    if char != '-' {
        loop {
            let flag = flag_bit(char).unwrap_or(0);
            if source.istext {
                if char == 'L' {
                    return Err(source.error(
                        "bad inline flags: cannot use 'L' flag with a str pattern",
                        0,
                    ));
                }
            } else if char == 'u' {
                return Err(source.error(
                    "bad inline flags: cannot use 'u' flag with a bytes pattern",
                    0,
                ));
            }
            add_flags |= flag;
            if flag & TYPE_FLAGS != 0 && add_flags & TYPE_FLAGS != flag {
                return Err(source.error(
                    "bad inline flags: flags 'a', 'u' and 'L' are incompatible",
                    0,
                ));
            }
            let Some(next) = source.get()? else {
                return Err(source.error("missing -, : or )", 0));
            };
            tok = next;
            if tok.is_in(")-:") {
                break;
            }
            match tok {
                Tok::Char(c) if flag_bit(c).is_some() => char = c,
                _ => {
                    let msg = if is_alpha(tok) {
                        "unknown flag"
                    } else {
                        "missing -, : or )"
                    };
                    return Err(source.error(msg, tok.len()));
                }
            }
        }
    }
    if tok.is(')') {
        state.flags |= add_flags;
        return Ok(None);
    }
    if add_flags & GLOBAL_FLAGS != 0 {
        return Err(source.error("bad inline flags: cannot turn on global flag", 1));
    }
    if tok.is('-') {
        let Some(next) = source.get()? else {
            return Err(source.error("missing flag", 0));
        };
        let mut c = match next {
            Tok::Char(c) if flag_bit(c).is_some() => c,
            _ => {
                let msg = if is_alpha(next) {
                    "unknown flag"
                } else {
                    "missing flag"
                };
                return Err(source.error(msg, next.len()));
            }
        };
        loop {
            let flag = flag_bit(c).unwrap_or(0);
            if flag & TYPE_FLAGS != 0 {
                return Err(source.error(
                    "bad inline flags: cannot turn off flags 'a', 'u' and 'L'",
                    0,
                ));
            }
            del_flags |= flag;
            let Some(next) = source.get()? else {
                return Err(source.error("missing :", 0));
            };
            if next.is(':') {
                break;
            }
            match next {
                Tok::Char(n) if flag_bit(n).is_some() => c = n,
                _ => {
                    let msg = if is_alpha(next) {
                        "unknown flag"
                    } else {
                        "missing :"
                    };
                    return Err(source.error(msg, next.len()));
                }
            }
        }
    }
    if del_flags & GLOBAL_FLAGS != 0 {
        return Err(source.error("bad inline flags: cannot turn off global flag", 1));
    }
    if add_flags & del_flags != 0 {
        return Err(source.error("bad inline flags: flag turned on and off", 1));
    }
    Ok(Some((add_flags, del_flags)))
}

/// `str.isalpha` of a token: a two-character escape token is alphabetic only
/// if both of its characters are, and `\` never is.
fn is_alpha(tok: Tok) -> bool {
    matches!(tok, Tok::Char(c) if c.is_alphabetic())
}

/// `fix_flags`: the flags a pattern of this kind ends up with.
fn fix_flags(istext: bool, mut flags: i64) -> Res<i64> {
    if istext {
        if flags & FLAG_LOCALE != 0 {
            return Err(Refusal::Value(
                "cannot use LOCALE flag with a str pattern".into(),
            ));
        }
        if flags & FLAG_ASCII == 0 {
            flags |= FLAG_UNICODE;
        } else if flags & FLAG_UNICODE != 0 {
            return Err(Refusal::Value(
                "ASCII and UNICODE flags are incompatible".into(),
            ));
        }
    } else {
        if flags & FLAG_UNICODE != 0 {
            return Err(Refusal::Value(
                "cannot use UNICODE flag with a bytes pattern".into(),
            ));
        }
        if flags & FLAG_LOCALE != 0 && flags & FLAG_ASCII != 0 {
            return Err(Refusal::Value(
                "ASCII and LOCALE flags are incompatible".into(),
            ));
        }
    }
    Ok(flags)
}

/// `_compiler._compile`'s look-behind check, in the order it compiles: the
/// first look-behind (outermost first, then left to right) whose pattern does
/// not have one fixed width.
fn check_lookbehinds(state: &State, p: &[Node]) -> Res<()> {
    for node in p {
        match node {
            Node::Assert { dir, p } => {
                if *dir < 0 {
                    let (lo, hi) = state.getwidth(p);
                    if lo > MAXCODE {
                        return Err(Refusal::Pattern {
                            msg: "looks too much behind".into(),
                            pos: None,
                        });
                    }
                    if lo != hi {
                        return Err(Refusal::Pattern {
                            msg: "look-behind requires fixed-width pattern".into(),
                            pos: None,
                        });
                    }
                }
                check_lookbehinds(state, p)?;
            }
            Node::Repeat { item: p, .. } | Node::Group { p, .. } | Node::AtomicGroup(p) => {
                check_lookbehinds(state, p)?
            }
            Node::Branch(items) => {
                for item in items {
                    check_lookbehinds(state, item)?;
                }
            }
            Node::GroupRefExists { yes, no } => {
                check_lookbehinds(state, yes)?;
                if let Some(no) = no {
                    check_lookbehinds(state, no)?;
                }
            }
            Node::Unit | Node::At | Node::GroupRef(_) | Node::Failure => {}
        }
    }
    Ok(())
}

/// Parse `pattern` (a `str`'s characters, or a `bytes` latin-1 decoded) with
/// `flags` as `re._parser.parse` and `re._compiler.compile` do, returning the
/// pattern's flags as `Pattern.flags` reports them — the inline global flags
/// folded in and `re.UNICODE` implied for a `str` — or why CPython refuses it.
pub fn check(pattern: &[char], istext: bool, flags: i64) -> Result<i64, Refusal> {
    let mut source = Tokenizer::new(pattern, istext)?;
    let mut state = State {
        flags,
        groupwidths: vec![None],
        ..State::default()
    };
    let p = parse_sub(&mut source, &mut state, flags & FLAG_VERBOSE != 0, 0)?;
    state.flags = fix_flags(istext, state.flags)?;
    if source.next.is_some() {
        return Err(source.error("unbalanced parenthesis", 0));
    }
    for &(g, pos) in &state.grouprefpos {
        if g >= state.groups() as u128 {
            return Err(Refusal::Pattern {
                msg: format!("invalid group reference {g}"),
                pos: Some(pos),
            });
        }
    }
    check_lookbehinds(&state, &p)?;
    Ok(state.flags)
}

/// The arguments of the `re.PatternError` last raised by [`raise`], kept until
/// `synth_exc` builds the exception from its rendered line (the same hand-off
/// `excunicode` uses): `(line, msg, pos)`.
pub struct PendingPatternError {
    pub msg: String,
    pub pattern: Vec<char>,
    pub istext: bool,
    pub pos: Option<usize>,
}

thread_local! {
    static PENDING: RefCell<Option<(String, PendingPatternError)>> = const { RefCell::new(None) };
}

/// The error line for `refusal` of `pattern`, recording a `PatternError`'s
/// arguments so the exception is built with its `msg`/`pattern`/`pos`.
pub fn raise(refusal: Refusal, pattern: &[char], istext: bool) -> String {
    match refusal {
        Refusal::Overflow => "OverflowError: the repetition number is too large".into(),
        Refusal::Value(msg) => format!("ValueError: {msg}"),
        Refusal::Pattern { msg, pos } => {
            let line = format!("re.PatternError: {}", render(&msg, pattern, pos));
            let args = PendingPatternError {
                msg,
                pattern: pattern.to_vec(),
                istext,
                pos,
            };
            PENDING.with(|p| *p.borrow_mut() = Some((line.clone(), args)));
            line
        }
    }
}

/// `PatternError.__init__`'s message: `msg`, then ` at position N`, then
/// ` (line L, column C)` when the pattern spans lines.
fn render(msg: &str, pattern: &[char], pos: Option<usize>) -> String {
    let Some(pos) = pos else {
        return msg.to_string();
    };
    let mut out = format!("{msg} at position {pos}");
    if pattern.contains(&'\n') {
        let before = &pattern[..pos.min(pattern.len())];
        let line = before.iter().filter(|&&c| c == '\n').count() + 1;
        let col = before.iter().rev().take_while(|&&c| c != '\n').count() + 1;
        out.push_str(&format!(" (line {line}, column {col})"));
    }
    out
}

/// The arguments recorded for `line`, consumed.
pub fn take_pending(line: &str) -> Option<PendingPatternError> {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        match &*p {
            Some((l, _)) if l == line => p.take().map(|(_, a)| a),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(p: &str) -> Refusal {
        let chars: Vec<char> = p.chars().collect();
        check(&chars, true, 0).expect_err(p)
    }

    fn at(msg: &str, pos: usize) -> Refusal {
        Refusal::Pattern {
            msg: msg.into(),
            pos: Some(pos),
        }
    }

    // Expected values are CPython 3.14's (`re.compile(p)` → `e.msg`, `e.pos`).
    #[test]
    fn refusals_match_sre_parse() {
        assert_eq!(refusal("("), at("missing ), unterminated subpattern", 0));
        assert_eq!(refusal("a)"), at("unbalanced parenthesis", 1));
        assert_eq!(refusal("a**"), at("multiple repeat", 2));
        assert_eq!(refusal("x{2}{3}"), at("multiple repeat", 4));
        assert_eq!(refusal("^*"), at("nothing to repeat", 1));
        assert_eq!(
            refusal("a{2,1}"),
            at("min repeat greater than max repeat", 2)
        );
        assert_eq!(refusal("[z-a]"), at("bad character range z-a", 1));
        assert_eq!(refusal("[\\d-z]"), at("bad character range \\d-z", 1));
        assert_eq!(
            refusal("(?P<1>a)"),
            at("bad character in group name '1'", 4)
        );
        assert_eq!(
            refusal("(?P<a>x)(?P<a>y)"),
            at("redefinition of group name 'a' as group 2; was group 1", 12)
        );
        assert_eq!(
            refusal("(?(1)a|b|c)"),
            at("conditional backref with more than two branches", 8)
        );
        assert_eq!(refusal("(?(2)a)(b)"), at("invalid group reference 2", 3));
        assert_eq!(
            refusal("\\777"),
            at("octal escape value \\777 outside of range 0-0o377", 0)
        );
        assert_eq!(refusal("(a)\\2"), at("invalid group reference 2", 4));
        assert_eq!(refusal("(?ix-s"), at("missing :", 6));
        assert_eq!(
            refusal("a(?i)"),
            at("global flags not at the start of the expression", 1)
        );
        assert_eq!(refusal("\\"), at("bad escape (end of pattern)", 0));
        assert_eq!(refusal("a{99999999999}"), Refusal::Overflow);
        assert_eq!(
            refusal("(?<!a*)"),
            Refusal::Pattern {
                msg: "look-behind requires fixed-width pattern".into(),
                pos: None
            }
        );
    }

    #[test]
    fn accepted_patterns_report_their_flags() {
        let chars: Vec<char> = "(?i)a{,3}[]-]\\1?(x)(?<=(a))\\2".chars().collect();
        assert!(
            check(&chars, true, 0).is_err(),
            "\\1 before group 1 closes is an error"
        );
        let ok: Vec<char> = "(?i)(a)b{,3}[]-](?<=\\1)".chars().collect();
        assert_eq!(check(&ok, true, 0), Ok(2 | FLAG_UNICODE));
        assert_eq!(check(&['a'], false, 0), Ok(0));
    }
}

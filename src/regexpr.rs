//! The regex engine behind `re`.
//!
//! Two engines, one API. `regex` is a finite-automaton matcher with linear-time
//! guarantees and no backtracking — which is exactly why it cannot do look-around
//! or backreferences. Python's `re` has both, and the stdlib uses them: the number
//! grammars in `_pydecimal` and `fractions` are written with `(?=\d|\.\d)`, so
//! `import decimal` failed outright on a lookahead-free engine.
//!
//! So: compile with `regex` first and keep its speed for the overwhelming majority
//! of patterns; fall back to `fancy_regex` (a backtracking engine layered over the
//! same crate) only for the patterns `regex` rejects. The fallback is per-pattern
//! and decided once, at compile time — matching never pays for the check.

/// A compiled pattern: the engine that took it, plus the pattern-text position
/// of every capture group's closing paren (see [`PyRegex::last_closed_group`]).
pub struct PyRegex {
    engine: Engine,
    /// `closes[g]` is the char index of group `g`'s `)` in the compiled
    /// pattern; index 0 (the whole match) is unused.
    closes: Vec<usize>,
}

/// Whichever engine could take the pattern.
enum Engine {
    Fast(regex::Regex),
    Fancy(fancy_regex::Regex),
}

/// The capture spans of one match: byte `(start, end)` per group, `None` for a
/// group that did not participate. Index 0 is the whole match.
pub type Spans = Vec<Option<(usize, usize)>>;

// ── the byte/codepoint boundary ─────────────────────────────────────────────
//
// Both engines index a `&str` by BYTE, and every span, slice and `pos` inside
// this crate's `re` implementation is a byte offset — which is what the slicing
// needs. CPython's `re` indexes a `str` by CODEPOINT: `re.search('b', 'éb')`
// reports `span() == (1, 2)`, not `(2, 3)`.
//
// The two agree on ASCII and only on ASCII, so the difference is invisible to
// an ASCII-only test and silently wrong on every other subject. The conversion
// therefore happens exactly at the boundary where a position becomes visible to
// Python (`start`/`end`/`span`/`pos`/`endpos`/the `repr`) or arrives from it
// (the `pos`/`endpos` arguments), and NOWHERE else: the stored spans stay byte
// offsets so the slicing that reads them stays correct.

/// The codepoint index of byte offset `byte` in `text` — the number CPython
/// reports for that position. `byte` is a char boundary for anything the engines
/// produce; a stray interior offset counts the characters before it.
pub fn char_index_of(text: &str, byte: usize) -> usize {
    match text.get(..byte) {
        Some(prefix) => prefix.chars().count(),
        // Not on a boundary (or past the end): count what is reachable.
        None => text
            .char_indices()
            .take_while(|(i, _)| *i < byte)
            .count()
            .min(text.chars().count()),
    }
}

/// The byte offset of codepoint index `ch` in `text` — the inverse of
/// [`char_index_of`], clamped to the end so an out-of-range `pos` argument
/// behaves like CPython's (which clamps to `len(string)`).
pub fn byte_index_of(text: &str, ch: usize) -> usize {
    text.char_indices()
        .nth(ch)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

impl PyRegex {
    /// Compile `pattern`, preferring the non-backtracking engine. If `regex`
    /// rejects it, retry on `fancy_regex`; if that fails too, report the FIRST
    /// error — the fast engine's message describes the pattern the caller wrote,
    /// while the fallback's would describe a construct it also could not parse.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let engine = match regex::Regex::new(pattern) {
            Ok(re) => Engine::Fast(re),
            Err(fast_err) => match fancy_regex::Regex::new(&fancy_spelling(pattern)) {
                Ok(re) => Engine::Fancy(re),
                Err(_) => return Err(last_line(&fast_err.to_string())),
            },
        };
        Ok(PyRegex {
            engine,
            closes: group_closes(pattern),
        })
    }

    /// Number of capture groups plus one (group 0, the whole match).
    pub fn captures_len(&self) -> usize {
        match &self.engine {
            Engine::Fast(re) => re.captures_len(),
            Engine::Fancy(re) => re.captures_len(),
        }
    }

    /// `(name, index)` for each named group, in pattern order.
    pub fn named_groups(&self) -> Vec<(String, usize)> {
        match &self.engine {
            Engine::Fast(re) => named_from(re.capture_names()),
            Engine::Fancy(re) => named_from(re.capture_names()),
        }
    }

    /// Spans of the leftmost match starting at or after byte `start`, searched
    /// with ALL of `text` in view. That is what CPython's `pos` argument means:
    /// `^`, `\b` and look-behind still see the characters before `start`, so
    /// `re.compile('^a').search('ba', 1)` is `None` where searching the slice
    /// `'a'` would have matched.
    pub fn captures_at(&self, text: &str, start: usize) -> Option<Spans> {
        let n = self.captures_len();
        match &self.engine {
            Engine::Fast(re) => re.captures_at(text, start).map(|c| collect_spans(&c, n)),
            // A backtracking match can fail at runtime (catastrophic backtracking
            // hits the step limit); treat that as "no match" rather than a panic.
            Engine::Fancy(re) => re
                .captures_from_pos(text, start)
                .ok()
                .flatten()
                .map(|c| collect_fancy_spans(&c, n)),
        }
    }

    /// One step of a left-to-right scan: the leftmost match at or after byte
    /// `pos`, under CPython's (3.7+) rule for empty matches — the one
    /// `findall`, `finditer`, `sub`, `split` and the `scanner` object all share
    /// in `_sre.c`. `must_advance` is `_sre.c`'s flag of that name, set when
    /// the previous match was EMPTY: the next one may then not be empty at the
    /// same position. After a NON-empty match an empty one may sit right where
    /// it ended, which both engines' own iterators refuse, so
    /// `re.sub('x*', '-', 'abxd')` gave `-a-b-d-` where CPython gives `-a-b--d-`.
    pub fn search_from(&self, text: &str, pos: usize, must_advance: bool) -> Option<Spans> {
        let spans = self.captures_at(text, pos)?;
        let (s, e) = spans.first().copied().flatten()?;
        if !(must_advance && s == pos && e == pos) {
            return Some(spans);
        }
        // The engines cannot be asked for "a match that is not empty here", so
        // resume one character on — the same stand-in their own iterators use.
        let next = pos + text[pos..].chars().next()?.len_utf8();
        self.captures_at(text, next)
    }

    /// Spans of every non-overlapping match, left to right (see
    /// [`PyRegex::search_from`] for the empty-match rule).
    pub fn all_captures(&self, text: &str) -> Vec<Spans> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        let mut must_advance = false;
        while pos <= text.len() {
            let Some(spans) = self.search_from(text, pos, must_advance) else {
                break;
            };
            let Some((s, e)) = spans.first().copied().flatten() else {
                break;
            };
            must_advance = s == e;
            pos = e;
            out.push(spans);
        }
        out
    }

    /// `Match.lastindex`: the capture group that CLOSED last in the match, or
    /// `None` when no group took part.
    ///
    /// `_sre.c` records it as the match runs — every time a group's closing
    /// mark is set, that group becomes `lastindex` — so for nested groups the
    /// OUTER one wins (`re.match('((a)b)', 'ab').lastindex` is 1, not 2): the
    /// inner group closes first. Neither engine reports the order in which
    /// groups closed, so it is reconstructed from the result: a group that
    /// ends later closed later, and between groups that end at the same
    /// position the one whose `)` comes later in the pattern closed later
    /// (an enclosing group's `)` follows every `)` nested inside it, and of two
    /// sibling groups the second closes second). That reconstruction is exact
    /// for a match that moves forward through the subject; a group closed
    /// inside a look-ahead is the case it cannot see, since the look-ahead
    /// reaches past positions the match closes later groups at.
    pub fn last_closed_group(&self, spans: &Spans) -> Option<usize> {
        spans
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(g, span)| span.map(|(_, end)| (end, self.closes.get(g).copied(), g)))
            .max()
            .map(|(_, _, g)| g)
    }
}

/// The char index of each capture group's closing `)` in an engine pattern,
/// indexed by group number (index 0, the whole match, holds 0).
///
/// Group numbers are assigned the way both engines and CPython assign them:
/// by the position of the OPENING paren, counting only capturing groups — a
/// bare `(`, `(?P<name>` or `(?<name>`. Everything that cannot open a group is
/// stepped over: a backslash escape, a character class (including a class
/// nested inside one, which the crate's syntax allows), a `(?#…)` comment and,
/// while the `x` flag is on, a `#` comment running to the end of the line.
fn group_closes(pattern: &str) -> Vec<usize> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut closes = vec![0];
    // One entry per open paren: the capture group it opened (if any) and the
    // verbose state to restore when it closes, so `(?x:…)` stays scoped.
    let mut open: Vec<(Option<usize>, bool)> = Vec::new();
    let mut verbose = false;
    let mut class_depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => i += 1,
            '[' => {
                class_depth += 1;
                // A `]` right after the open (or after `^`) is a member.
                if chars.get(i + 1) == Some(&'^') {
                    i += 1;
                }
                if chars.get(i + 1) == Some(&']') {
                    i += 1;
                }
            }
            ']' if class_depth > 0 => class_depth -= 1,
            _ if class_depth > 0 => {}
            '#' if verbose => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '(' if chars.get(i + 1) == Some(&'?') => {
                let rest: String = chars[i + 2..].iter().take(3).collect();
                if rest.starts_with('#') {
                    while i < chars.len() && chars[i] != ')' {
                        i += 1;
                    }
                } else if rest.starts_with("P<")
                    || (rest.starts_with('<') && !rest[1..].starts_with(['=', '!']))
                {
                    closes.push(0);
                    open.push((Some(closes.len() - 1), verbose));
                } else {
                    // An inline-flag group: `(?x)` turns the flag on for the rest
                    // of the enclosing group, `(?x:…)` only inside its own.
                    let flags: String = chars[i + 2..]
                        .iter()
                        .take_while(|ch| ch.is_ascii_alphabetic() || **ch == '-')
                        .collect();
                    let after = chars.get(i + 2 + flags.chars().count());
                    let sets_x = flags.split('-').next().is_some_and(|on| on.contains('x'));
                    let clears_x = flags.split('-').nth(1).is_some_and(|off| off.contains('x'));
                    let flagged = (verbose || sets_x) && !clears_x;
                    if after == Some(&')') {
                        // Applies to the enclosing group from here on; the paren
                        // itself opens and closes nothing.
                        verbose = flagged;
                        i += 2 + flags.chars().count();
                    } else {
                        open.push((None, verbose));
                        if after == Some(&':') {
                            verbose = flagged;
                        }
                    }
                }
            }
            '(' => {
                closes.push(0);
                open.push((Some(closes.len() - 1), verbose));
            }
            ')' => {
                if let Some((group, outer_verbose)) = open.pop() {
                    if let Some(g) = group {
                        closes[g] = i;
                    }
                    verbose = outer_verbose;
                }
            }
            _ => {}
        }
        i += 1;
    }
    closes
}

/// The pattern as `fancy_regex` must be given it. The ASCII rewrite in
/// `builtins.rs` spells `re.ASCII`'s word boundaries as the crate's
/// `(?-u:\b)`/`(?-u:\B)`, and `fancy_regex` cannot turn Unicode off, so for
/// it they become the equivalent look-around over the ASCII word class.
/// Python has no `(?-u:` syntax, so these sequences come only from that
/// rewrite.
fn fancy_spelling(pattern: &str) -> std::borrow::Cow<'_, str> {
    if !pattern.contains("(?-u:") {
        return std::borrow::Cow::Borrowed(pattern);
    }
    const W: &str = "[0-9A-Za-z_]";
    let boundary = format!("(?:(?<={W})(?!{W})|(?<!{W})(?={W}))");
    let not_boundary = format!("(?:(?<={W})(?={W})|(?<!{W})(?!{W}))");
    let mut spelled = pattern
        .replace("(?-u:\\b)", &boundary)
        .replace("(?-u:\\B)", &not_boundary);
    // The `re.ASCII` spelling of a `k`/`s` literal: the plain letter, whose
    // case folding here is the crate's Unicode one.
    for letter in ["k", "K", "s", "S"] {
        spelled = spelled.replace(&format!("(?-u:{letter})"), letter);
    }
    std::borrow::Cow::Owned(spelled)
}

fn named_from<'a>(names: impl Iterator<Item = Option<&'a str>>) -> Vec<(String, usize)> {
    names
        .enumerate()
        .filter_map(|(i, n)| n.map(|n| (n.to_string(), i)))
        .collect()
}

fn collect_spans(caps: &regex::Captures<'_>, n: usize) -> Spans {
    (0..n)
        .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
        .collect()
}

fn collect_fancy_spans(caps: &fancy_regex::Captures<'_>, n: usize) -> Spans {
    (0..n)
        .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
        .collect()
}

/// The `regex` crate's errors are multi-line diagrams; `re.error` is one line.
fn last_line(msg: &str) -> String {
    msg.lines()
        .last()
        .unwrap_or("bad pattern")
        .trim()
        .to_string()
}

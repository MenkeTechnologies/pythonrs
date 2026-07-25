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

/// A compiled pattern, on whichever engine could take it.
pub enum PyRegex {
    Fast(regex::Regex),
    Fancy(fancy_regex::Regex),
}

/// The capture spans of one match: byte `(start, end)` per group, `None` for a
/// group that did not participate. Index 0 is the whole match.
pub type Spans = Vec<Option<(usize, usize)>>;

impl PyRegex {
    /// Compile `pattern`, preferring the non-backtracking engine. If `regex`
    /// rejects it, retry on `fancy_regex`; if that fails too, report the FIRST
    /// error — the fast engine's message describes the pattern the caller wrote,
    /// while the fallback's would describe a construct it also could not parse.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let fast_err = match regex::Regex::new(pattern) {
            Ok(re) => return Ok(PyRegex::Fast(re)),
            Err(e) => e,
        };
        match fancy_regex::Regex::new(pattern) {
            Ok(re) => Ok(PyRegex::Fancy(re)),
            Err(_) => Err(last_line(&fast_err.to_string())),
        }
    }

    /// Number of capture groups plus one (group 0, the whole match).
    pub fn captures_len(&self) -> usize {
        match self {
            PyRegex::Fast(re) => re.captures_len(),
            PyRegex::Fancy(re) => re.captures_len(),
        }
    }

    /// `(name, index)` for each named group, in pattern order.
    pub fn named_groups(&self) -> Vec<(String, usize)> {
        match self {
            PyRegex::Fast(re) => named_from(re.capture_names()),
            PyRegex::Fancy(re) => named_from(re.capture_names()),
        }
    }

    /// Spans of the leftmost match at or after the start of `text`.
    pub fn first_captures(&self, text: &str) -> Option<Spans> {
        let n = self.captures_len();
        match self {
            PyRegex::Fast(re) => re.captures(text).map(|c| collect_spans(&c, n)),
            // A backtracking match can fail at runtime (catastrophic backtracking
            // hits the step limit); treat that as "no match" rather than a panic.
            PyRegex::Fancy(re) => re
                .captures(text)
                .ok()
                .flatten()
                .map(|c| collect_fancy_spans(&c, n)),
        }
    }

    /// Spans of every non-overlapping match, left to right.
    pub fn all_captures(&self, text: &str) -> Vec<Spans> {
        let n = self.captures_len();
        match self {
            PyRegex::Fast(re) => re.captures_iter(text).map(|c| collect_spans(&c, n)).collect(),
            PyRegex::Fancy(re) => re
                .captures_iter(text)
                .filter_map(|c| c.ok())
                .map(|c| collect_fancy_spans(&c, n))
                .collect(),
        }
    }

    /// Split on the pattern. `limit` is the maximum number of PIECES (as
    /// `re.split`'s `maxsplit + 1`); `None` splits on every match.
    pub fn split_n(&self, text: &str, limit: Option<usize>) -> Vec<String> {
        match (self, limit) {
            (PyRegex::Fast(re), Some(n)) => re.splitn(text, n).map(str::to_string).collect(),
            (PyRegex::Fast(re), None) => re.split(text).map(str::to_string).collect(),
            (PyRegex::Fancy(re), Some(n)) => re
                .splitn(text, n)
                .filter_map(|s| s.ok())
                .map(str::to_string)
                .collect(),
            (PyRegex::Fancy(re), None) => re
                .split(text)
                .filter_map(|s| s.ok())
                .map(str::to_string)
                .collect(),
        }
    }

    /// Replace the first `count` matches (all of them when `count` is `None`) with
    /// `repl`, whose `$1`/`$name` references expand as the engines' `Replacer` does.
    pub fn replace_n(&self, text: &str, count: Option<usize>, repl: &str) -> String {
        match (self, count) {
            (PyRegex::Fast(re), Some(n)) => re.replacen(text, n, repl).into_owned(),
            (PyRegex::Fast(re), None) => re.replace_all(text, repl).into_owned(),
            (PyRegex::Fancy(re), Some(n)) => re.replacen(text, n, repl).into_owned(),
            (PyRegex::Fancy(re), None) => re.replace_all(text, repl).into_owned(),
        }
    }

    /// How many non-overlapping matches `text` holds, capped at `limit`.
    pub fn count_matches(&self, text: &str, limit: Option<usize>) -> usize {
        match self {
            PyRegex::Fast(re) => match limit {
                Some(n) => re.find_iter(text).take(n).count(),
                None => re.find_iter(text).count(),
            },
            PyRegex::Fancy(re) => {
                let it = re.find_iter(text).filter_map(|m| m.ok());
                match limit {
                    Some(n) => it.take(n).count(),
                    None => it.count(),
                }
            }
        }
    }
}

fn named_from<'a>(names: impl Iterator<Item = Option<&'a str>>) -> Vec<(String, usize)> {
    names
        .enumerate()
        .filter_map(|(i, n)| n.map(|n| (n.to_string(), i)))
        .collect()
}

fn collect_spans(caps: &regex::Captures<'_>, n: usize) -> Spans {
    (0..n).map(|i| caps.get(i).map(|m| (m.start(), m.end()))).collect()
}

fn collect_fancy_spans(caps: &fancy_regex::Captures<'_>, n: usize) -> Spans {
    (0..n).map(|i| caps.get(i).map(|m| (m.start(), m.end()))).collect()
}

/// The `regex` crate's errors are multi-line diagrams; `re.error` is one line.
fn last_line(msg: &str) -> String {
    msg.lines().last().unwrap_or("bad pattern").trim().to_string()
}

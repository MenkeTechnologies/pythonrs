//! The `re` standard-library module: regular expressions, backed by the Rust
//! `regex` crate.
//!
//! Implemented functions: `findall`, `sub`, `split`, `match`, `search`,
//! `fullmatch`, `escape`. All accept `(pattern, string, …)` with the pattern and
//! string as the leading positional args.
//!
//! ## Match objects are approximated as tuples (this round)
//!
//! pythonrs has no `re.Match` host type yet, so `match` / `search` / `fullmatch`
//! return a **tuple of the matched groups** instead of a real Match object:
//!   - element `0` is the whole match (CPython's `m.group(0)`),
//!   - elements `1..` are the capture groups in order,
//!   - a group that did not participate in the match is `None` (`Value::Undef`).
//! On **no match** the functions return `None` (`Value::Undef`), matching the
//! falsy return of `re.match` etc.
//! Real Match objects with `.group(n)` / `.start()` / `.span()` / `.groupdict()`
//! are a later host-type pass. Until then callers must read groups by tuple index.
//!
//! ## Honest engine limitations (`regex` crate, not CPython's `sre`)
//!
//! The `regex` crate is a finite-automaton engine. It **does not support
//! backreferences** (`(a)\1`) or **lookaround** (`(?=…)`, `(?<=…)`, `(?!…)`,
//! `(?<!…)`) — patterns using them fail to compile and surface here as a
//! `re.error`. This is a real semantic gap, not faked: such patterns simply
//! cannot run on this engine. Named groups use the `(?P<name>…)` form (shared by
//! both CPython and the `regex` crate).
//!
//! Other approximations, called out so callers know the edges:
//!   - **Flags are ignored.** A trailing `flags=` positional (e.g.
//!     `re.match(pat, s, re.I)`) is not honored; inline flags in the pattern
//!     (`(?i)…`) are the supported path.
//!   - **`repl` must be a string.** A callable `repl` (CPython allows
//!     `re.sub(pat, fn, s)`) is unsupported this round and raises `TypeError`.
//!   - **Zero-width `split`** follows the `regex` crate's iteration (splits on
//!     empty matches like CPython ≥ 3.7), but exact empty-match adjacency rules
//!     may differ from `sre` in corner cases.
//!
//! Wiring (done by the parent): an `import_module` arm for `"re"` that calls
//! [`entries`], a `call_builtin_function` arm routing `re.*` to [`call`], and the
//! `re.` prefix added to `is_builtin_function`.

use crate::host::{type_error, PyHost};
use fusevm::Value;

/// The module namespace: each callable is a first-class `PyObj::Builtin` handle
/// the VM invokes back through [`call`].
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let names = [
        "findall",
        "sub",
        "split",
        "match",
        "search",
        "fullmatch",
        "escape",
    ];
    names
        .iter()
        .map(|n| {
            (
                (*n).to_string(),
                h.alloc(crate::host::PyObj::Builtin(format!("re.{n}"))),
            )
        })
        .collect()
}

/// Dispatch a `re.*` builtin. `fname` is already stripped of the `re.` prefix.
/// Returns `None` for names this module does not own.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    Some(match fname {
        "findall" => findall(h, args),
        "sub" => sub(h, args),
        "split" => split(h, args),
        "match" => search_like(h, args, Mode::Match),
        "search" => search_like(h, args, Mode::Search),
        "fullmatch" => search_like(h, args, Mode::Full),
        "escape" => escape(h, args),
        _ => return None,
    })
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// CPython raises `re.error` for bad patterns / bad group references; format the
/// message the way that exception prints.
fn re_error(msg: impl std::fmt::Display) -> String {
    format!("re.error: {msg}")
}

/// Pull a str out of a `Value`, accepting both the immediate `Value::Str` and a
/// heap `PyObj::Str`.
fn arg_str(h: &PyHost, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Str(s)) => Some((**s).clone()),
        Some(v) => h.as_str(v),
        None => None,
    }
}

/// Required string argument `pos` (0-based) of function `fname`.
fn req_str(h: &PyHost, args: &[Value], pos: usize, fname: &str, param: &str) -> Result<String, String> {
    match arg_str(h, args.get(pos)) {
        Some(s) => Ok(s),
        None => Err(type_error(&format!(
            "{fname}() argument '{param}' must be str"
        ))),
    }
}

/// Optional non-negative integer argument (`count` / `maxsplit`), default 0.
fn opt_int(h: &PyHost, args: &[Value], pos: usize) -> i64 {
    match args.get(pos) {
        None | Some(Value::Undef) => 0,
        Some(v) => h.as_int(v).unwrap_or(0),
    }
}

fn compile(pat: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(pat).map_err(re_error)
}

/// Number of explicit capture groups (excludes the whole-match group 0).
fn group_count(re: &regex::Regex) -> usize {
    re.captures_len().saturating_sub(1)
}

/// Build a match tuple `(group0, group1, …)` with `None` for groups that did not
/// participate — the approximate Match value documented in the file header.
fn build_match_tuple(h: &mut PyHost, caps: &regex::Captures) -> Value {
    let strs: Vec<Option<String>> = (0..caps.len())
        .map(|i| caps.get(i).map(|m| m.as_str().to_string()))
        .collect();
    let items: Vec<Value> = strs
        .into_iter()
        .map(|s| match s {
            Some(s) => h.new_str(s),
            None => Value::Undef,
        })
        .collect();
    h.new_tuple(items)
}

// ── findall ──────────────────────────────────────────────────────────────────

fn findall(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let pat = req_str(h, args, 0, "findall", "pattern")?;
    let text = req_str(h, args, 1, "findall", "string")?;
    let re = compile(&pat)?;
    let ng = group_count(&re);

    // Collect as plain Rust first (borrows `text`), then allocate — keeps the
    // `regex` borrow separate from the `&mut PyHost` allocations.
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for caps in re.captures_iter(&text) {
        if ng == 0 {
            // group 0 always participates for a produced match.
            rows.push(vec![caps.get(0).map(|m| m.as_str().to_string())]);
        } else {
            let row = (1..=ng)
                .map(|i| caps.get(i).map(|m| m.as_str().to_string()))
                .collect();
            rows.push(row);
        }
    }

    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        if ng <= 1 {
            // 0 groups → whole match; 1 group → that group. CPython uses "" for a
            // non-participating single group in findall.
            let s = row.into_iter().next().flatten().unwrap_or_default();
            out.push(h.new_str(s));
        } else {
            // ≥2 groups → a tuple per match; "" for non-participating groups.
            let items: Vec<Value> = row
                .into_iter()
                .map(|s| h.new_str(s.unwrap_or_default()))
                .collect();
            let t = h.new_tuple(items);
            out.push(t);
        }
    }
    Ok(h.new_list(out))
}

// ── sub ──────────────────────────────────────────────────────────────────────

fn sub(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let pat = req_str(h, args, 0, "sub", "pattern")?;
    // repl must be a string; a callable repl is not supported this round.
    let repl = match arg_str(h, args.get(1)) {
        Some(s) => s,
        None => {
            return Err(type_error(
                "sub() repl must be str (callable repl not supported yet)",
            ))
        }
    };
    let text = req_str(h, args, 2, "sub", "string")?;
    let count = opt_int(h, args, 3);
    let re = compile(&pat)?;
    let limit = if count <= 0 {
        usize::MAX
    } else {
        count as usize
    };

    let mut out = String::new();
    let mut last = 0usize;
    let mut done = 0usize;
    for caps in re.captures_iter(&text) {
        if done >= limit {
            break;
        }
        let m = caps.get(0).expect("group 0 present for a produced match");
        out.push_str(&text[last..m.start()]);
        out.push_str(&expand_repl(&repl, &caps)?);
        last = m.end();
        done += 1;
    }
    out.push_str(&text[last..]);
    Ok(h.new_str(out))
}

/// Expand a `sub` replacement template against one match: `\1`..`\99`,
/// `\g<n>` / `\g<name>`, `\\`, and the common char escapes. Unknown escapes keep
/// the backslash literally (an approximation of CPython, which errors on unknown
/// ASCII-letter escapes).
fn expand_repl(repl: &str, caps: &regex::Captures) -> Result<String, String> {
    let chars: Vec<char> = repl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1; // consume backslash
        if i >= chars.len() {
            // Trailing backslash: CPython errors; keep it literally.
            out.push('\\');
            break;
        }
        let n = chars[i];
        match n {
            '\\' => {
                out.push('\\');
                i += 1;
            }
            'n' => {
                out.push('\n');
                i += 1;
            }
            't' => {
                out.push('\t');
                i += 1;
            }
            'r' => {
                out.push('\r');
                i += 1;
            }
            'f' => {
                out.push('\u{0c}');
                i += 1;
            }
            'v' => {
                out.push('\u{0b}');
                i += 1;
            }
            'a' => {
                out.push('\u{07}');
                i += 1;
            }
            'b' => {
                out.push('\u{08}');
                i += 1;
            }
            'g' => {
                i += 1; // consume 'g'
                if chars.get(i) != Some(&'<') {
                    return Err(re_error("missing < in group name reference"));
                }
                i += 1; // consume '<'
                let mut name = String::new();
                while i < chars.len() && chars[i] != '>' {
                    name.push(chars[i]);
                    i += 1;
                }
                if chars.get(i) != Some(&'>') {
                    return Err(re_error("missing >, unterminated name in group reference"));
                }
                i += 1; // consume '>'
                out.push_str(&resolve_group(caps, &name)?);
            }
            d if d.is_ascii_digit() => {
                // Numeric backref: up to two digits (CPython's `\NN`).
                let mut num = String::new();
                num.push(d);
                i += 1;
                if i < chars.len() && chars[i].is_ascii_digit() {
                    num.push(chars[i]);
                    i += 1;
                }
                out.push_str(&resolve_group(caps, &num)?);
            }
            other => {
                // Unknown escape: preserve backslash + char.
                out.push('\\');
                out.push(other);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Resolve a `\g<…>` / `\NN` group reference to its matched text. A numeric
/// reference beyond the pattern's group count is a `re.error` (as in CPython); a
/// group that exists but did not participate expands to the empty string.
fn resolve_group(caps: &regex::Captures, name: &str) -> Result<String, String> {
    if let Ok(idx) = name.parse::<usize>() {
        if idx >= caps.len() {
            return Err(re_error(format!("invalid group reference {idx}")));
        }
        Ok(caps.get(idx).map(|m| m.as_str()).unwrap_or("").to_string())
    } else {
        match caps.name(name) {
            Some(m) => Ok(m.as_str().to_string()),
            // A named group that exists but didn't participate → "". A name that
            // isn't in the pattern at all → error.
            None => Err(re_error(format!("unknown group name {name:?}"))),
        }
    }
}

// ── split ────────────────────────────────────────────────────────────────────

fn split(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let pat = req_str(h, args, 0, "split", "pattern")?;
    let text = req_str(h, args, 1, "split", "string")?;
    let maxsplit = opt_int(h, args, 2);
    let re = compile(&pat)?;
    let ng = group_count(&re);
    let limit = if maxsplit <= 0 {
        usize::MAX
    } else {
        maxsplit as usize
    };

    // Gather (piece, [group texts]) segments while borrowing `text`.
    let mut pieces: Vec<String> = Vec::new();
    let mut group_rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut last = 0usize;
    let mut done = 0usize;
    for caps in re.captures_iter(&text) {
        if done >= limit {
            break;
        }
        let m = caps.get(0).expect("group 0 present for a produced match");
        pieces.push(text[last..m.start()].to_string());
        let row = (1..=ng)
            .map(|i| caps.get(i).map(|g| g.as_str().to_string()))
            .collect();
        group_rows.push(row);
        last = m.end();
        done += 1;
    }
    pieces.push(text[last..].to_string());

    // Interleave: piece, then that split's captured groups, … , final piece.
    let mut out: Vec<Value> = Vec::new();
    for (idx, piece) in pieces.iter().enumerate() {
        out.push(h.new_str(piece.clone()));
        if let Some(row) = group_rows.get(idx) {
            for g in row {
                match g {
                    Some(s) => {
                        let v = h.new_str(s.clone());
                        out.push(v);
                    }
                    None => out.push(Value::Undef),
                }
            }
        }
    }
    Ok(h.new_list(out))
}

// ── match / search / fullmatch ───────────────────────────────────────────────

enum Mode {
    /// Anchored at the start of the string (`re.match`).
    Match,
    /// Anywhere in the string (`re.search`).
    Search,
    /// Must span the whole string (`re.fullmatch`).
    Full,
}

fn search_like(h: &mut PyHost, args: &[Value], mode: Mode) -> Result<Value, String> {
    let fname = match mode {
        Mode::Match => "match",
        Mode::Search => "search",
        Mode::Full => "fullmatch",
    };
    let pat = req_str(h, args, 0, fname, "pattern")?;
    let text = req_str(h, args, 1, fname, "string")?;

    // `fullmatch` wraps the pattern in `\A(?:…)\z` so the automaton only accepts a
    // whole-string match (non-capturing group + zero-width anchors preserve group
    // numbering). `match` uses the bare pattern and requires the leftmost match to
    // begin at offset 0 — since the leftmost match is the earliest, a start of 0
    // is exactly CPython's start-anchored semantics.
    let re = match mode {
        Mode::Full => compile(&format!("\\A(?:{pat})\\z"))?,
        _ => compile(&pat)?,
    };

    let result = match re.captures(&text) {
        Some(caps) => {
            let anchored_ok = match mode {
                Mode::Match => caps.get(0).map(|m| m.start() == 0).unwrap_or(false),
                _ => true,
            };
            if anchored_ok {
                Some(build_match_tuple(h, &caps))
            } else {
                None
            }
        }
        None => None,
    };
    Ok(result.unwrap_or(Value::Undef))
}

// ── escape ───────────────────────────────────────────────────────────────────

/// `re.escape`: backslash-escape the characters CPython treats as special so the
/// string can be used as a literal in a pattern. Mirrors CPython's
/// `_special_chars_map` set.
fn escape(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let s = req_str(h, args, 0, "escape", "pattern")?;
    const SPECIAL: &[char] = &[
        '(', ')', '[', ']', '{', '}', '?', '*', '+', '-', '|', '^', '$', '\\', '.', '&', '~', '#',
        ' ', '\t', '\n', '\r', '\u{0b}', '\u{0c}',
    ];
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(h.new_str(out))
}

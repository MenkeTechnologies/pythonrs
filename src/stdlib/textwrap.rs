//! The `textwrap` standard-library module: `wrap`, `fill`, `shorten`, `indent`,
//! `dedent`.
//!
//! This is a greedy word-wrapper matching CPython's default `TextWrapper`
//! options (`expand_tabs=True`, `replace_whitespace=True`, `drop_whitespace=True`,
//! `break_long_words=True`). Two `TextWrapper` refinements are intentionally NOT
//! implemented (they need the regex chunker): `break_on_hyphens` (splitting on
//! intra-word hyphens) and `fix_sentence_endings`. Whitespace is collapsed to
//! single spaces and words are packed greedily up to `width`, breaking any word
//! longer than `width`.
//!
//! Wiring (done by the parent): an `import_module` arm for `"textwrap"` calling
//! [`entries`], and a `call_builtin_function` arm routing `textwrap.*` to
//! [`call`].

use crate::host::{type_error, PyHost, PyObj};
use fusevm::Value;

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    ["wrap", "fill", "shorten", "indent", "dedent"]
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                h.alloc(PyObj::Builtin(format!("textwrap.{name}"))),
            )
        })
        .collect()
}

pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "wrap" => Some(wrap(h, args)),
        "fill" => Some(fill(h, args)),
        "shorten" => Some(shorten(h, args)),
        "indent" => Some(indent(h, args)),
        "dedent" => Some(dedent(h, args)),
        _ => None,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn text_arg(h: &PyHost, args: &[Value], fname: &str) -> Result<String, String> {
    let v = args
        .first()
        .ok_or_else(|| type_error(&format!("{fname}() missing required argument: 'text'")))?;
    h.as_str(v)
        .ok_or_else(|| type_error(&format!("{fname}() requires a str, not '{}'", h.type_name(v))))
}

/// Optional positional `width` (`args[idx]`), defaulting to 70.
fn width_arg(h: &PyHost, args: &[Value], idx: usize) -> usize {
    args.get(idx)
        .and_then(|v| h.as_int(v))
        .map(|n| n.max(1) as usize)
        .unwrap_or(70)
}

/// Expand tabs to spaces (tabsize 8) and normalise every other whitespace run to
/// a single space, then split into words — CPython's `replace_whitespace` +
/// `expand_tabs` followed by whitespace splitting.
fn split_words(text: &str) -> Vec<String> {
    // `str::split_whitespace` already collapses runs and drops leading/trailing
    // whitespace, and tabs/newlines are whitespace, so expand+replace collapse to
    // the same word list here.
    text.split_whitespace().map(|w| w.to_string()).collect()
}

/// Break `word` into `width`-sized pieces when it cannot fit on a line by itself.
fn break_long_word(word: &str, width: usize, out: &mut Vec<String>) {
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;
    while chars.len() - i > width {
        out.push(chars[i..i + width].iter().collect());
        i += width;
    }
    out.push(chars[i..].iter().collect());
}

// ── wrap / fill ──────────────────────────────────────────────────────────────

fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let words = split_words(text);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in words {
        let wlen = word.chars().count();
        // A word longer than the whole line is broken into fragments.
        if wlen > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let mut pieces = Vec::new();
            break_long_word(&word, width, &mut pieces);
            // All but the last fragment fill an entire line.
            let last = pieces.pop();
            for p in pieces {
                lines.push(p);
            }
            if let Some(last) = last {
                cur_len = last.chars().count();
                cur = last;
            }
            continue;
        }
        if cur.is_empty() {
            cur = word;
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(&word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word;
            cur_len = wlen;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn wrap(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let text = text_arg(h, args, "wrap")?;
    let width = width_arg(h, args, 1);
    let lines: Vec<Value> = wrap_lines(&text, width)
        .into_iter()
        .map(|l| h.new_str(l))
        .collect();
    Ok(h.new_list(lines))
}

fn fill(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let text = text_arg(h, args, "fill")?;
    let width = width_arg(h, args, 1);
    Ok(h.new_str(wrap_lines(&text, width).join("\n")))
}

// ── shorten ──────────────────────────────────────────────────────────────────

fn shorten(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let text = text_arg(h, args, "shorten")?;
    let width = width_arg(h, args, 1);
    // CPython default placeholder is " [...]"; a caller can override positionally.
    let placeholder = args
        .get(2)
        .and_then(|v| h.as_str(v))
        .unwrap_or_else(|| " [...]".to_string());

    let collapsed = split_words(&text).join(" ");
    if collapsed.chars().count() <= width {
        return Ok(h.new_str(collapsed));
    }
    // The stripped placeholder must itself fit, else there is no room to shorten.
    let ph_stripped = placeholder.trim_start();
    if ph_stripped.chars().count() > width {
        return Err("ValueError: placeholder too large for max width".into());
    }
    let ph_len = placeholder.chars().count();

    // Greedily accumulate leading words while the result + placeholder still fits.
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in split_words(&text) {
        let wlen = word.chars().count();
        let added = if cur.is_empty() { wlen } else { 1 + wlen };
        if cur_len + added + ph_len <= width {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&word);
            cur_len += added;
        } else {
            break;
        }
    }
    let result = if cur.is_empty() {
        ph_stripped.to_string()
    } else {
        format!("{cur}{placeholder}")
    };
    Ok(h.new_str(result))
}

// ── indent ───────────────────────────────────────────────────────────────────

fn indent(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let text = text_arg(h, args, "indent")?;
    let prefix = args
        .get(1)
        .and_then(|v| h.as_str(v))
        .ok_or_else(|| type_error("indent() missing required argument: 'prefix'"))?;
    // Default predicate: prefix only lines that are not entirely whitespace.
    // Line terminators are preserved by splitting inclusively on '\n'.
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&prefix);
            out.push_str(line);
        }
    }
    Ok(h.new_str(out))
}

// ── dedent ───────────────────────────────────────────────────────────────────

fn dedent(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let text = text_arg(h, args, "dedent")?;

    // Whitespace-only lines are normalised to empty and ignored when computing
    // the common margin (CPython's `_whitespace_only_re` substitution).
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut margin: Option<String> = None;
    for line in &raw_lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        margin = Some(match margin {
            None => indent,
            Some(m) => common_prefix(&m, &indent),
        });
    }

    let margin = margin.unwrap_or_default();
    let out: Vec<String> = raw_lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line.strip_prefix(&margin).unwrap_or(line).to_string()
            }
        })
        .collect();
    Ok(h.new_str(out.join("\n")))
}

/// The longest common leading substring of two indent strings.
fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

//! CPython's `Did you mean: 'x'?` hint — a port of `traceback.py`'s
//! `_compute_suggestion_error` and `_levenshtein_distance` — and its
//! `SyntaxError` keyword hint, `_find_keyword_typos` (with the `difflib` and
//! `textwrap` pieces it uses).
//!
//! The distance is not textbook Levenshtein: a move costs 2 while a pure
//! case change costs 1, common affixes are trimmed first, and the search bails
//! out as soon as a row can no longer beat the budget. The exact costs decide
//! which candidate wins, so they are ported rather than approximated.
//!
//! The hint belongs to the *rendered traceback*, not to the exception: CPython's
//! `str(e)` for a `NameError` never carries it.

/// `traceback._MAX_CANDIDATE_ITEMS` — a namespace larger than this gets no hint.
const MAX_CANDIDATE_ITEMS: usize = 750;
/// `traceback._MAX_STRING_SIZE` — a name longer than this gets no hint.
const MAX_STRING_SIZE: usize = 40;
/// `traceback._MOVE_COST` / `_CASE_COST`.
const MOVE_COST: i64 = 2;
const CASE_COST: i64 = 1;

fn substitution_cost(a: char, b: char) -> i64 {
    if a == b {
        return 0;
    }
    if a.to_lowercase().eq(b.to_lowercase()) {
        return CASE_COST;
    }
    MOVE_COST
}

/// `traceback._levenshtein_distance`: the cost of turning `a` into `b`, or
/// `max_cost + 1` as soon as that is known to exceed the budget.
fn levenshtein(a: &str, b: &str, max_cost: i64) -> i64 {
    if a == b {
        return 0;
    }
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    // Trim the common prefix, then the common suffix.
    let mut lo = 0;
    while lo < av.len() && lo < bv.len() && av[lo] == bv[lo] {
        lo += 1;
    }
    let (mut ahi, mut bhi) = (av.len(), bv.len());
    while ahi > lo && bhi > lo && av[ahi - 1] == bv[bhi - 1] {
        ahi -= 1;
        bhi -= 1;
    }
    let mut a = &av[lo..ahi];
    let mut b = &bv[lo..bhi];
    if a.is_empty() || b.is_empty() {
        return MOVE_COST * (a.len() + b.len()) as i64;
    }
    if a.len() > MAX_STRING_SIZE || b.len() > MAX_STRING_SIZE {
        return max_cost + 1;
    }
    // Keep the shorter string as the row, and fail fast when even a pure run of
    // insertions cannot fit the budget.
    if b.len() < a.len() {
        std::mem::swap(&mut a, &mut b);
    }
    if (b.len() - a.len()) as i64 * MOVE_COST > max_cost {
        return max_cost + 1;
    }
    // One row of the distance matrix, updated in place.
    let mut row: Vec<i64> = (1..=a.len() as i64).map(|i| i * MOVE_COST).collect();
    let mut result = 0;
    for (bindex, &bchar) in b.iter().enumerate() {
        let mut distance = bindex as i64 * MOVE_COST;
        result = distance;
        let mut minimum = i64::MAX;
        for (index, &achar) in a.iter().enumerate() {
            let substitute = distance + substitution_cost(bchar, achar);
            distance = row[index];
            let insert_delete = result.min(distance) + MOVE_COST;
            result = insert_delete.min(substitute);
            row[index] = result;
            minimum = minimum.min(result);
        }
        if minimum > max_cost {
            return max_cost + 1;
        }
    }
    result
}

/// The candidate closest to `wrong`, or `None` when nothing is close enough.
/// Ties go to the earliest candidate, so callers must present them in CPython's
/// order (`dir()` output is sorted; a namespace is in insertion order).
///
/// This follows `Python/suggestions.c`'s `_Py_CalculateSuggestions`, which is
/// what 3.13+ actually runs, NOT `traceback.py`'s pure-Python fallback. They
/// disagree: the fallback seeds its running best with `len(wrong_name)`, so a
/// two-character typo can never be matched, while the C version starts
/// unbounded — `st` suggests `set` under the C version and nothing under the
/// fallback.
pub fn closest(candidates: &[String], wrong: &str) -> Option<String> {
    if candidates.len() >= MAX_CANDIDATE_ITEMS {
        return None;
    }
    let wrong_len = wrong.chars().count() as i64;
    let mut best_distance = i64::MAX;
    let mut suggestion: Option<&String> = None;
    for candidate in candidates {
        if candidate == wrong {
            continue;
        }
        // No more than a third of the characters involved may need changing,
        // and never worse than the best already found.
        let budget = (candidate.chars().count() as i64 + wrong_len + 3) * MOVE_COST / 6;
        let max_distance = budget.min(best_distance - 1);
        let distance = levenshtein(wrong, candidate, max_distance);
        if distance > max_distance {
            continue;
        }
        if suggestion.is_none() || distance < best_distance {
            suggestion = Some(candidate);
            best_distance = distance;
        }
    }
    suggestion.cloned()
}

/// Append CPython's hint to a terse `Type: message` line, or return it as is.
pub fn with_hint(line: &str, suggestion: Option<String>) -> String {
    match suggestion {
        Some(s) => format!("{line}. Did you mean: '{s}'?"),
        None => line.to_string(),
    }
}

/// The unbound name in a `NameError: name 'x' is not defined` line, if that is
/// what `line` is. Parsed back out of the rendered line because that is the only
/// place the hint is applied (CPython reads `exc.name`, which pythonrs's error
/// strings do not carry).
fn name_error_subject(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("NameError: name '")?;
    let end = rest.find('\'')?;
    rest[end..]
        .starts_with("' is not defined")
        .then_some(&rest[..end])
}

/// CPython's second NameError hint: when the unbound name IS a stdlib module,
/// `traceback` adds "did you forget to import it". It stacks with the near-miss
/// hint above and changes case when it does — a lone hint reads
/// `. Did you forget to import 'json'?` while a stacked one reads
/// `. Did you mean: 'jsonx'? Or did you forget to import 'json'?`.
pub fn with_import_hint(line: String, is_stdlib_module: impl Fn(&str) -> bool) -> String {
    let subject = match name_error_subject(&line) {
        Some(s) if is_stdlib_module(s) => s.to_string(),
        _ => return line,
    };
    if line.contains(". Did you mean: '") {
        format!("{line} Or did you forget to import '{subject}'?")
    } else {
        format!("{line}. Did you forget to import '{subject}'?")
    }
}

// ── SyntaxError keyword typos ───────────────────────────────────────────────

/// `traceback`'s cap on the names `_find_keyword_typos` tries.
const KEYWORD_TYPO_TOKENS: usize = 10;
/// `traceback`'s cap on the source `_find_keyword_typos` re-tokenizes.
const KEYWORD_TYPO_MAX_SOURCE: usize = 1024;

/// A port of `traceback.TracebackException._find_keyword_typos` (3.14): for a
/// bare `invalid syntax` (or `Perhaps you forgot a comma`), try each of the
/// first ten non-keyword names in the source up to the error line against the
/// keywords it most resembles, and if putting a keyword in its place makes the
/// source compile, point at that name instead: the source line, `offset`/
/// `end_offset` and `lineno` become the name's, and the message reads
/// `invalid syntax. Did you mean 'kw'?`. `None` when nothing qualifies, which
/// includes every case `traceback` aborts with an exception it suppresses.
///
/// The source is the error's `_metadata` (the parser records the whole input
/// there); with none, `traceback` reads the file, else the error's `text`.
/// The positions are those of the DEDENTED excerpt the search tokenizes, as
/// CPython reports them.
pub fn keyword_typo(msg: &str, pos: &crate::parser::SyntaxPos) -> Option<(crate::parser::SyntaxPos, String)> {
    use crate::builtins::str_splitlines;
    use crate::lexer::Tok;
    if msg != "invalid syntax" && !msg.contains("Perhaps you forgot a comma") {
        return None;
    }
    let meta = pos.metadata.as_ref()?;
    let mut line = meta.lineno;
    let mut end_line = pos.lineno.unwrap_or(0);
    let mut from_filename = false;
    let lines: Vec<String> = match &meta.source {
        Some(source) => str_splitlines(source, false),
        None => {
            let read = pos.filename.as_deref().and_then(|f| std::fs::read_to_string(f).ok());
            match read {
                Some(file) => {
                    from_filename = true;
                    str_splitlines(&file, false)
                }
                None => {
                    if pos.filename.is_some() {
                        (line, end_line) = (0, 1);
                    }
                    str_splitlines(pos.text.as_deref()?, false)
                }
            }
        }
    };
    let start = if line > 0 { line - 1 } else { 0 } as usize;
    let stop = usize::try_from(end_line).unwrap_or(0).min(lines.len());
    let excerpt = lines.get(start..stop.max(start)).unwrap_or_default().join("\n");
    let error_code = dedent(&excerpt);
    if error_code.chars().count() > KEYWORD_TYPO_MAX_SOURCE {
        return None;
    }
    let error_lines = str_splitlines(&error_code, false);
    // `tokenize` reads the excerpt line by line (`StringIO.readline` splits on
    // `\n` only), and each token reports the physical line it is on.
    let physical: Vec<&str> = error_code.split_inclusive('\n').collect();
    // Any failure to tokenize is suppressed by `traceback`, ending the search.
    let tokens = crate::lexer::lex(&error_code).ok()?.toks;
    let keywords: Vec<String> = crate::parser::KEYWORDS.iter().map(|k| k.to_string()).collect();
    let mut tokens_left = KEYWORD_TYPO_TOKENS;
    for token in tokens {
        if !matches!(token.tok, Tok::Name(_) | Tok::Ident(_)) {
            continue;
        }
        let row = token.line as i64;
        let the_end = if line == 0 { end_line } else { end_line + 1 };
        if from_filename && row + line != the_end {
            continue;
        }
        let token_line = *physical.get(token.line as usize - 1)?;
        let (col, end_col) = (token.col as usize, token.end_col as usize);
        // The name as written: the lexer's own spelling is NFKC-normalized.
        let wrong_name: String = token_line.chars().skip(col).take(end_col - col).collect();
        if crate::parser::is_keyword(&wrong_name) {
            continue;
        }
        if tokens_left == 0 {
            break;
        }
        tokens_left -= 1;
        let mut matches: Vec<String> = closest(&keywords, &wrong_name).into_iter().collect();
        matches.extend(get_close_matches(&wrong_name, &keywords, 3, 0.5));
        matches.truncate(3);
        for suggestion in matches {
            if suggestion.is_empty() || suggestion == wrong_name {
                continue;
            }
            let mut the_lines = error_lines.clone();
            let chars: Vec<char> = the_lines.get(token.line as usize - 1)?.chars().collect();
            let (a, b) = (col.min(chars.len()), end_col.min(chars.len()));
            let replaced: String = chars[..a]
                .iter()
                .chain(suggestion.chars().collect::<Vec<_>>().iter())
                .chain(chars[b.max(a)..].iter())
                .collect();
            the_lines[token.line as usize - 1] = replaced;
            if !compiles_as_command(&the_lines.join("\n")) {
                continue;
            }
            let mut typo_pos = pos.clone();
            typo_pos.text = Some(token_line.to_string());
            typo_pos.offset = Some(col as i64 + 1);
            typo_pos.end_offset = Some(end_col as i64 + 1);
            typo_pos.lineno = Some(row);
            typo_pos.end_lineno = Some(row);
            return Some((typo_pos, format!("invalid syntax. Did you mean '{suggestion}'?")));
        }
    }
    None
}

/// Whether `codeop.compile_command(code, symbol="exec",
/// flags=PyCF_ONLY_AST)` returns rather than raising: the code compiles, or it
/// is only incomplete (`codeop` answers `None` for that). `_maybe_compile`
/// ends with a full compile whenever the incomplete-input parse accepts the
/// code, so a compiler error (`'yield' outside function`) still counts against
/// it.
fn compiles_as_command(code: &str) -> bool {
    crate::compile(code).is_ok() || crate::parser::is_incomplete_input(code)
}

/// `textwrap.dedent` (3.14): remove the leading whitespace every non-blank
/// line shares, and empty the whitespace-only lines.
fn dedent(text: &str) -> String {
    use crate::builtins::is_py_space;
    let lines: Vec<&str> = text.split('\n').collect();
    let blank = |l: &str| !l.is_empty() && l.chars().all(is_py_space);
    let non_blank: Vec<&str> = lines.iter().copied().filter(|l| !l.is_empty() && !blank(l)).collect();
    let l1 = non_blank.iter().min().copied().unwrap_or("");
    let l2 = non_blank.iter().max().copied().unwrap_or("");
    // `for margin, c in enumerate(l1): if c != l2[margin] or c not in ' \t': break`
    let mut margin = 0;
    for (i, (c, d)) in l1.chars().zip(l2.chars().chain(std::iter::repeat('\0'))).enumerate() {
        margin = i;
        if c != d || !matches!(c, ' ' | '\t') {
            break;
        }
    }
    lines
        .iter()
        .map(|l| if blank(l) { String::new() } else { l.chars().skip(margin).collect() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`: the `n`
/// possibilities `SequenceMatcher` scores at least `cutoff` against `word`,
/// best first (ties broken by the larger string, as `heapq.nlargest` orders
/// the `(score, x)` pairs).
fn get_close_matches(word: &str, possibilities: &[String], n: usize, cutoff: f64) -> Vec<String> {
    let b: Vec<char> = word.chars().collect();
    let matcher = SequenceMatcher::new(&b);
    let mut result: Vec<(f64, &String)> = Vec::new();
    for x in possibilities {
        let a: Vec<char> = x.chars().collect();
        if matcher.real_quick_ratio(&a) >= cutoff
            && matcher.quick_ratio(&a) >= cutoff
            && matcher.ratio(&a) >= cutoff
        {
            result.push((matcher.ratio(&a), x));
        }
    }
    result.sort_by(|p, q| q.0.total_cmp(&p.0).then_with(|| q.1.cmp(p.1)));
    result.into_iter().take(n).map(|(_, x)| x.clone()).collect()
}

/// `difflib.SequenceMatcher` with `isjunk=None` and `autojunk=True`, `b`
/// fixed (`set_seq2`) and `a` given per call (`set_seq1`), which is how
/// `get_close_matches` drives it.
struct SequenceMatcher<'b> {
    b: &'b [char],
    /// `b2j`: each element of `b` to the indices it occurs at, without the
    /// "popular" elements autojunk drops from a `b` of 200 or more.
    b2j: std::collections::HashMap<char, Vec<usize>>,
    /// `fullbcount`, for `quick_ratio`.
    fullbcount: std::collections::HashMap<char, usize>,
}

/// `difflib._calculate_ratio`.
fn calculate_ratio(matches: usize, length: usize) -> f64 {
    if length > 0 {
        2.0 * matches as f64 / length as f64
    } else {
        1.0
    }
}

impl<'b> SequenceMatcher<'b> {
    /// `__chain_b`, plus the `fullbcount` `quick_ratio` builds on first use.
    fn new(b: &'b [char]) -> Self {
        let mut b2j: std::collections::HashMap<char, Vec<usize>> = std::collections::HashMap::new();
        let mut fullbcount = std::collections::HashMap::new();
        for (i, &elt) in b.iter().enumerate() {
            b2j.entry(elt).or_default().push(i);
            *fullbcount.entry(elt).or_insert(0) += 1;
        }
        if b.len() >= 200 {
            let ntest = b.len() / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        SequenceMatcher { b, b2j, fullbcount }
    }

    /// `find_longest_match(alo, ahi, blo, bhi)`. With no junk, only the first
    /// pair of extension loops can move the match (over popular elements).
    fn find_longest_match(&self, a: &[char], alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let b = self.b;
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        let mut j2len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for (i, elt) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len = std::collections::HashMap::new();
            for &j in self.b2j.get(elt).map(Vec::as_slice).unwrap_or_default() {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).copied().unwrap_or(0) + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                }
            }
            j2len = newj2len;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            (besti, bestj, bestsize) = (besti - 1, bestj - 1, bestsize + 1);
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    /// The total size of `get_matching_blocks()`, which is all `ratio` sums.
    fn matching_size(&self, a: &[char]) -> usize {
        let mut queue = vec![(0, a.len(), 0, self.b.len())];
        let mut total = 0;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(a, alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }

    fn ratio(&self, a: &[char]) -> f64 {
        calculate_ratio(self.matching_size(a), a.len() + self.b.len())
    }

    fn quick_ratio(&self, a: &[char]) -> f64 {
        let mut avail: std::collections::HashMap<char, i64> = std::collections::HashMap::new();
        let mut matches = 0;
        for elt in a {
            let numb = match avail.get(elt) {
                Some(&n) => n,
                None => self.fullbcount.get(elt).copied().unwrap_or(0) as i64,
            };
            avail.insert(*elt, numb - 1);
            if numb > 0 {
                matches += 1;
            }
        }
        calculate_ratio(matches, a.len() + self.b.len())
    }

    fn real_quick_ratio(&self, a: &[char]) -> f64 {
        calculate_ratio(a.len().min(self.b.len()), a.len() + self.b.len())
    }
}

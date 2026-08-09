//! CPython's `Did you mean: 'x'?` hint — a port of `traceback.py`'s
//! `_compute_suggestion_error` and `_levenshtein_distance`.
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

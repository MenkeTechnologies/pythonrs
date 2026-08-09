//! Source-level guard on the NAME-keyed registries, the sibling of
//! `tests/opcodes.rs`'s guard on the integer op-id space.
//!
//! `tests/opcodes.rs` proves no two builtins share a numeric id. The same
//! last-write-wins hazard exists wherever a *name* is the key, and there it is
//! worse, because the compiler's coverage is partial and what it does catch is
//! only a warning. Measured against rustc 1.x on this crate
//! (`rustc --edition 2021 --crate-type lib` on a five-case probe):
//!
//! | shape                                          | rustc |
//! |------------------------------------------------|-------|
//! | `match n { "x" => .., "x" => .. }`              | warns (`unreachable_pattern`) |
//! | `matches!(n, "x" \| "y" \| "x")`                | warns (`unreachable_pattern`) |
//! | `match n { "x" if g => .., "x" => .. }`         | **silent** |
//! | `const T: &[&str] = &["x", "y", "x"];`          | **silent** |
//! | the same name in two tables unioned at runtime  | **silent** |
//!
//! This crate sets no `deny(warnings)`, so even the two it does catch leave
//! `cargo build` at exit 0 — a warning inside 62k lines of source is not a
//! signal anybody reads. Every row above therefore reaches a release the same
//! way: build clean, wrong handler at run time, and an error naming neither the
//! duplicate nor the entry it shadowed.
//!
//! pythonrs carries the extra exposure `tests/opcodes.rs` already documents:
//! compiled chunks persist to `~/.pythonrs/scripts.rkyv`. Method names ride in
//! a chunk as string constants and the tables below are consulted at run time,
//! so retabling a name takes effect immediately; renaming one in a way that
//! changes emitted bytecode needs a `cache::SCHEMA` bump like any other codegen
//! change.
//!
//! These tests read the *written source text* rather than the compiled values,
//! for the reason `tests/opcodes.rs` gives: two entries sharing one key compile
//! perfectly, so only the text can report the collision by name and line.

/// Every file that declares or dispatches a name-keyed registry.
const SOURCES: &[(&str, &str)] = &[
    ("src/builtins.rs", include_str!("../src/builtins.rs")),
    ("src/host.rs", include_str!("../src/host.rs")),
    ("src/lsp.rs", include_str!("../src/lsp.rs")),
    ("src/parser.rs", include_str!("../src/parser.rs")),
    ("src/lexer.rs", include_str!("../src/lexer.rs")),
    ("src/compiler.rs", include_str!("../src/compiler.rs")),
    ("src/excgroup.rs", include_str!("../src/excgroup.rs")),
    ("src/regexpr.rs", include_str!("../src/regexpr.rs")),
    ("src/stdlib/mod.rs", include_str!("../src/stdlib/mod.rs")),
    ("src/stdlib/pyio.rs", include_str!("../src/stdlib/pyio.rs")),
    (
        "src/stdlib/pyimp.rs",
        include_str!("../src/stdlib/pyimp.rs"),
    ),
    (
        "src/stdlib/pycsv.rs",
        include_str!("../src/stdlib/pycsv.rs"),
    ),
    (
        "src/stdlib/codecs.rs",
        include_str!("../src/stdlib/codecs.rs"),
    ),
    (
        "src/stdlib/pyhash.rs",
        include_str!("../src/stdlib/pyhash.rs"),
    ),
    (
        "src/stdlib/pystruct.rs",
        include_str!("../src/stdlib/pystruct.rs"),
    ),
    (
        "src/stdlib/pysignal.rs",
        include_str!("../src/stdlib/pysignal.rs"),
    ),
    (
        "src/stdlib/pyopcode.rs",
        include_str!("../src/stdlib/pyopcode.rs"),
    ),
    (
        "src/stdlib/binascii.rs",
        include_str!("../src/stdlib/binascii.rs"),
    ),
    (
        "src/stdlib/pytokenize.rs",
        include_str!("../src/stdlib/pytokenize.rs"),
    ),
];

/// Read one Rust string literal starting at `b[i]`, which must be the opening
/// quote. Returns the unescaped-enough content and the index just past the
/// closing quote. Escapes are kept verbatim: two literals are "the same key"
/// only if they are written the same way, which is the property under test.
fn read_string(b: &[u8], i: usize) -> Option<(String, usize)> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    let mut out = String::new();
    while j < b.len() {
        match b[j] {
            b'\\' => {
                out.push('\\');
                if let Some(&c) = b.get(j + 1) {
                    out.push(c as char);
                }
                j += 2;
            }
            b'"' => return Some((out, j + 1)),
            c => {
                out.push(c as char);
                j += 1;
            }
        }
    }
    None
}

/// Byte offset -> 1-based line number.
fn line_of(src: &str, off: usize) -> usize {
    src[..off.min(src.len())]
        .bytes()
        .filter(|c| *c == b'\n')
        .count()
        + 1
}

/// Skip whitespace and `//` line comments.
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        return i;
    }
}

/// One `const`/`static` table of string-keyed rows.
struct Table {
    name: String,
    line: usize,
    /// Row = the string literals of one entry, in written order. A `&[&str]`
    /// table yields one-element rows; a tuple table yields the leading strings.
    rows: Vec<(Vec<String>, usize)>,
    /// `&[(..)]` rather than `&[&str]`.
    tuple: bool,
}

/// Pull every `const/static NAME: &[&str]` and `const/static NAME: &[(&str, …)]`
/// table out of `src`.
fn tables(src: &str) -> Vec<Table> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    for kw in ["const ", "static "] {
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(kw) {
            let start = from + rel;
            from = start + kw.len();
            // `const NAME: &[` … the declaration must be an all-caps item name.
            let rest = &src[start + kw.len()..];
            let Some(colon) = rest.find(':') else {
                continue;
            };
            let name = rest[..colon].trim();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            {
                continue;
            }
            let after = &rest[colon + 1..];
            let tuple = match after.trim_start().strip_prefix("&[") {
                Some(t) => {
                    let t = t.trim_start();
                    if t.starts_with('(') {
                        true
                    } else if t.starts_with("&'static str") || t.starts_with("&str") {
                        false
                    } else {
                        continue; // not a string-keyed table
                    }
                }
                None => continue,
            };
            // Body spans from the `&[` that opens the initialiser to its match.
            let Some(eq) = after.find('=') else { continue };
            let init = start + kw.len() + colon + 1 + eq + 1;
            let mut i = skip_trivia(b, init);
            if b.get(i) != Some(&b'&') {
                continue;
            }
            i += 1;
            i = skip_trivia(b, i);
            if b.get(i) != Some(&b'[') {
                continue;
            }
            let mut depth = 0i32;
            let mut rows: Vec<(Vec<String>, usize)> = Vec::new();
            let mut cur: Vec<String> = Vec::new();
            let mut cur_off = i;
            let mut in_tuple = false;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        let Some((s, ni)) = read_string(b, i) else {
                            break;
                        };
                        if !tuple {
                            rows.push((vec![s], line_of(src, i)));
                        } else if in_tuple {
                            if cur.is_empty() {
                                cur_off = i;
                            }
                            cur.push(s);
                        }
                        i = ni;
                        continue;
                    }
                    b'(' if tuple && depth == 1 => {
                        in_tuple = true;
                        cur = Vec::new();
                        cur_off = i;
                        depth += 1;
                    }
                    b')' if tuple => {
                        depth -= 1;
                        if depth == 1 && in_tuple {
                            in_tuple = false;
                            if !cur.is_empty() {
                                rows.push((std::mem::take(&mut cur), line_of(src, cur_off)));
                            }
                        }
                    }
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b'/' if b.get(i + 1) == Some(&b'/') => {
                        while i < b.len() && b[i] != b'\n' {
                            i += 1;
                        }
                        continue;
                    }
                    _ => {}
                }
                i += 1;
            }
            if !rows.is_empty() {
                out.push(Table {
                    name: name.to_string(),
                    line: line_of(src, start),
                    rows,
                    tuple,
                });
            }
        }
    }
    out
}

/// A plain `&[&str]` table IS its key set. A repeated string is dead weight at
/// best — it inflates `dir()` and every `len()` derived from the table — and a
/// merge accident at worst, since these blocks are append-only and two branches
/// that each add the same method name merge with no conflict marker.
#[test]
fn no_name_table_repeats_a_key() {
    let mut bad = Vec::new();
    let mut scanned = 0usize;
    for (path, src) in SOURCES {
        for t in tables(src) {
            if t.tuple {
                continue;
            }
            scanned += 1;
            let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for (row, line) in &t.rows {
                if let Some(first) = seen.insert(row[0].as_str(), *line) {
                    bad.push(format!(
                        "{} (declared {path}:{}) declares {:?} twice ({path}:{first} and {path}:{line})",
                        t.name, t.line, row[0]
                    ));
                }
            }
        }
    }
    assert!(
        scanned > 40,
        "expected to scan the whole name-table surface, saw {scanned} tables — \
         the parser is broken, not the source"
    );
    assert!(
        bad.is_empty(),
        "duplicate key in a name table (rustc does not lint this at all):\n  {}",
        bad.join("\n  ")
    );
}

/// Tuple tables are keyed lookups, and the key width follows the row width:
///
/// * a 2-tuple is `(key, value)` — `EXC_PARENTS`, `SIGNALS`, `NB_OPS` — so the
///   FIRST element must be unique. A repeat is a genuine last-write-wins: one
///   of the two values is unreachable, and which one depends on whether the
///   consumer uses `.find()` (first wins) or folds into a map (last wins).
/// * a wider row is a record whose key is `(first, second)` — `CORPUS` is
///   `(name, chapter, doc, example)` and legitimately carries `pow` under both
///   `Builtin` and `math`. The pair must still be unique, or `hover` and
///   `completion` pick an arbitrary one of two entries.
#[test]
fn no_keyed_tuple_table_repeats_a_key() {
    let mut bad = Vec::new();
    let mut scanned = 0usize;
    for (path, src) in SOURCES {
        for t in tables(src) {
            if !t.tuple {
                continue;
            }
            let width = t.rows.iter().map(|(r, _)| r.len()).max().unwrap_or(0);
            // Rows must be uniform for the key rule to mean anything.
            if width == 0 {
                continue;
            }
            scanned += 1;
            // A 2-tuple `(key, value)` keeps only the leading string; a wider
            // record is keyed by its first two string columns.
            let keep = if width >= 2 { 2 } else { 1 };
            let mut seen: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for (row, line) in &t.rows {
                // `(&str, i32)` rows parse to a single string: key on that.
                let k = row
                    .iter()
                    .take(keep.min(row.len()))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\u{1}");
                if let Some(first) = seen.insert(k.clone(), *line) {
                    bad.push(format!(
                        "{} (declared {path}:{}) repeats key {:?} ({path}:{first} and {path}:{line})",
                        t.name,
                        t.line,
                        k.replace('\u{1}', ", ")
                    ));
                }
            }
        }
    }
    assert!(
        scanned > 3,
        "expected to scan the tuple-table surface, saw {scanned} — parser broken"
    );
    assert!(
        bad.is_empty(),
        "duplicate key in a keyed tuple table (rustc does not lint this at all):\n  {}",
        bad.join("\n  ")
    );
}

/// No `match` arm on a string literal is shadowed by an earlier UNGUARDED arm
/// for the same literal.
///
/// rustc reports this as `unreachable_pattern`, but only as a warning, and the
/// crate sets no `deny(warnings)`: `cargo build` still exits 0 and the line
/// scrolls past. Promoting it to a red test is the whole point — a shadowed
/// name-dispatch arm is exactly the failure that ships clean and then answers
/// with the wrong handler. Only an unguarded earlier arm is reported, because
/// `"x" if a => .., "x" if !a => ..` is legitimate and provably-dead requires
/// the shadowing arm to be unconditional.
#[test]
fn no_match_arm_is_shadowed_by_an_earlier_unguarded_arm() {
    let mut bad = Vec::new();
    let mut scanned = 0usize;
    for (path, src) in SOURCES {
        let b = src.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find("match ") {
            let head = from + rel;
            from = head + 6;
            // Find the `{` that opens this match's arm list.
            let Some(open) = src[head..].find('{').map(|o| head + o) else {
                break;
            };
            scanned += 1;
            // Walk the arms at brace depth 1, tracking `"lit" [| "lit"]* [if g] =>`.
            let mut i = open + 1;
            let mut depth = 1i32;
            // literal -> (line, earlier arm was unguarded)
            let mut seen: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            // literals of the arm currently being read, and its start line
            let mut pending: Vec<(String, usize)> = Vec::new();
            // an `if` guard token seen in the arm head currently being read
            let mut guarded = false;
            while i < b.len() && depth > 0 {
                match b[i] {
                    b'{' | b'[' | b'(' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' | b')' => {
                        depth -= 1;
                        i += 1;
                    }
                    b'/' if b.get(i + 1) == Some(&b'/') => {
                        while i < b.len() && b[i] != b'\n' {
                            i += 1;
                        }
                    }
                    b'"' if depth == 1 => {
                        let Some((s, ni)) = read_string(b, i) else {
                            break;
                        };
                        pending.push((s, line_of(src, i)));
                        i = ni;
                    }
                    b'i' if depth == 1
                        && src[i..].starts_with("if ")
                        && !i
                            .checked_sub(1)
                            .and_then(|p| b.get(p))
                            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') =>
                    {
                        guarded = true;
                        i += 3;
                    }
                    b'=' if depth == 1 && b.get(i + 1) == Some(&b'>') => {
                        for (lit, line) in pending.drain(..) {
                            match seen.get(&lit) {
                                Some(first) => bad.push(format!(
                                    "{path}:{line}: arm {lit:?} is dead — an unguarded \
                                     arm for the same name already matched at {path}:{first}"
                                )),
                                None => {
                                    if !guarded {
                                        seen.insert(lit, line);
                                    }
                                }
                            }
                        }
                        guarded = false;
                        i += 2;
                    }
                    b',' if depth == 1 => {
                        pending.clear();
                        guarded = false;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
        }
    }
    assert!(
        scanned > 500,
        "expected to walk the crate's match blocks, saw {scanned} — parser broken"
    );
    assert!(
        bad.is_empty(),
        "shadowed name-dispatch arm (rustc warns but does not fail the build):\n  {}",
        bad.join("\n  ")
    );
}

/// `matches!(name, "a" | "b" | "a")` has the same hazard with the same
/// warning-only treatment, and it is how most of the small method sets in
/// `type_has_method` are written.
#[test]
fn no_matches_macro_repeats_an_alternative() {
    let mut bad = Vec::new();
    let mut scanned = 0usize;
    for (path, src) in SOURCES {
        let b = src.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find("matches!(") {
            let head = from + rel;
            from = head + 9;
            scanned += 1;
            let mut i = head + 9;
            let mut depth = 1i32;
            let mut seen: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            while i < b.len() && depth > 0 {
                match b[i] {
                    b'(' | b'[' | b'{' => {
                        depth += 1;
                        i += 1;
                    }
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        i += 1;
                    }
                    b'"' if depth == 1 => {
                        let Some((s, ni)) = read_string(b, i) else {
                            break;
                        };
                        let line = line_of(src, i);
                        if let Some(first) = seen.insert(s.clone(), line) {
                            bad.push(format!(
                                "{path}:{line}: `matches!` repeats {s:?} \
                                 (already listed at {path}:{first})"
                            ));
                        }
                        i = ni;
                    }
                    _ => i += 1,
                }
            }
        }
    }
    assert!(
        scanned > 100,
        "expected to walk the crate's `matches!` sites, saw {scanned} — parser broken"
    );
    assert!(
        bad.is_empty(),
        "repeated alternative in a `matches!` name set:\n  {}",
        bad.join("\n  ")
    );
}

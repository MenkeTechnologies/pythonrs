//! Source-level guard on the hand-assigned builtin/opcode id space.
//!
//! Every Python operation the VM cannot do natively is a `fusevm` builtin call,
//! and the ids are hand-written `pub const`s in `src/host.rs::ops`. Two things
//! make a duplicated id catastrophic and *silent*:
//!
//! 1. `fusevm::VM::register_builtin(id, handler)` writes into a `Vec` slot
//!    (`builtin_table[id] = Some(handler)`), so a second registration at the
//!    same id **replaces** the first with no error. The op that was registered
//!    earlier in `builtins.rs::install` simply starts running the other op's
//!    handler.
//! 2. Two concurrent branches that each append one constant both pick the same
//!    next number. The files merge with **no conflict marker** — the constant
//!    block is append-only, so git takes both lines happily.
//!
//! pythonrs has a third exposure the other frontends don't: compiled chunks are
//! persisted to `~/.pythonrs/scripts.rkyv` with the raw `CallBuiltin(id, argc)`
//! baked in, so a reassigned id also mis-dispatches every *cached* script until
//! `cache::SCHEMA` is bumped.
//!
//! These tests read the constants back out of the source text (not the compiled
//! values, so a duplicate is reported by name and line rather than silently
//! collapsing) and fail on any collision.

/// The declaration site of every builtin id and op tag.
const HOST_RS: &str = include_str!("../src/host.rs");
/// The registration site: `install()` binds each id to its handler.
const BUILTINS_RS: &str = include_str!("../src/builtins.rs");

/// One `pub const NAME: TY = VALUE;` inside a `pub mod` block.
#[derive(Debug)]
struct Decl {
    name: String,
    value: i64,
    line: usize,
}

/// Pull every `pub const` out of the top-level `pub mod <module>` block in `src`.
///
/// Deliberately textual: the point is to see the *written* numbers, which a
/// `use`-and-compare against the compiled constants could never do (two names
/// bound to one number compile perfectly).
fn consts_in_module(src: &str, module: &str) -> Vec<Decl> {
    let header = format!("pub mod {module} {{");
    let mut out = Vec::new();
    let mut inside = false;
    for (i, raw) in src.lines().enumerate() {
        if !inside {
            inside = raw == header;
            continue;
        }
        if raw == "}" {
            break; // top-level close of the module block
        }
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let (name, rest) = rest.split_once(':').expect("`pub const NAME: TY = V;`");
        let (_ty, rest) = rest.split_once('=').expect("`pub const NAME: TY = V;`");
        let value = rest
            .split(';')
            .next()
            .expect("terminating `;`")
            .trim()
            .parse::<i64>()
            .unwrap_or_else(|e| panic!("{module}::{} value is not an integer: {e}", name.trim()));
        out.push(Decl {
            name: name.trim().to_string(),
            value,
            line: i + 1,
        });
    }
    assert!(
        !out.is_empty(),
        "no `pub const`s found in `pub mod {module}` — did the module get renamed?"
    );
    out
}

/// Report every id shared by two or more constants in one module.
fn collisions(decls: &[Decl]) -> Vec<String> {
    let mut report = Vec::new();
    for (i, a) in decls.iter().enumerate() {
        for b in &decls[i + 1..] {
            if a.value == b.value {
                report.push(format!(
                    "id {} is assigned to both {} (src/host.rs:{}) and {} (src/host.rs:{})",
                    a.value, a.name, a.line, b.name, b.line
                ));
            }
            if a.name == b.name {
                report.push(format!(
                    "name {} is declared twice (src/host.rs:{} and src/host.rs:{})",
                    a.name, a.line, b.line
                ));
            }
        }
    }
    report
}

/// No two builtin ops share an id. A duplicate means the later
/// `register_builtin` silently overwrote the earlier op's handler.
#[test]
fn every_builtin_op_id_is_distinct() {
    let ops = consts_in_module(HOST_RS, "ops");
    let found = collisions(&ops);
    assert!(
        found.is_empty(),
        "colliding builtin op ids in `src/host.rs::ops` \
         (the later `register_builtin` wins and the earlier op is dead):\n  {}",
        found.join("\n  ")
    );
}

/// The operator tag modules carry the same hazard: `iop`/`binop`/`unop` values
/// are the integer operand of `INPLACE`/`BINOP`/`UNARY`, dispatched by a `match`
/// in `builtins.rs`. Two tags on one number means one operator runs the other's
/// dunder.
#[test]
fn every_op_tag_is_distinct() {
    for module in ["iop", "binop", "unop"] {
        let tags = consts_in_module(HOST_RS, module);
        let found = collisions(&tags);
        assert!(
            found.is_empty(),
            "colliding tags in `src/host.rs::{module}`:\n  {}",
            found.join("\n  ")
        );
    }
}

/// The id space is dense, so "the next free id" is unambiguously `max + 1`.
/// A gap means an id was removed or renumbered — which invalidates the on-disk
/// bytecode cache without a `cache::SCHEMA` bump, and re-opens the slot for a
/// future collision.
#[test]
fn op_ids_are_dense() {
    let mut ops = consts_in_module(HOST_RS, "ops");
    ops.sort_by_key(|d| d.value);
    assert_eq!(ops[0].value, 1, "builtin op ids start at 1 (0 is unused)");
    for pair in ops.windows(2) {
        assert_eq!(
            pair[1].value,
            pair[0].value + 1,
            "gap in the builtin op id space between {} = {} and {} = {} \
             (src/host.rs:{}); ids must stay dense so the next one is max+1",
            pair[0].name,
            pair[0].value,
            pair[1].name,
            pair[1].value,
            pair[1].line
        );
    }

    for module in ["iop", "binop", "unop"] {
        let mut tags = consts_in_module(HOST_RS, module);
        tags.sort_by_key(|d| d.value);
        for (i, tag) in tags.iter().enumerate() {
            assert_eq!(
                tag.value, i as i64,
                "`{module}` tags must be dense from 0 (they are indexed by \
                 declaration order); {} = {} breaks that",
                tag.name, tag.value
            );
        }
    }
}

/// Every op the frontend actually emits has exactly one handler bound to it.
///
/// Catches the two ways `install()` can drift from `ops`: an op referenced by
/// the compiler with no `register_builtin` line (dispatch hits an empty table
/// slot at run time), and one op registered twice (the second binding wins, so
/// one of the two handlers is unreachable).
#[test]
fn every_emitted_op_is_registered_exactly_once() {
    /// Collect the `ops::NAME` identifiers following each occurrence of `needle`.
    fn refs(src: &str, needle: &str) -> Vec<String> {
        let mut out = Vec::new();
        for chunk in src.split(needle).skip(1) {
            let name: String = chunk
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
        out
    }

    let registered = refs(BUILTINS_RS, "register_builtin(ops::");
    let mut seen = std::collections::HashMap::new();
    for name in &registered {
        *seen.entry(name.as_str()).or_insert(0usize) += 1;
    }
    let twice: Vec<_> = seen
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(name, n)| format!("{name} registered {n} times"))
        .collect();
    assert!(
        twice.is_empty(),
        "duplicate `register_builtin` in `src/builtins.rs::install` \
         (only the last binding survives):\n  {}",
        twice.join("\n  ")
    );

    // Every op named anywhere outside its own declaration must have a handler.
    let declared: Vec<String> = consts_in_module(HOST_RS, "ops")
        .into_iter()
        .map(|d| d.name)
        .collect();
    let mut emitted: Vec<&String> = Vec::new();
    let used_in_compiler = refs(include_str!("../src/compiler.rs"), "ops::");
    for name in &declared {
        if used_in_compiler.contains(name) {
            emitted.push(name);
        }
    }
    assert!(
        emitted.len() > 50,
        "expected the compiler to emit most of the op table, saw {} — \
         the `ops::` scan is broken, not the source",
        emitted.len()
    );
    let missing: Vec<&str> = emitted
        .iter()
        .filter(|name| !seen.contains_key(name.as_str()))
        .map(|s| s.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "op(s) emitted by `src/compiler.rs` with no handler in \
         `src/builtins.rs::install` (run-time dispatch into an empty slot): {missing:?}"
    );

    let unknown: Vec<&String> = registered
        .iter()
        .filter(|n| !declared.contains(n))
        .collect();
    assert!(
        unknown.is_empty(),
        "`install()` registers ids that `ops` does not declare: {unknown:?}"
    );
}

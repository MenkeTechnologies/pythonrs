//! rkyv-backed bytecode cache for compiled Python scripts (mirrors the fleet's
//! zshrs/rubylang design). Every ordinary `python foo.py` run is transparently
//! cached: the source is hashed, the shard consulted, and on a hit the compiled
//! `fusevm::Chunk`s run directly — lex/parse/lower are skipped entirely. On a
//! miss the program is compiled, stored, then run. `python --build` warms the
//! same shard ahead of time.
//!
//! Layout: a single shard at `~/.pythonrs/scripts.rkyv`. The *outer* container is
//! a zero-copy rkyv archive (`Shard`), validated on load; each *inner* entry blob
//! is a bincode-encoded `CProg` (the compiled `fusevm::Chunk`s + func/try
//! tables), because `fusevm::Chunk` is serde-owned, not `rkyv::Archive`. The key
//! is a 64-bit hash of the source plus a schema version, so a source or format
//! change misses cleanly instead of loading stale bytecode.

use crate::compiler::Program;
use crate::host::{FuncDef, TryDef};
use fusevm::Chunk;
use rkyv::{Archive, Deserialize as RkyvDe, Serialize as RkyvSer};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Bump on any incompatible change to `CProg` / the lowering / the shard layout.
/// v3: new builtin op ids + generator/comprehension-as-function/match/unpack
/// lowering — bytecode compiled by any earlier pythonrs must miss cleanly.
/// v4: `yield from` now lowers to the `GENRET` op (delegated return value); older
/// cached bytecode for a `yield from` would drop the value, so it must miss.
/// v5: `MKFUNC` carries keyword-only defaults + a count below the func id; older
/// bytecode uses the 1-arg-only layout and must miss cleanly.
/// v6: `raise X from Y` now emits `RAISE` with argc 2 (cause pushed under exc);
/// older bytecode dropped the cause, so it must miss cleanly.
/// v7: the lexer decodes `\NNN` octal string escapes; older bytecode baked the
/// undecoded literal into the chunk, so it must miss cleanly.
/// v8: `FuncDef` gained a `posonly` count (positional-only enforcement); older
/// serialized func templates lack it and must miss cleanly.
/// v9: comprehensions now inject a `global`/`nonlocal` declaration for walrus
/// (`:=`) targets so they leak to the enclosing scope; older bytecode lacks the
/// declaration and must miss cleanly.
/// v10: `BUILD_CLASS` takes a 4th arg (the explicit metaclass, or `None`) pushed
/// below the bases; older 3-arg bytecode must miss cleanly.
/// v12: f-string format specs are compiled as their own joined-string so nested
/// replacement fields (`{w}` in `{x:{w}.2f}`) evaluate at runtime; older bytecode
/// baked the spec as a literal constant and must miss cleanly.
/// v15: augmented assignment emits the `INPLACE` op (the CPython in-place-dunder
/// protocol) instead of a plain `x = x <op> y` rebind; `with` uses the hit-flag
/// `__exit__` desugar; chained comparisons bind interior operands to walrus temps
/// for single-evaluation. All three change lowering, so older cached bytecode
/// (which would run the old rebind / re-evaluating forms) must miss cleanly.
/// v16: `yield from` lowers to a single `YIELD_FROM` op (full PEP 380 delegation:
/// sent values, thrown exceptions, and close forwarded into the sub-iterator)
/// instead of the old FORITER/YIELDV/GENRET loop that dropped sent values.
/// v17: a loop whose `break`/`continue` crosses a `try`/`with` boundary lowers
/// its body to a `LOOP_BODY` sub-chunk driven by control signals (so a `finally`
/// runs before the loop exit) instead of an in-chunk jump; `CProg` gained a
/// `warnings` list (compile-time `SyntaxWarning`s). Older bytecode used the jump
/// form (which panicked on that shape) and lacks the field, so it must miss.
/// v18: `match` singleton value patterns (None/True/False) lower to an identity
/// check (`IS`) instead of `NumEq`, matching PEP 634 (`0` no longer matches
/// `case False`). Older bytecode used `NumEq` and would mismatch, so it must miss.
/// v19: `FuncDef` gained a `locals` set (names local to the scope) so a read of a
/// function-local name before it is bound raises `UnboundLocalError` instead of
/// falling through to an enclosing/global binding. Older bytecode lacks the field
/// and would give the wrong error, so it must miss.
/// v20: `FuncDef` gained a `qualname` (`__qualname__`) so function introspection
/// dunders resolve. Older bytecode lacks the field, so it must miss.
/// v21: a class body now lowers its simple annotations (`x: int`) into an
/// `__annotations__` dict (SETUP + per-field SETITEM), so `dataclass`/
/// `typing.NamedTuple` and `Cls.__annotations__` see the fields; also large
/// collection literals (over 255 elements) lower via the `EXTEND_*` ops. Older
/// cached bytecode lacks the annotation/extend lowering and must miss cleanly.
/// v22: `CProg.warnings` entries now carry the full `SyntaxWarning` text (not
/// just a keyword), and the compiler adds the `"is" with a literal` warning.
/// Older cached warnings held bare keywords and would print malformed.
/// v23: `MKFUNC` carries an `__annotations__` dict as its deepest arg (built from
/// param/return annotations at def time) and `FuncVal` gained an `annotations`
/// field; also `...` lowers to the new `ELLIPSIS` op (a distinct `Ellipsis`
/// singleton) instead of `LoadUndef` (`None`). Older bytecode used the
/// annotation-free layout / conflated `...` with `None` and must miss cleanly.
const SCHEMA: u64 = 23;

/// The outer, rkyv-archived shard: a flat list of (key, bincode-blob) entries.
#[derive(Archive, RkyvSer, RkyvDe, Default)]
#[archive(check_bytes)]
struct Shard {
    entries: Vec<Entry>,
}

#[derive(Archive, RkyvSer, RkyvDe)]
#[archive(check_bytes)]
struct Entry {
    key: u64,
    /// A second, independent hash of the source. A cache hit requires BOTH `key`
    /// and `verify` to match, so an `FxHash` collision on `key` can never return
    /// a different program's bytecode (which would silently produce wrong
    /// results — far worse than a cache miss).
    verify: u64,
    blob: Vec<u8>,
}

/// The inner, serde/bincode form of a compiled program.
#[derive(Serialize, Deserialize)]
struct CProg {
    main: Chunk,
    functions: Vec<(String, FuncDef)>,
    tries: Vec<TryDef>,
    warnings: Vec<(u32, String)>,
}

/// A stable content key for a source string (fast `FxHash`, used for lookup).
pub fn key_for(src: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    SCHEMA.hash(&mut h);
    src.hash(&mut h);
    h.finish()
}

/// An independent verification hash (std `DefaultHasher`/SipHash), so a hit
/// requires both hashes to agree — collision-proof for correctness.
fn verify_for(src: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    SCHEMA.hash(&mut h);
    src.len().hash(&mut h);
    src.hash(&mut h);
    h.finish()
}

fn shard_path() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join(".pythonrs");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("scripts.rkyv"))
}

fn load_shard() -> Shard {
    let Some(path) = shard_path() else {
        return Shard::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Shard::default();
    };
    rkyv::from_bytes::<Shard>(&bytes).unwrap_or_default()
}

fn write_shard(shard: &Shard) -> Result<(), String> {
    let path = shard_path().ok_or("no home dir for cache")?;
    let bytes = rkyv::to_bytes::<_, 4096>(shard).map_err(|e| format!("cache serialize: {e}"))?;
    // Atomic replace (write temp + rename) so a concurrent reader — up to 16
    // instances run against the shared shard — never sees a torn file. A losing
    // concurrent writer just drops its entry (recompiled next run); it can never
    // corrupt the shard. The temp name is unique per WRITE (pid + a monotonic
    // counter), not just per process, so concurrent writers within one process
    // (e.g. parallel test threads) never clobber each other's temp file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("rkyv.tmp.{}.{n}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("cache write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cache rename: {e}")
    })
}

/// Look up a compiled program for `src`, if present and current.
pub fn load(src: &str) -> Option<Program> {
    let key = key_for(src);
    let verify = verify_for(src);
    let shard = load_shard();
    let entry = shard
        .entries
        .iter()
        .find(|e| e.key == key && e.verify == verify)?;
    let cp: CProg = bincode::deserialize(&entry.blob).ok()?;
    Some(Program {
        main: cp.main,
        functions: cp.functions,
        procs: Vec::new(),
        tries: cp.tries,
        warnings: cp.warnings,
    })
}

/// Store `prog` (compiled from `src`) into the shard, replacing any prior entry.
pub fn store(src: &str, prog: &Program) -> Result<(), String> {
    let key = key_for(src);
    let verify = verify_for(src);
    let cp = CProg {
        main: prog.main.clone(),
        functions: prog.functions.clone(),
        tries: prog.tries.clone(),
        warnings: prog.warnings.clone(),
    };
    let blob = bincode::serialize(&cp).map_err(|e| format!("cache encode: {e}"))?;
    let mut shard = load_shard();
    shard.entries.retain(|e| e.key != key);
    shard.entries.push(Entry { key, verify, blob });
    write_shard(&shard)
}

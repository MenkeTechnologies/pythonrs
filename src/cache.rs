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

use crate::ast::Span;
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
/// v24: `CProg` gained a `positions` list (traceback-caret op-index → span tables)
/// and the loader recomputes each chunk's `op_hash` (skipped by serde) so caret
/// lookups match. Older entries lack the field and must miss cleanly.
/// v25: `Entry` gained a `source` name (script path / `<string>` / `<stdin>`) for
/// `--cacheview` provenance; the added rkyv field changes the shard layout, so
/// older shards fail to decode and rebuild.
/// v26: new `IMPORT_STAR` opcode for `from m import *` — the compiler now emits
/// different bytecode for a star-import, so any entry cached under v25 (which
/// mis-emitted it as an `IMPORT_FROM "*"` attribute fetch) must miss and recompile.
/// v27: `FuncDef` gained `freevars` (drives `func.__closure__`/`co_freevars` and
/// the `CO_NOFREE` flag); recompile so closures carry their free-variable list.
/// v28: new `IMPORT_RELATIVE` opcode — a relative `from . import x` now compiles
/// to a runtime-resolved import (previously the leading dots were dropped and the
/// module name collapsed to `""`), so cached star/relative-import bytecode misses.
/// v29: PEP 695 type parameters (`class C[T]`, `def f[T]`) now parse and emit
/// `T = object` bindings; files that previously failed to compile now do, so any
/// stale negative-cache/relative bytecode must rebuild.
/// v30: `import a.b.c` now binds the TOP package `a` (re-importing it) instead of
/// the leaf submodule, so the emitted bytecode differs for any dotted `import`.
/// v31: a `def`/`lambda` with annotations compiles them as a `<annotate>` thunk
/// (MKFUNC evaluates it with forward-reference NameErrors caught) instead of an
/// inline dict, so annotated-function bytecode differs.
const SCHEMA: u64 = 35;

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
    /// The source name that last produced this entry — a script path, `<string>`
    /// (`-c`), or `<stdin>`. The cache keys by source CONTENT, so this is
    /// best-effort provenance for `--cacheview`, not part of the lookup.
    source: String,
    blob: Vec<u8>,
}

/// The inner, serde/bincode form of a compiled program.
#[derive(Serialize, Deserialize)]
struct CProg {
    main: Chunk,
    functions: Vec<(String, FuncDef)>,
    tries: Vec<TryDef>,
    warnings: Vec<(u32, String)>,
    /// Traceback-caret position tables, `(chunk op_hash, op-index → span)`.
    positions: Vec<(u64, Vec<Span>)>,
}

/// Recompute a chunk's `op_hash` exactly as `fusevm::ChunkBuilder::build` does
/// (a `DefaultHasher` over ops then constants). `op_hash` is `#[serde(skip)]`, so
/// a deserialized chunk carries `0`; restoring it (from the pre-rebase cached
/// ops, which match the compile-time hash) lets caret lookups by `op_hash` hit.
fn restore_op_hash(chunk: &mut Chunk) {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    chunk.ops.hash(&mut h);
    chunk.constants.hash(&mut h);
    chunk.op_hash = h.finish();
    for sub in &mut chunk.sub_chunks {
        restore_op_hash(sub);
    }
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

/// An exclusive advisory lock (`flock`) over the shard, held for the duration of
/// a read-modify-write. Up to 16 instances share one shard; without this, two
/// writers both load the same base, both rewrite it, and the losing rename drops
/// the other's entry. Serializing the whole load→append→write under the lock
/// makes every writer re-read the latest shard, so no entry is ever lost — while
/// keeping the single authoritative store (readers still mmap the renamed file;
/// the lock is a sibling `.lock`, never in the read path).
struct ShardLock(std::fs::File);

impl ShardLock {
    fn acquire() -> Option<Self> {
        use std::os::unix::io::AsRawFd;
        let path = shard_path()?.with_extension("lock");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .ok()?;
        // Blocking exclusive lock; released when `f` is dropped (fd closed).
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        Some(ShardLock(f))
    }
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
    let mut cp: CProg = bincode::deserialize(&entry.blob).ok()?;
    // Restore each chunk's serde-skipped `op_hash` so runtime caret lookups match
    // the cached position tables' keys.
    restore_op_hash(&mut cp.main);
    for (_, f) in &mut cp.functions {
        restore_op_hash(&mut f.chunk);
    }
    // `try` bodies/handlers/else/finally run as their own chunks (in `tries`, not
    // `main.sub_chunks`), so their `op_hash` must be restored too or a caret for
    // an error inside a `try` block would be lost on a cache hit.
    for t in &mut cp.tries {
        restore_op_hash(&mut t.body);
        for (typ, _, handler) in &mut t.handlers {
            if let Some(typ) = typ {
                restore_op_hash(typ);
            }
            restore_op_hash(handler);
        }
        if let Some(e) = &mut t.orelse {
            restore_op_hash(e);
        }
        if let Some(f) = &mut t.finalbody {
            restore_op_hash(f);
        }
    }
    Some(Program {
        main: cp.main,
        functions: cp.functions,
        procs: Vec::new(),
        tries: cp.tries,
        warnings: cp.warnings,
        positions: cp.positions,
    })
}

// ── Introspection (`--doctor` / `--cacheview` / `--cache-clear`) ─────────────
// Read-only accessors mirroring the fleet's cache-introspection surface
// (elisprs `src/cache.rs`): the CLI extensions render these; the hot path
// (`load`/`store`) never touches them.

/// The schema version stamped into every cache key. Bumping it (see `SCHEMA`)
/// invalidates every prior entry cleanly.
pub const fn schema_version() -> u64 {
    SCHEMA
}

/// Path to the on-disk bytecode shard (`~/.pythonrs/scripts.rkyv`). Falls back to
/// a `/tmp` path only when there is no home directory.
pub fn default_cache_path() -> PathBuf {
    shard_path().unwrap_or_else(|| PathBuf::from("/tmp/.pythonrs/scripts.rkyv"))
}

/// `PYTHONRS_CACHE=0|false|no` disables the transparent bytecode cache (every run
/// recompiles). Any other value — or unset — leaves it on.
pub fn cache_enabled() -> bool {
    !matches!(
        std::env::var("PYTHONRS_CACHE").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    )
}

/// Entry count and on-disk byte size of the shard (`0`/`0` when absent).
pub fn stats() -> (usize, u64) {
    let count = load_shard().entries.len();
    let bytes = default_cache_path()
        .metadata()
        .map(|m| m.len())
        .unwrap_or(0);
    (count, bytes)
}

/// Delete the on-disk shard. A missing shard is success (nothing to clear).
pub fn clear() -> std::io::Result<()> {
    let Some(path) = shard_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A read-only summary of one cached program, for `--cacheview`. The counts are
/// decoded from the inner bincode blob; a blob that fails to decode (stale
/// format) reports zeros rather than aborting the listing.
pub struct EntryInfo {
    /// The `FxHash` lookup key (source + schema).
    pub key: u64,
    /// The source name that produced the entry (script path / `<string>` /
    /// `<stdin>`), for provenance in the listing.
    pub source: String,
    /// The independent SipHash verification hash.
    pub verify: u64,
    /// On-disk size of the compiled blob in bytes.
    pub blob_len: usize,
    /// Ops in the `__main__` chunk.
    pub main_ops: usize,
    /// Compiled top-level functions.
    pub functions: usize,
    /// `try` bodies (own chunks, not in `main.sub_chunks`).
    pub tries: usize,
    /// Compile-time `SyntaxWarning`s stored with the program.
    pub warnings: usize,
}

/// Summarize every entry in the shard (decoding each blob's counts), in shard
/// order. Used only by `--cacheview`.
pub fn entries() -> Vec<EntryInfo> {
    load_shard()
        .entries
        .iter()
        .map(|e| {
            let (main_ops, functions, tries, warnings) = bincode::deserialize::<CProg>(&e.blob)
                .map(|cp| {
                    (
                        cp.main.ops.len(),
                        cp.functions.len(),
                        cp.tries.len(),
                        cp.warnings.len(),
                    )
                })
                .unwrap_or((0, 0, 0, 0));
            EntryInfo {
                key: e.key,
                source: e.source.clone(),
                verify: e.verify,
                blob_len: e.blob.len(),
                main_ops,
                functions,
                tries,
                warnings,
            }
        })
        .collect()
}

/// Store `prog` (compiled from `src`) into the shard, replacing any prior entry.
/// The `--cacheview` provenance label defaults to the runtime's traceback
/// filename (a script path, `<string>`, or `<stdin>`).
pub fn store(src: &str, prog: &Program) -> Result<(), String> {
    let source = crate::host::with_host(|h| h.tb_filename.clone());
    store_labeled(src, prog, &source)
}

/// [`store`] with an explicit provenance `source` label. Imported modules pass
/// their own file path here so `--cacheview` shows the module — otherwise every
/// module compiled during a `python -c` run would inherit the `<string>` label
/// of that run and the listing would read as `-c` churn instead of real modules.
pub fn store_labeled(src: &str, prog: &Program, source: &str) -> Result<(), String> {
    let key = key_for(src);
    let verify = verify_for(src);
    let source = source.to_string();
    let cp = CProg {
        main: prog.main.clone(),
        functions: prog.functions.clone(),
        tries: prog.tries.clone(),
        warnings: prog.warnings.clone(),
        positions: prog.positions.clone(),
    };
    let blob = bincode::serialize(&cp).map_err(|e| format!("cache encode: {e}"))?;
    // Serialize the whole read-modify-write against the other instances sharing
    // the shard, so concurrent writers merge instead of clobbering. The lock is
    // held until this function returns (the guard drops). If the lock cannot be
    // taken, fall through unlocked — a dropped entry only costs a recompile.
    let _guard = ShardLock::acquire();
    let mut shard = load_shard();
    shard.entries.retain(|e| e.key != key);
    shard.entries.push(Entry {
        key,
        verify,
        source,
        blob,
    });
    write_shard(&shard)
}

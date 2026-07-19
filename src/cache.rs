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

/// Bump on any incompatible change to `CProg` / the lowering.
const SCHEMA: u64 = 1;

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
    blob: Vec<u8>,
}

/// The inner, serde/bincode form of a compiled program.
#[derive(Serialize, Deserialize)]
struct CProg {
    main: Chunk,
    functions: Vec<(String, FuncDef)>,
    tries: Vec<TryDef>,
}

/// A stable content key for a source string.
pub fn key_for(src: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    SCHEMA.hash(&mut h);
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
    std::fs::write(&path, &bytes).map_err(|e| format!("cache write: {e}"))
}

/// Look up a compiled program for `src`, if present and current.
pub fn load(src: &str) -> Option<Program> {
    let key = key_for(src);
    let shard = load_shard();
    let entry = shard.entries.iter().find(|e| e.key == key)?;
    let cp: CProg = bincode::deserialize(&entry.blob).ok()?;
    Some(Program {
        main: cp.main,
        functions: cp.functions,
        procs: Vec::new(),
        tries: cp.tries,
    })
}

/// Store `prog` (compiled from `src`) into the shard, replacing any prior entry.
pub fn store(src: &str, prog: &Program) -> Result<(), String> {
    let key = key_for(src);
    let cp = CProg {
        main: prog.main.clone(),
        functions: prog.functions.clone(),
        tries: prog.tries.clone(),
    };
    let blob = bincode::serialize(&cp).map_err(|e| format!("cache encode: {e}"))?;
    let mut shard = load_shard();
    shard.entries.retain(|e| e.key != key);
    shard.entries.push(Entry { key, blob });
    write_shard(&shard)
}

//! `python --cacheview` — list the compiled programs held in the rkyv bytecode
//! shard (`~/.pythonrs/scripts.rkyv`). The fleet analogue is zshrs `dbview`: an
//! inspection-only view of the cache, never consulted on the hot path.
//!
//! Each row is one cached program: its lookup/verify hashes, on-disk blob size,
//! and the decoded op / function / try / warning counts. Explicit,
//! user-requested output only.

use super::{bold, dim, format_bytes, green, yellow};
use crate::cache;

/// Print the shard listing. Returns a process exit code (always 0 — an empty or
/// absent shard is a normal state, reported not failed).
pub fn run() -> i32 {
    let path = cache::default_cache_path();
    println!("{}", bold("pythonrs bytecode cache"));
    println!("{}", dim(&"=".repeat(60)));
    println!("  path:    {}", path.display());
    println!("  schema:  v{}", cache::schema_version());

    if !path.exists() {
        println!("  {}", yellow("(absent — nothing compiled yet)"));
        return 0;
    }

    let entries = cache::entries();
    let (count, bytes) = cache::stats();
    println!(
        "  entries: {}   size: {}",
        green(&count.to_string()),
        format_bytes(bytes),
    );
    println!();

    if entries.is_empty() {
        println!("  {}", dim("(shard exists but holds no entries)"));
        return 0;
    }

    // Column header. Padding is applied to plain (ANSI-free) text — coloring a
    // `{:>18}` cell would count the escape bytes toward the width and break
    // alignment — so the whole header line is bolded after it is laid out.
    let header = format!(
        "  {:>18}  {:>18}  {:>9}  {:>5}  {:>5}  {:>5}  {:>4}",
        "key", "verify", "blob", "ops", "fns", "try", "warn",
    );
    println!("{}", bold(&header));
    println!("  {}", dim(&"-".repeat(74)));

    let mut total_ops = 0usize;
    let mut total_fns = 0usize;
    for e in &entries {
        total_ops += e.main_ops;
        total_fns += e.functions;
        println!(
            "  {:>18}  {:>18}  {:>9}  {:>5}  {:>5}  {:>5}  {:>4}",
            format!("{:016x}", e.key),
            format!("{:016x}", e.verify),
            format_bytes(e.blob_len as u64),
            e.main_ops,
            e.functions,
            e.tries,
            e.warnings,
        );
    }
    println!("  {}", dim(&"-".repeat(74)));
    println!(
        "  {} programs, {} main ops, {} functions",
        entries.len(),
        total_ops,
        total_fns,
    );
    println!();
    println!("  {}", dim("clear with: python --cache-clear"));

    0
}

//! `python --cacheview` — list the compiled programs held in the rkyv bytecode
//! shard (`~/.pythonrs/scripts.rkyv`). The fleet analogue is zshrs `dbview`: an
//! inspection-only view of the cache, never consulted on the hot path.
//!
//! Each row is one cached program: its lookup/verify hashes, on-disk blob size,
//! and the decoded op / function / try / warning counts. Explicit,
//! user-requested output only.

use super::{bold, dim, format_bytes, green, yellow};
use crate::cache;

/// Fit `s` into `width` columns, keeping the tail (the filename end of a path is
/// the identifying part) with a leading `…` when it overflows.
fn trunc_tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (width - 1)).collect();
    format!("…{tail}")
}

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
    // cell would count the escape bytes toward the width and break alignment — so
    // the whole header line is bolded after it is laid out. `source` (the script
    // path / `<string>` / `<stdin>`) leads so an entry is identifiable at a glance.
    const SRC_W: usize = 40;
    let header = format!(
        "  {:<srcw$}  {:>8}  {:>9}  {:>5}  {:>5}  {:>5}  {:>4}",
        "source",
        "key",
        "blob",
        "ops",
        "fns",
        "try",
        "warn",
        srcw = SRC_W,
    );
    println!("{}", bold(&header));
    println!("  {}", dim(&"-".repeat(SRC_W + 42)));

    let mut total_ops = 0usize;
    let mut total_fns = 0usize;
    for e in &entries {
        total_ops += e.main_ops;
        total_fns += e.functions;
        let source = if e.source.is_empty() {
            "<unknown>"
        } else {
            &e.source
        };
        println!(
            "  {:<srcw$}  {:>8}  {:>9}  {:>5}  {:>5}  {:>5}  {:>4}",
            trunc_tail(source, SRC_W),
            // A short 8-hex prefix of the lookup key — enough to disambiguate a
            // listing without the full 64-bit hash crowding out the path.
            format!("{:08x}", e.key as u32),
            format_bytes(e.blob_len as u64),
            e.main_ops,
            e.functions,
            e.tries,
            e.warnings,
            srcw = SRC_W,
        );
    }
    println!("  {}", dim(&"-".repeat(SRC_W + 42)));
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

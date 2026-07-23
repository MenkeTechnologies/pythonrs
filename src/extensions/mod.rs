//! CLI extensions beyond the core interpreter — diagnostics and cache
//! inspection that the fleet siblings (zshrs `--doctor`, elisprs `--cache-*`)
//! ship. Each submodule exposes a single `run()` that prints a report and
//! returns a process exit code. None of this is on the hot path; it exists so a
//! `python --doctor` / `--cacheview` tells the truth about the runtime and the
//! rkyv bytecode shard without the user reading source.

pub mod cacheview;
pub mod doctor;

// ── Shared rendering helpers ─────────────────────────────────────────────────
// Raw ANSI (matching the ported zshrs `run_doctor` style) so the extensions are
// self-contained. Color is unconditional — these reports are explicit,
// user-requested output, never emitted on a normal run.

pub(crate) fn green(s: &str) -> String {
    format!("\x1b[32m{s}\x1b[0m")
}
pub(crate) fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}
pub(crate) fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}
pub(crate) fn cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}
pub(crate) fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[0m")
}
pub(crate) fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

/// Human-readable byte size (`1.5 KiB`, `2.0 MiB`), matching zshrs `format_bytes`.
pub(crate) fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = n as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

//! Differential parity harness (development tool): run the example corpus
//! through pythonrs and the reference `python3`, diffing stdout. Needs `python3`
//! on PATH, so CI never runs it.
//!
//! What this harness CANNOT report, by construction — it compares one stream of
//! one run of each interpreter:
//!
//! * **stderr** is dropped, so a traceback-text or warning divergence is
//!   invisible here (`parity-fuzz --stderr` compares a normalized last line).
//! * **the exit code** is not compared at all: a corpus script that ran to
//!   completion on both sides passes even if one exited non-zero.
//! * **the environment** is inherited whole. Nothing is pinned — not
//!   `PYTHONHASHSEED`, not `LC_ALL` — so a run is only as reproducible as the
//!   shell it was started from.
//! * **file-system effects** are not diffed; only what reached stdout is.
//!
//! There is no frozen replay of these outputs. A no-`python3` machine measures
//! nothing here; the CI-safe coverage is `tests/lang.rs` and friends, which
//! assert against values transcribed from CPython rather than against a live
//! oracle — and therefore catch a REGRESSION, never a divergence CPython and
//! pythonrs have always disagreed on.

use std::path::Path;
use std::process::Command;

fn main() {
    let dir = Path::new("examples");
    if !dir.exists() {
        eprintln!("parity: no examples/ directory");
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "py").unwrap_or(false))
        .collect();
    files.sort();

    // Our `python` binary is a sibling of this harness binary.
    let ours_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("python")))
        .unwrap_or_else(|| Path::new("python").to_path_buf());

    let mut pass = 0;
    let mut fail = 0;
    for f in &files {
        let ours = Command::new(&ours_bin)
            .arg(f)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let theirs = Command::new("python3")
            .arg(f)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        match (ours, theirs) {
            (Some(a), Some(b)) if a == b => {
                pass += 1;
                println!("ok   {}", f.display());
            }
            (Some(a), Some(b)) => {
                fail += 1;
                println!("DIFF {}\n  ours:   {a:?}\n  python: {b:?}", f.display());
            }
            (None, _) => {
                fail += 1;
                println!("ERR  {} (pythonrs failed to run)", f.display());
            }
            (_, None) => {
                println!("skip {} (no python3)", f.display());
            }
        }
    }
    println!("\nparity: {pass} passed, {fail} failed");
}

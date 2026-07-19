//! Differential parity harness (development tool): run the example corpus
//! through pythonrs and the reference `python3`, diffing stdout. Needs `python3`
//! on PATH, so CI never runs it. Frozen outputs live in
//! tests/data/parity_expected.txt for the no-`python3` replay in tests/parity.rs.

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

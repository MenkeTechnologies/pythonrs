//! Differential parity harness (development tool): run the example corpus
//! through pythonrs and the reference CPython, diffing stdout. Needs a CPython
//! to compare against, so CI never runs it.
//!
//! The oracle is resolved to an ABSOLUTE path by `pythonrs::oracle` — the same
//! resolver `tests/parity.rs` and the fuzzer use — and printed before anything
//! is compared. A bare `python3` names a PATH lookup rather than a toolchain, so
//! a run that spawned one could not be attributed to any particular CPython
//! afterwards; a shim, a venv, or a pyenv/Homebrew shadow all answer to it.
//!
//! What this harness CANNOT report, by construction — it compares one stream of
//! one run of each interpreter:
//!
//! * **stderr** is dropped, so a traceback-text or warning divergence is
//!   invisible here (`parity-fuzz --stderr` compares a normalized last line).
//! * **the exit code** is not compared at all: a corpus script that ran to
//!   completion on both sides passes even if one exited non-zero.
//! * **the environment** is pinned (`PYTHONHASHSEED`, `LC_ALL`, `TZ`,
//!   `PYTHONIOENCODING`) so the two sides are comparable and a rerun agrees with
//!   itself. Unpinned, the reference randomizes string hashing per process and
//!   with it every string-keyed container's iteration order, so a corpus script
//!   that prints one would diverge at random.
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
    std::process::exit(run());
}

/// Every exit here is non-zero unless the corpus was actually measured and every
/// script agreed.
///
/// This harness used to return `0` in four different ways that measured nothing
/// or measured a failure: a missing `examples/` printed a note and returned; an
/// `examples/` with no `.py` files ran the loop zero times; a machine without
/// `python3` printed `skip` for every file; and a run with `fail > 0` still fell
/// off the end of `main`. In all four the last line was
/// `parity: N passed, M failed` and the exit status was success, so any caller
/// reading the status — a shell `&&`, a CI step — saw a green harness that had
/// compared nothing. `scripts/dropin_check.sh` already refuses an empty corpus
/// and a missing reference; this one did not.
/// `bin`'s stdout on `f`, under an environment pinned hard enough that the two
/// sides are comparable — nothing is inherited from the invoking shell.
fn stdout_of(bin: &Path, f: &Path) -> Option<String> {
    Command::new(bin)
        .arg(f)
        .env("PYTHONHASHSEED", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("TZ", "UTC")
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn run() -> i32 {
    let dir = Path::new("examples");
    if !dir.exists() {
        eprintln!("parity: no examples/ directory — measured nothing");
        return 2;
    }
    let mut files: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "py").unwrap_or(false))
            .collect(),
        Err(e) => {
            eprintln!("parity: cannot read examples/: {e}");
            return 2;
        }
    };
    files.sort();
    if files.is_empty() {
        eprintln!("parity: examples/ matched zero *.py scripts — measured nothing");
        return 2;
    }

    let oracle = match pythonrs::oracle::resolve() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parity: {e} — measured nothing");
            return 2;
        }
    };
    println!("parity: oracle {}", pythonrs::oracle::identify(&oracle));

    // Our `python` binary is a sibling of this harness binary.
    let ours_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("python")))
        .unwrap_or_else(|| Path::new("python").to_path_buf());

    let mut pass = 0;
    let mut fail = 0;
    let mut skipped = 0;
    for f in &files {
        let ours = stdout_of(&ours_bin, f);
        let theirs = stdout_of(&oracle, f);
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
                skipped += 1;
                println!("skip {} (oracle failed to run)", f.display());
            }
        }
    }
    println!("\nparity: {pass} passed, {fail} failed, {skipped} skipped");
    if skipped > 0 {
        eprintln!(
            "parity: FAIL — {skipped}/{} script(s) had no reference to compare against; \
             install a CPython or this run proves nothing",
            files.len()
        );
        return 2;
    }
    if fail > 0 {
        eprintln!(
            "parity: FAIL — {fail} of {} script(s) diverged",
            files.len()
        );
        return 1;
    }
    println!(
        "parity: PASS — compared {pass} script(s) against {}",
        pythonrs::oracle::identify(&oracle)
    );
    0
}

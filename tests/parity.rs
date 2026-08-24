//! Live differential parity: every probe in `tests/data/parity_probes.py` runs
//! under BOTH the reference `python3` and the built `python`, and their stdout,
//! stderr and exit code must agree.
//!
//! This is the harness shape the sibling frontends use (`rubylang/tests/parity.rs`,
//! `node-js/tests/parity.rs`), with one deliberate difference: those replay a
//! FROZEN transcript, this one asks the live reference. A frozen transcript can
//! only catch a regression away from whatever was recorded; a live oracle also
//! catches a divergence pythonrs and CPython have ALWAYS disagreed on, which is
//! most of what there is left to find. The cost is that it measures nothing on a
//! machine with no `python3` — so it SKIPS there rather than failing, and CI
//! stays green either way. `tests/lang.rs` and friends carry the frozen,
//! always-runs coverage.
//!
//! Both interpreters run the same probe FILE from the same path, so a traceback
//! naming that path is comparable rather than trivially different.
//!
//! What is compared, and why not uniformly:
//!
//! * **stdout** and the **exit code**, byte for byte, against any reference from
//!   CPython 3.9 on. The corpus is written to be version-stable on stdout.
//! * **stderr** byte for byte only against a reference new enough to render
//!   tracebacks the way pythonrs targets (3.13+, where the `~^~` caret anchors
//!   settled). Against an older reference the comparison narrows to the final
//!   `ExcType: message` line, whose wording is stable much further back.
//!   Narrowing rather than skipping keeps an old-reference run meaningful.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Probe separator inside the corpus file.
const SEP: &str = "\n#==#\n";

/// The built `python` under test.
fn ours() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_python"))
}

/// `(major, minor)` of the reference on PATH, or `None` when there is no usable
/// `python3` — the signal to skip.
///
/// The version is read from `sys.version_info` rather than parsed out of
/// `--version`, because it also serves as a liveness check: a `python3` that is
/// a broken shim, or a stub that prints a version and cannot execute anything,
/// fails here instead of being mistaken for a working oracle and reported as a
/// wall of divergences.
fn reference_version() -> Option<(u32, u32)> {
    let out = Command::new("python3")
        .arg("-c")
        .arg("import sys; print(sys.version_info[0], sys.version_info[1])")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut it = text.split_whitespace();
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// Split the corpus into probes, dropping the leading comment banner and any
/// block that is only comments and blank lines.
fn probes(text: &str) -> Vec<String> {
    text.split(SEP)
        .map(|s| s.trim_matches('\n').to_string())
        .filter(|s| {
            s.lines()
                .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        })
        .collect()
}

/// Run `bin` on `path` with an environment pinned hard enough that the two sides
/// are comparable.
///
/// Nothing here is inherited. `PYTHONHASHSEED` fixes `str`/`bytes` hashing, and
/// with it every string-keyed container's iteration order — unpinned, the
/// reference randomizes it per process and the two sides would disagree at
/// random. `TZ`/`LANG`/`LC_ALL` pin the two places CPython reads the ambient
/// locale. `PYTHONIOENCODING` keeps stdout UTF-8 whatever the terminal claims.
fn run(bin: &Path, path: &Path) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let out = Command::new(bin)
        .arg(path)
        .env("PYTHONHASHSEED", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("TZ", "UTC")
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", bin.display()));
    (out.stdout, out.stderr, out.status.code())
}

/// The last non-blank line of a stderr block — the `ExcType: message` line of a
/// traceback. This is the part whose wording is stable across CPython releases,
/// so it is what an older reference is held to.
fn final_line(err: &[u8]) -> String {
    String::from_utf8_lossy(err)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .next_back()
        .unwrap_or("")
        .to_string()
}

#[test]
fn probes_match_the_reference_python3() {
    let Some((major, minor)) = reference_version() else {
        eprintln!(
            "parity: SKIP — no usable `python3` on PATH, so there is no oracle to \
             compare against. This test measures nothing here by design; the \
             always-runs coverage is tests/lang.rs and friends."
        );
        return;
    };
    // stderr is compared in full only against a reference that renders tracebacks
    // the way pythonrs targets.
    let full_stderr = (major, minor) >= (3, 13);

    let corpus_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/parity_probes.py");
    let corpus = std::fs::read_to_string(&corpus_path)
        .unwrap_or_else(|e| panic!("missing corpus {}: {e}", corpus_path.display()));
    let probes = probes(&corpus);

    // An empty or unparsable corpus satisfies every check below — the loop runs
    // zero times and the terminal assertion passes having compared nothing. A
    // separator that stopped matching reduces the whole file to one probe, which
    // is the same failure wearing a different number, so the floor is a count
    // rather than merely non-empty.
    assert!(
        probes.len() >= 10,
        "corpus parsed to {} probe(s) — the separator is stale or the file was \
         truncated; a run over nothing is not a passing parity run",
        probes.len()
    );

    let dir = std::env::temp_dir().join(format!("pythonrs-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create probe dir");
    let ours_bin = ours();

    let mut failures = Vec::new();
    for (i, probe) in probes.iter().enumerate() {
        // One file per probe, run by BOTH sides, so a traceback's `File "…"` line
        // carries the same path on each and compares as content rather than noise.
        let path = dir.join(format!("probe_{i:03}.py"));
        std::fs::write(&path, format!("{probe}\n")).expect("write probe");

        let (ref_out, ref_err, ref_code) = run(Path::new("python3"), &path);
        let (our_out, our_err, our_code) = run(&ours_bin, &path);
        let head = probe
            .lines()
            .find(|l| !l.trim_start().starts_with('#'))
            .unwrap_or("");

        // Compared as BYTES: `from_utf8_lossy` maps every invalid sequence to the
        // same replacement char, so two different invalid outputs would compare
        // equal and a real divergence could go unreported.
        if our_out != ref_out {
            failures.push(format!(
                "── probe #{i} ({head}) STDOUT\n  python3 : {:?}\n  pythonrs: {:?}",
                String::from_utf8_lossy(&ref_out),
                String::from_utf8_lossy(&our_out),
            ));
        }
        if our_code != ref_code {
            failures.push(format!(
                "── probe #{i} ({head}) EXIT\n  python3 : {ref_code:?}\n  pythonrs: {our_code:?}",
            ));
        }
        // A pythonrs PANIC exits non-zero having printed no Python diagnostic at
        // all. Where the reference also failed, the exit-code check above is
        // satisfied by it, so it is called out on its own terms.
        if String::from_utf8_lossy(&our_err).contains("panicked at") {
            failures.push(format!(
                "── probe #{i} ({head}) PANIC\n  pythonrs: {:?}",
                String::from_utf8_lossy(&our_err)
            ));
            continue;
        }
        let (want_err, got_err) = if full_stderr {
            (
                String::from_utf8_lossy(&ref_err).to_string(),
                String::from_utf8_lossy(&our_err).to_string(),
            )
        } else {
            (final_line(&ref_err), final_line(&our_err))
        };
        if want_err != got_err {
            failures.push(format!(
                "── probe #{i} ({head}) STDERR{}\n  python3 : {want_err:?}\n  pythonrs: {got_err:?}",
                if full_stderr { "" } else { " (final line only)" }
            ));
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "pythonrs diverged from python3 {major}.{minor} on {} of {} probe(s):\n\n{}",
        failures.len(),
        probes.len(),
        failures.join("\n\n")
    );
}

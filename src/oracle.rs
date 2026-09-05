//! Resolving the reference CPython the differential harnesses compare against.
//!
//! Every parity result is the sentence "pythonrs disagrees with THIS
//! interpreter", so *which* interpreter it was is part of the result, not
//! incidental setup. Two failure modes this module exists to close:
//!
//! * **A bare `python3` is not an identity.** It is a PATH lookup, and the PATH
//!   a harness ran under is not the PATH whoever reads the report has. A shim, a
//!   venv, a pyenv/conda shadow or a Homebrew/system split all answer to the same
//!   four letters and are different toolchains. Only an absolute path names one.
//! * **A harness that never says what it resolved is unfalsifiable.** A run that
//!   silently compared against the wrong CPython reports either a wall of
//!   divergences that are really version drift, or a clean sheet that measured
//!   the wrong thing. Neither is distinguishable from a correct run unless the
//!   oracle is printed.
//!
//! So [`resolve`] always answers with an absolute path, and every harness prints
//! [`identify`] before it compares anything.
//!
//! `PYTHONRS_ORACLE` (or the older `PYTHONRS_FUZZ_PYTHON`) names the oracle
//! explicitly. If one is set but unusable that is a HARD error rather than a
//! fallback: silently comparing against a different CPython answers a different
//! question than the one that was asked.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The environment variables that name an oracle explicitly, newest first.
pub const ORACLE_VARS: [&str; 2] = ["PYTHONRS_ORACLE", "PYTHONRS_FUZZ_PYTHON"];

/// The candidates tried, in order, when nothing names an oracle.
const CANDIDATES: [&str; 4] = [
    "python3",
    "/usr/bin/python3",
    "/opt/homebrew/bin/python3",
    "python",
];

/// `<prog> --version` output, or `None` when the program cannot be run at all.
///
/// This doubles as a LIVENESS check, which is why it is the probe rather than a
/// mere `Path::exists`: a broken shim or a stub that prints a version and can
/// execute nothing fails here instead of being mistaken for a working oracle and
/// reported as a wall of divergences. CPython has printed the version to stdout
/// since 3.4 and to stderr before that, so both streams are read.
pub fn version_of(prog: &Path) -> Option<String> {
    let o = Command::new(prog).arg("--version").output().ok()?;
    let mut s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if s.is_empty() {
        s = String::from_utf8_lossy(&o.stderr).trim().to_string();
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The absolute path `name` resolves to, or `None` when it is not runnable.
///
/// An already-absolute path is returned as given (not canonicalized: a
/// deliberately-named symlink such as a venv's `bin/python3` is the answer the
/// caller asked for, and resolving it through to the interpreter it points at
/// would report something they did not name). A bare name is looked up along
/// `PATH` exactly as the shell would, so the recorded path is the file that
/// actually ran.
fn absolutize(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() || name.contains('/') {
        return version_of(p).is_some().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file() && version_of(c).is_some())
}

/// The oracle to compare against, as an absolute path.
///
/// `Err` carries a message ready to print; every caller treats it as fatal,
/// except the integration test, which SKIPS on it — a machine with no CPython
/// has no oracle, and a test that measures nothing there is honest where a
/// failing one would only be noise.
pub fn resolve() -> Result<PathBuf, String> {
    for var in ORACLE_VARS {
        let Ok(p) = std::env::var(var) else { continue };
        if p.is_empty() {
            continue;
        }
        return absolutize(&p).ok_or_else(|| format!("{var}={p}: not a usable python"));
    }
    for c in CANDIDATES {
        if let Some(p) = absolutize(c) {
            return Ok(p);
        }
    }
    Err(format!(
        "no reference python3 found on PATH; set {}=/path/to/python3",
        ORACLE_VARS[0]
    ))
}

/// `<absolute path> (<version>)` — what every harness prints before comparing,
/// and what a divergence record is stamped with, so a result can be attributed
/// to the exact interpreter that produced it.
pub fn identify(oracle: &Path) -> String {
    let v = version_of(oracle).unwrap_or_else(|| "unknown".to_string());
    format!("{} ({v})", oracle.display())
}

/// `(major, minor)` of an oracle, parsed from `sys.version_info` rather than
/// from `--version` text.
///
/// A harness gates on this (traceback rendering settled in 3.13, so full-stderr
/// comparison only means something from there on), and a gate must not be
/// decided by string-scraping a banner that a distributor is free to reword.
/// Running code for it also re-proves the interpreter can execute, not merely
/// start.
pub fn version_info(oracle: &Path) -> Option<(u32, u32)> {
    let out = Command::new(oracle)
        .arg("-c")
        .arg("import sys; print(sys.version_info[0], sys.version_info[1])")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

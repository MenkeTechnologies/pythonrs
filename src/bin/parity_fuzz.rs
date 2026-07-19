//! Differential parity fuzzer: `python3 -c <s>` vs our `python -c <s>`.
//!
//! Generates thousands of grammar-driven, deterministic-output Python snippets,
//! runs each through both interpreters, and reports every case where stdout OR
//! success/failure diverge. Each case is produced from a per-index seed so any
//! divergence replays exactly: `parity-fuzz --seed <N> --once`.
//!
//! The generator is biased toward the historically weak areas of a from-scratch
//! Python (float `repr`, integer `//`/`%` sign rules, bignum, slices, the
//! `format` mini-language, string methods). Pure random bytes only produce
//! mutual SyntaxErrors that agree on both sides and teach nothing.
//!
//! Determinism invariant: the generator NEVER emits a construct whose output is
//! nondeterministic for reasons unrelated to parity (`random`, `time`, `id()`,
//! object addresses, set iteration order). Every probe is wrapped in `print`
//! and any `set` is `sorted(...)` before printing, so every reported divergence
//! is a real parity gap, not a false positive. `PYTHONHASHSEED=0` is forced on
//! both children as a second belt.
//!
//! Subprocess-only: this binary never links the pythonrs library — it compares
//! two `python` processes, exactly as a user would observe them.
//!
//! Build:  cargo build --bin parity-fuzz
//! Run:    ./target/debug/parity-fuzz --count 5000

use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64) — no `rand` dependency.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

fn pick<'a, T>(rng: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[rng.below(xs.len() as u64) as usize]
}

// ---------------------------------------------------------------------------
// Binary resolution / invocation
// ---------------------------------------------------------------------------

/// Our `python` binary — the sibling of this harness binary.
fn ours_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_python") {
        return PathBuf::from(p);
    }
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        let cand = d.join("python");
        if cand.exists() {
            return cand;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("python")
}

/// The ORACLE — reference CPython. Every divergence this harness reports is
/// "pythonrs disagrees with THIS interpreter", so which interpreter it is, is
/// part of the result: CPython 3.12 and 3.14 differ (e.g. error wording, some
/// `repr`s), so a baseline is only meaningful against the CPython that produced
/// it. `PYTHONRS_FUZZ_PYTHON` names the oracle explicitly; if it is set but
/// unusable this is a HARD ERROR — silently falling back to a different CPython
/// would answer a different question than the one that was asked.
fn resolve_oracle() -> String {
    if let Ok(p) = std::env::var("PYTHONRS_FUZZ_PYTHON") {
        if version_of(&p).is_none() {
            eprintln!("parity-fuzz: PYTHONRS_FUZZ_PYTHON={p}: not a usable python");
            std::process::exit(2);
        }
        return p;
    }
    for p in [
        "python3",
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "python",
    ] {
        if version_of(p).is_some() {
            return p.to_string();
        }
    }
    eprintln!("parity-fuzz: no reference python3 found; set PYTHONRS_FUZZ_PYTHON");
    std::process::exit(2);
}

/// `<prog> --version` output, or None if the program can't be run.
fn version_of(prog: &str) -> Option<String> {
    let o = Command::new(prog).arg("--version").output().ok()?;
    if !o.status.success() && o.stdout.is_empty() && o.stderr.is_empty() {
        return None;
    }
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

/// `<path> (<version>)`, for the run header and the report file, so a divergence
/// record can be attributed to the exact oracle that produced it.
fn oracle_id(oracle: &str) -> String {
    let v = version_of(oracle).unwrap_or_else(|| "unknown".to_string());
    format!("{oracle} ({v})")
}

static CMP_STDERR: AtomicBool = AtomicBool::new(false);

/// Raw bytes, never `String`: an interpreter legitimately emits output that is
/// not valid UTF-8. `read_to_string` FAILS on such a stream and leaves the
/// buffer empty, so both sides would report "" and silently agree — a
/// divergence the harness could never see. Comparing bytes (and only ever
/// lossy-rendering for the human report) keeps the byte surface honest.
struct RunOut {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: i32,
    timed_out: bool,
}

/// Render captured bytes for a report. Invalid UTF-8 is shown lossily AND
/// followed by a hex line — two different invalid byte strings both render to
/// U+FFFD, so without the hex the record would show a divergence as identical
/// text.
fn render(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    if std::str::from_utf8(bytes).is_err() {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        return format!("{text}\n  (hex) {}", hex.join(" "));
    }
    text.to_string()
}

/// Best-effort stderr normalization for `--stderr`: CPython prints a multi-line
/// traceback, pythonrs its own format, so we collapse to the last non-empty
/// line (usually `ExceptionType: message`) lowercased. Cross-interpreter stderr
/// rarely matches verbatim; this is a loose "same error class" check.
fn norm_stderr(s: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(s);
    let last = text
        .lines()
        .map(|l| l.trim())
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_lowercase();
    last.into_bytes()
}

/// A parity gap: stdout bytes differ, OR one side accepted the program (exit 0)
/// while the other rejected it. We compare success-ness, not the exact exit
/// code — a from-scratch interpreter is free to pick its own nonzero code for
/// an uncaught exception, so "both rejected it" is agreement, not a gap.
fn differs(oracle: &RunOut, ours: &RunOut) -> bool {
    if (oracle.exit == 0) != (ours.exit == 0) {
        return true;
    }
    if oracle.stdout != ours.stdout {
        return true;
    }
    if CMP_STDERR.load(Ordering::Relaxed)
        && norm_stderr(&oracle.stderr) != norm_stderr(&ours.stderr)
    {
        return true;
    }
    false
}

/// Run `<prog> -c <src>` with a wall-clock timeout enforced by a watchdog: two
/// reader threads drain stdout/stderr (so a large writer can't deadlock on a
/// full pipe) while the main thread polls `try_wait` and `kill()`s on overrun.
fn run_prog(prog: &Path, src: &str, timeout: Duration, oracle_env: bool) -> RunOut {
    let mut cmd = Command::new(prog);
    cmd.arg("-c")
        .arg(src)
        .env("PYTHONHASHSEED", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if oracle_env {
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            return RunOut {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: -1,
                timed_out: false,
            }
        }
    };

    let mut out_h = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = o.read_to_end(&mut b);
            b
        })
    });
    let mut err_h = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = e.read_to_end(&mut b);
            b
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let s = child.wait().ok();
                    exit = s.and_then(|s| s.code()).unwrap_or(-1);
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                exit = -1;
                break;
            }
        }
    }

    let stdout = out_h.take().and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_h.take().and_then(|h| h.join().ok()).unwrap_or_default();
    RunOut {
        stdout,
        stderr,
        exit,
        timed_out,
    }
}

fn build_program(stmts: &[String]) -> String {
    stmts.join("\n")
}

// ---------------------------------------------------------------------------
// Generators — each returns a statement list whose stdout is deterministic.
// ---------------------------------------------------------------------------

const INTS: &[&str] = &[
    "0", "1", "2", "3", "5", "7", "10", "-1", "-3", "-7", "42", "100", "-100", "1000",
];
const POSINTS: &[&str] = &["1", "2", "3", "4", "5", "6", "8", "10"];
const FLOATS: &[&str] = &[
    "0.1", "0.2", "0.5", "1.5", "2.0", "3.14", "10.0", "-1.5", "100.0", "0.0", "2.5", "1e3", "1e-3",
];
const STRS: &[&str] = &[
    "'hello'",
    "'World'",
    "'abc'",
    "'Python'",
    "''",
    "'a'",
    "'foo bar'",
    "'  pad  '",
    "'AbC'",
];
const WORDS: &[&str] = &["'a'", "'b'", "'c'", "'x'", "'y'", "'z'", "'ab'", "'cd'"];

fn gen_arith(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    // `**` only against a tiny exponent — a power tower (`10 ** 10 ** 10`) is
    // bignum's job, and would OOM here.
    let exp = pick(r, &["2", "3", "0", "-1"]);
    let op = pick(r, &["+", "-", "*", "//", "%"]);
    let e = match r.below(6) {
        0 => format!("{a} {op} {b}"),
        1 => format!("{a} + {b} * {c}"),
        2 => format!("({a} + {b}) * {c}"),
        3 => format!("-{a} ** {exp}"),
        4 => format!("{a} // {b} + {c} % {b}"),
        _ => format!("{a} {op} {b} {op} {c}"),
    };
    vec![format!("print({e})")]
}

fn gen_bignum(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    match r.below(3) {
        0 => {
            let n = 16 + r.below(240);
            vec![format!("print(2 ** {n})")]
        }
        1 => {
            let n = 8 + r.below(80);
            vec![format!("print(10 ** {n})")]
        }
        _ => {
            let n = 10 + r.below(40);
            vec![
                "f = 1".into(),
                format!("for i in range(1, {n}): f *= i"),
                "print(f)".into(),
            ]
        }
    }
}

fn gen_floatfmt(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, FLOATS);
    let b = pick(r, FLOATS);
    let k = r.below(6);
    let e = match r.below(7) {
        0 => "0.1 + 0.2".to_string(),
        1 => format!("{a} / {b}"),
        2 => format!("{a} * {b}"),
        3 => format!("round({a} / {b}, {k})"),
        4 => format!("{} ** 0.5", pick(r, POSINTS)),
        5 => format!("{a} + {b}"),
        _ => a.to_string(),
    };
    vec![format!("print({e})")]
}

fn gen_strings(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let s = pick(r, STRS);
    let n = r.below(4);
    let e = match r.below(5) {
        0 => format!("{s}[{}]", r.below(3)),
        1 => format!("{s} * {n}"),
        2 => format!("{s} + {}", pick(r, STRS)),
        3 => format!("{} in {s}", pick(r, WORDS)),
        _ => format!("len({s})"),
    };
    vec![format!("print({e})")]
}

fn gen_fstring(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let x = pick(r, FLOATS);
    let n = pick(r, INTS);
    let s = pick(r, STRS);
    let e = match r.below(6) {
        0 => format!("f\"{{{x}}}\""),
        1 => format!("f\"{{{x}:.2f}}\""),
        2 => format!("f\"{{{s}!r}}\""),
        3 => format!("f\"{{{n}:05d}}\""),
        4 => format!("f\"[{{{n}}}][{{{x}}}]\""),
        _ => format!("f\"{{{n} + {}}}\"", pick(r, INTS)),
    };
    vec![format!("print({e})")]
}

fn gen_slice(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let seq = pick(
        r,
        &[
            "'abcdefg'",
            "[1, 2, 3, 4, 5]",
            "(10, 20, 30, 40)",
            "'Python3'",
        ],
    );
    let idx = &["", "0", "1", "2", "-1", "-2", "3", "5"];
    let a = pick(r, idx);
    let b = pick(r, idx);
    let step = pick(r, &["", "1", "2", "-1", "-2"]);
    let e = if step.is_empty() {
        format!("{seq}[{a}:{b}]")
    } else {
        format!("{seq}[{a}:{b}:{step}]")
    };
    vec![format!("print({e})")]
}

fn gen_listcomp(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 3 + r.below(6);
    let e = match r.below(4) {
        0 => format!("[i * i for i in range({n})]"),
        1 => format!("[i for i in range({n}) if i % 2 == 0]"),
        2 => "[(i, j) for i in range(3) for j in range(2)]".to_string(),
        _ => "[c.upper() for c in 'abcd' if c != 'b']".to_string(),
    };
    vec![format!("print({e})")]
}

fn gen_dictcomp(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 3 + r.below(5);
    let e = match r.below(3) {
        0 => format!("{{i: i * i for i in range({n})}}"),
        1 => "{c: ord(c) for c in 'abc'}".to_string(),
        _ => format!("{{i: i % 2 == 0 for i in range({n})}}"),
    };
    vec![format!("print({e})")]
}

fn gen_setcomp(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 4 + r.below(8);
    // Sets iterate nondeterministically across runs/impls — always sort.
    let inner = match r.below(3) {
        0 => format!("{{i % 3 for i in range({n})}}"),
        1 => "{c for c in 'banana'}".to_string(),
        _ => format!("{{i * i % 5 for i in range({n})}}"),
    };
    vec![format!("print(sorted({inner}))")]
}

fn gen_sorting(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let lst = pick(
        r,
        &[
            "[3, 1, 2, 5, 4]",
            "[10, -1, 7, 0]",
            "['banana', 'apple', 'cherry']",
            "[2.5, 1.1, 3.3]",
        ],
    );
    let e = match r.below(6) {
        0 => format!("sorted({lst})"),
        1 => format!("sorted({lst}, reverse=True)"),
        2 => format!("sorted({lst}, key=lambda x: -x if isinstance(x, int) else x)"),
        3 => format!("min({lst})"),
        4 => format!("max({lst})"),
        _ => format!("sorted('{}')", "dcba"),
    };
    vec![format!("print({e})")]
}

fn gen_formatspec(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let x = pick(r, FLOATS);
    let n = pick(r, &["0", "1", "42", "255", "-7", "1000"]);
    let s = pick(r, WORDS);
    let e = match r.below(9) {
        0 => format!("'{{:.3f}}'.format({x})"),
        1 => format!("'{{:05d}}'.format({n})"),
        2 => format!("'{{:>8}}'.format({s})"),
        3 => format!("'{{:x}}'.format({n})"),
        4 => format!("'{{:b}}'.format({n})"),
        5 => format!("'{{:e}}'.format({x})"),
        6 => format!("'%d %s' % ({n}, {s})"),
        7 => format!("'%.2f' % {x}"),
        _ => format!("'{{:+.2f}}'.format({x})"),
    };
    vec![format!("print({e})")]
}

fn gen_boolint(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let e = match r.below(6) {
        0 => "True + True".to_string(),
        1 => format!("{a} and {b}"),
        2 => format!("{a} or {b}"),
        3 => format!("not {a}"),
        4 => format!("int({a} == {b})"),
        _ => format!("{a} if {b} else 0"),
    };
    vec![format!("print({e})")]
}

fn gen_ranges(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, &["0", "1", "2", "-3", "5", "10"]);
    let b = pick(r, &["0", "3", "5", "-2", "10", "-5"]);
    let c = pick(r, &["1", "2", "3", "-1", "-2"]);
    let e = match r.below(3) {
        0 => format!("list(range({a}, {b}, {c}))"),
        1 => format!("sum(range({}))", pick(r, POSINTS)),
        _ => format!("list(range({b}))"),
    };
    vec![format!("print({e})")]
}

fn gen_strmeth(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let s = pick(r, STRS);
    let e = match r.below(11) {
        0 => format!("{s}.upper()"),
        1 => format!("{s}.lower()"),
        2 => format!("{s}.split()"),
        3 => "','.join(['a', 'b', 'c'])".to_string(),
        4 => format!("{s}.replace('a', 'X')"),
        5 => format!("{s}.strip()"),
        6 => format!("{s}.find('o')"),
        7 => format!("{s}.count('a')"),
        8 => format!("{s}.startswith('h')"),
        9 => format!("'{}'.zfill({})", "42", r.below(6)),
        _ => format!("{s}.title()"),
    };
    vec![format!("print({e})")]
}

fn gen_comparison(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let e = match r.below(5) {
        0 => format!("{a} < {b} < {c}"),
        1 => format!("{a} == {b}"),
        2 => format!("{a} <= {b} <= {c}"),
        3 => format!("(1, 2) < (1, {})", pick(r, POSINTS)),
        // int vs float equality across the type boundary.
        _ => format!("{a} == float({a})"),
    };
    vec![format!("print({e})")]
}

fn gen_builtins(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, POSINTS);
    let b = pick(r, POSINTS);
    let n = pick(r, &["0", "7", "42", "255", "65"]);
    let x = pick(r, FLOATS);
    let e = match r.below(12) {
        0 => format!("divmod({}, {})", pick(r, INTS), b),
        1 => format!("abs({})", pick(r, INTS)),
        2 => format!("hex({n})"),
        3 => format!("oct({n})"),
        4 => format!("bin({n})"),
        5 => format!("pow({a}, {b})"),
        6 => format!("pow({a}, {b}, {})", pick(r, POSINTS)),
        7 => format!("round({x}, {})", r.below(4)),
        8 => "ord('A')".to_string(),
        9 => format!("chr({})", 65 + r.below(20)),
        10 => format!("int('{}', 2)", pick(r, &["101", "1111", "10"])),
        _ => format!("sum([{a}, {b}, {n}])"),
    };
    vec![format!("print({e})")]
}

fn gen_ternary(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let cond = pick(r, &["True", "False", "1 > 0", "0", "''", "[]"]);
    vec![format!("print({a} if {cond} else {b})")]
}

fn gen_augassign(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, INTS);
    let b = pick(r, POSINTS);
    let op = pick(r, &["+=", "-=", "*=", "//=", "%=", "**="]);
    vec![format!("x = {a}"), format!("x {op} {b}"), "print(x)".into()]
}

// ---------------------------------------------------------------------------
// Mode dispatch
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Mode {
    Mixed,
    Arith,
    Bignum,
    Floatfmt,
    Strings,
    Fstring,
    Slice,
    Listcomp,
    Dictcomp,
    Setcomp,
    Sorting,
    Formatspec,
    Boolint,
    Ranges,
    Strmeth,
    Comparison,
    Builtins,
    Ternary,
    Augassign,
}

const REAL_MODES: &[Mode] = &[
    Mode::Arith,
    Mode::Bignum,
    Mode::Floatfmt,
    Mode::Strings,
    Mode::Fstring,
    Mode::Slice,
    Mode::Listcomp,
    Mode::Dictcomp,
    Mode::Setcomp,
    Mode::Sorting,
    Mode::Formatspec,
    Mode::Boolint,
    Mode::Ranges,
    Mode::Strmeth,
    Mode::Comparison,
    Mode::Builtins,
    Mode::Ternary,
    Mode::Augassign,
];

/// Generate the statement list for a seed in the selected mode. `Mixed` rotates
/// across every real mode by seed, so a plain run exercises the whole surface.
fn gen_case(seed: u64, mode: Mode) -> Vec<String> {
    match mode {
        Mode::Mixed => {
            let m = REAL_MODES[(seed % REAL_MODES.len() as u64) as usize];
            gen_case(seed, m)
        }
        Mode::Arith => gen_arith(seed),
        Mode::Bignum => gen_bignum(seed),
        Mode::Floatfmt => gen_floatfmt(seed),
        Mode::Strings => gen_strings(seed),
        Mode::Fstring => gen_fstring(seed),
        Mode::Slice => gen_slice(seed),
        Mode::Listcomp => gen_listcomp(seed),
        Mode::Dictcomp => gen_dictcomp(seed),
        Mode::Setcomp => gen_setcomp(seed),
        Mode::Sorting => gen_sorting(seed),
        Mode::Formatspec => gen_formatspec(seed),
        Mode::Boolint => gen_boolint(seed),
        Mode::Ranges => gen_ranges(seed),
        Mode::Strmeth => gen_strmeth(seed),
        Mode::Comparison => gen_comparison(seed),
        Mode::Builtins => gen_builtins(seed),
        Mode::Ternary => gen_ternary(seed),
        Mode::Augassign => gen_augassign(seed),
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Mixed => "mixed",
        Mode::Arith => "arith",
        Mode::Bignum => "bignum",
        Mode::Floatfmt => "floatfmt",
        Mode::Strings => "strings",
        Mode::Fstring => "fstring",
        Mode::Slice => "slice",
        Mode::Listcomp => "listcomp",
        Mode::Dictcomp => "dictcomp",
        Mode::Setcomp => "setcomp",
        Mode::Sorting => "sorting",
        Mode::Formatspec => "formatspec",
        Mode::Boolint => "boolint",
        Mode::Ranges => "ranges",
        Mode::Strmeth => "strmeth",
        Mode::Comparison => "comparison",
        Mode::Builtins => "builtins",
        Mode::Ternary => "ternary",
        Mode::Augassign => "augassign",
    }
}

fn mode_from_name(s: &str) -> Option<Mode> {
    const ALL: &[Mode] = &[
        Mode::Mixed,
        Mode::Arith,
        Mode::Bignum,
        Mode::Floatfmt,
        Mode::Strings,
        Mode::Fstring,
        Mode::Slice,
        Mode::Listcomp,
        Mode::Dictcomp,
        Mode::Setcomp,
        Mode::Sorting,
        Mode::Formatspec,
        Mode::Boolint,
        Mode::Ranges,
        Mode::Strmeth,
        Mode::Comparison,
        Mode::Builtins,
        Mode::Ternary,
        Mode::Augassign,
    ];
    ALL.iter().copied().find(|&m| mode_name(m) == s)
}

// ---------------------------------------------------------------------------
// Divergence check + delta-debug minimizer
// ---------------------------------------------------------------------------

fn diverges(script: &str, bin: &Path, oracle: &str, timeout: Duration) -> bool {
    let o = run_prog(Path::new(oracle), script, timeout, true);
    let r = run_prog(bin, script, timeout, false);
    !o.timed_out && differs(&o, &r)
}

/// Delta-debug: greedily drop statements while the divergence survives.
fn minimize(stmts: Vec<String>, bin: &Path, oracle: &str, timeout: Duration) -> Vec<String> {
    let mut cur = stmts;
    let mut changed = true;
    while changed && cur.len() > 1 {
        changed = false;
        for i in 0..cur.len() {
            let mut cand = cur.clone();
            cand.remove(i);
            if cand.is_empty() {
                continue;
            }
            if diverges(&build_program(&cand), bin, oracle, timeout) {
                cur = cand;
                changed = true;
                break;
            }
        }
    }
    cur
}

/// Normalize a minimal reproducer to a stable gap-class signature: mask numeric
/// literals and quoted words so many instances of the same gap collapse to one
/// signature. Used by `--baseline` so known gaps don't fail CI but new ones do.
fn signature(program: &str) -> String {
    let body = program
        .lines()
        .map(|l| l.trim())
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    mask_words(&mask_numbers(&body))
}

/// Replace every quoted string literal ('...' or "...") with `W`.
fn mask_words(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            i += 1; // closing quote
            out.push('W');
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Replace every run of digits (with an optional leading `-` and a `.` fraction)
/// with `N`.
fn mask_numbers(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev_alnum = out
            .chars()
            .last()
            .map(|p| p.is_alphanumeric() || p == '_')
            .unwrap_or(false);
        if c.is_ascii_digit() && !prev_alnum {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            out.push('N');
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Args {
    count: u64,
    base_seed: u64,
    once: bool,
    timeout_ms: u64,
    out_path: PathBuf,
    max_report: usize,
    jobs: usize,
    mode: Mode,
    verify: usize,
    baseline: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut count = 2000u64;
    let mut base_seed = 1u64;
    let mut once = false;
    let mut timeout_ms = 5000u64;
    let mut max_report = 200usize;
    let mut mode = Mode::Mixed;
    let mut verify = 1usize;
    let mut baseline: Option<PathBuf> = None;
    let mut jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("parity-fuzz")
        .join("divergences.txt");

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--count" | "-c" => {
                i += 1;
                count = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or(count);
            }
            "--seed" | "-s" => {
                i += 1;
                base_seed = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(base_seed);
            }
            "--once" => once = true,
            "--timeout-ms" => {
                i += 1;
                timeout_ms = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(timeout_ms);
            }
            "--out" | "-o" => {
                i += 1;
                if let Some(p) = argv.get(i) {
                    out_path = PathBuf::from(p);
                }
            }
            "--max-report" => {
                i += 1;
                max_report = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(max_report);
            }
            "--jobs" | "-j" => {
                i += 1;
                jobs = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|&j| j >= 1)
                    .unwrap_or(jobs);
            }
            "--mode" | "-m" => {
                i += 1;
                match argv.get(i).and_then(|s| mode_from_name(s)) {
                    Some(m) => mode = m,
                    None => {
                        eprintln!(
                            "unknown --mode '{}'",
                            argv.get(i).map(|s| s.as_str()).unwrap_or("")
                        );
                        std::process::exit(2);
                    }
                }
            }
            a if a.starts_with("--") && mode_from_name(&a[2..]).is_some() => {
                mode = mode_from_name(&a[2..]).unwrap();
            }
            "--verify" => {
                i += 1;
                verify = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|&k| k >= 1)
                    .unwrap_or(verify);
            }
            "--baseline" => {
                i += 1;
                baseline = argv.get(i).map(PathBuf::from);
            }
            "--stderr" => {
                CMP_STDERR.store(true, Ordering::Relaxed);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }
    Args {
        count,
        base_seed,
        once,
        timeout_ms,
        out_path,
        max_report,
        jobs,
        mode,
        verify,
        baseline,
    }
}

fn print_help() {
    eprintln!(
        "parity-fuzz — differential python3/pythonrs parity fuzzer\n\
         \n\
         --count N        number of cases (default 2000)\n\
         --seed N         base seed; case i uses seed+i (default 1)\n\
         --mode M         mixed (default; rotates all modes), arith, bignum,\n\
         floatfmt, strings, fstring, slice, listcomp, dictcomp,\n\
         setcomp, sorting, formatspec, boolint, ranges, strmeth,\n\
         comparison, builtins, ternary, augassign\n\
         (each also accepted as a `--<mode>` shorthand)\n\
         --stderr         also require the normalized error line to match\n\
         --once           run a single case (seed) and print both outputs\n\
         --timeout-ms N   per-process wall-clock timeout (default 5000)\n\
         --out PATH       divergence corpus file\n\
         --max-report N   stop after N divergences (default 200)\n\
         --jobs N         parallel workers (default = CPU count)\n\
         --verify K       require K consecutive divergences to report (default 1)\n\
         --baseline FILE  allowlist of known-gap signatures; only a NEW\n\
         divergence (not in FILE) fails the run (exit 1)\n\
         \n\
         env  PYTHONRS_FUZZ_PYTHON=PATH  the reference CPython to compare against\n\
         (HARD ERROR if set but unusable). Every run prints the oracle it used."
    );
}

fn main() {
    let args = parse_args();
    let bin = ours_bin();
    let oracle = resolve_oracle();
    let timeout = Duration::from_millis(args.timeout_ms);

    if !bin.exists() {
        eprintln!(
            "pythonrs `python` binary not found at {}; run `cargo build` first",
            bin.display()
        );
        std::process::exit(2);
    }

    // --once: replay a single seed, minimize if it diverges, dump both sides.
    if args.once {
        let stmts = gen_case(args.base_seed, args.mode);
        let script = build_program(&stmts);
        let o = run_prog(Path::new(&oracle), &script, timeout, true);
        let r = run_prog(&bin, &script, timeout, false);
        let diverged = !o.timed_out && differs(&o, &r);
        println!("seed   : {}", args.base_seed);
        println!("mode   : {}", mode_name(args.mode));
        let (show, o, r) = if diverged && stmts.len() > 1 {
            let m = minimize(stmts, &bin, &oracle, timeout);
            let ms = build_program(&m);
            let mo = run_prog(Path::new(&oracle), &ms, timeout, true);
            let mr = run_prog(&bin, &ms, timeout, false);
            (ms, mo, mr)
        } else {
            (script, o, r)
        };
        println!("program:\n  {}", show.replace('\n', "\n  "));
        println!("--- python3 exit={} timeout={} ---", o.exit, o.timed_out);
        let _ = std::io::stdout().write_all(&o.stdout);
        println!("--- pythonrs exit={} timeout={} ---", r.exit, r.timed_out);
        let _ = std::io::stdout().write_all(&r.stdout);
        println!("--- {} ---", if diverged { "DIVERGE" } else { "match" });
        std::process::exit(if diverged { 1 } else { 0 });
    }

    let next = AtomicU64::new(0);
    let checked = AtomicU64::new(0);
    let timeouts = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let divergences: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    let start = Instant::now();

    eprintln!("oracle: {}", oracle_id(&oracle));
    eprintln!("ours  : {}", bin.display());
    eprintln!(
        "fuzzing {} cases ({}) across {} workers…",
        args.count,
        mode_name(args.mode),
        args.jobs
    );

    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= args.count {
                    break;
                }
                let seed = args.base_seed.wrapping_add(idx);
                let stmts = gen_case(seed, args.mode);
                let script = build_program(&stmts);
                let o = run_prog(Path::new(&oracle), &script, timeout, true);
                let r = run_prog(&bin, &script, timeout, false);
                let done = checked.fetch_add(1, Ordering::Relaxed) + 1;
                if o.timed_out || r.timed_out {
                    timeouts.fetch_add(1, Ordering::Relaxed);
                }
                // oracle-side timeout ⇒ pathological case; not a parity gap.
                if !o.timed_out && differs(&o, &r) {
                    let minimal = minimize(stmts, &bin, &oracle, timeout);
                    let mscript = build_program(&minimal);
                    let mo = run_prog(Path::new(&oracle), &mscript, timeout, true);
                    let mr = run_prog(&bin, &mscript, timeout, false);
                    // Re-verify: a REAL gap diverges every time; a transient
                    // (empty output under resource pressure) won't reproduce.
                    let mut confirmed = differs(&mo, &mr);
                    for _ in 1..args.verify.max(1) {
                        if !confirmed {
                            break;
                        }
                        confirmed = diverges(&mscript, &bin, &oracle, timeout);
                    }
                    if !confirmed {
                        continue;
                    }
                    let err_of = |o: &RunOut| -> String {
                        if CMP_STDERR.load(Ordering::Relaxed) {
                            format!(
                                "\n  stderr: {}",
                                render(&norm_stderr(&o.stderr)).replace('\n', "\n  ")
                            )
                        } else {
                            String::new()
                        }
                    };
                    let rec = format!(
                        "==== seed {seed} ====\n\
                         program:\n  {}\n\
                         python3  : exit={} timeout={}{}\n{}\n\
                         pythonrs : exit={} timeout={}{}\n{}\n",
                        mscript.replace('\n', "\n  "),
                        mo.exit,
                        mo.timed_out,
                        err_of(&mo),
                        render(&mo.stdout),
                        mr.exit,
                        mr.timed_out,
                        err_of(&mr),
                        render(&mr.stdout),
                    );
                    let mut d = divergences.lock().unwrap();
                    d.push((seed, rec));
                    if d.len() >= args.max_report {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                if done % 500 == 0 {
                    let n = divergences.lock().unwrap().len();
                    eprintln!(
                        "  {done}/{} checked, {n} divergences, {:.0}/s",
                        args.count,
                        done as f64 / start.elapsed().as_secs_f64().max(0.001)
                    );
                }
            });
        }
    });

    let checked = checked.load(Ordering::Relaxed);
    let timeouts = timeouts.load(Ordering::Relaxed);
    let mut divergences: Vec<(u64, String)> = divergences.into_inner().unwrap();
    divergences.sort_by_key(|(seed, _)| *seed);
    let divergences: Vec<String> = divergences.into_iter().map(|(_, r)| r).collect();
    let elapsed = start.elapsed();

    let sig_of = |rec: &str| -> String {
        let prog = rec
            .split("program:\n")
            .nth(1)
            .and_then(|s| s.split("\npython3").next())
            .unwrap_or(rec);
        signature(prog)
    };

    let allowed: std::collections::HashSet<String> = match &args.baseline {
        Some(bp) => std::fs::read_to_string(bp)
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect(),
        None => std::collections::HashSet::new(),
    };
    let mut new_records: Vec<&String> = Vec::new();
    let mut new_sigs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut known = 0usize;
    for rec in &divergences {
        let sig = sig_of(rec);
        if args.baseline.is_some() && allowed.contains(&sig) {
            known += 1;
        } else {
            new_records.push(rec);
            new_sigs.insert(sig);
        }
    }

    let oracle = oracle_id(&oracle);
    println!(
        "\nfuzzed {checked} cases in {:.1}s ({:.0}/s)\n\
         oracle      : {}\n\
         divergences : {} ({} known / {} new)\n\
         timeouts    : {}",
        elapsed.as_secs_f64(),
        checked as f64 / elapsed.as_secs_f64().max(0.001),
        oracle,
        divergences.len(),
        known,
        new_records.len(),
        timeouts,
    );

    if !divergences.is_empty() {
        if let Some(parent) = args.out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&args.out_path) {
            let _ = writeln!(f, "# oracle: {oracle}");
            for d in &divergences {
                let _ = writeln!(f, "{d}");
            }
            println!(
                "wrote {} divergences to {}",
                divergences.len(),
                args.out_path.display()
            );
        }
    }

    if !new_records.is_empty() {
        println!(
            "\n--- {} NEW gap signature(s) (add to baseline once triaged) ---",
            new_sigs.len()
        );
        for s in &new_sigs {
            println!("{s}");
        }
        println!(
            "\n--- first {} new divergence record(s) ---",
            new_records.len().min(5)
        );
        for d in new_records.iter().take(5) {
            println!("{d}");
        }
        std::process::exit(1);
    }
    if known > 0 {
        println!("all {known} divergences are known (in baseline) — OK");
    }
}

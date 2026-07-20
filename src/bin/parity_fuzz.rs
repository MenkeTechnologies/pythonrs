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

/// Augmented-assignment in-place semantics, `with`-statement `__exit__`
/// suppression + real exception triple, and chained-comparison single evaluation
/// of the interior operand (with short-circuit). Every printed form is
/// deterministic; identity is probed with `is`, and any set is `sorted` so the
/// output is order-stable across CPython and pythonrs.
fn gen_augwith(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    match r.below(6) {
        // in-place dunder (identity preserved) vs binary fallback (rebinds).
        0 => {
            let n = pick(r, POSINTS);
            if r.below(2) == 0 {
                vec![
                    "class C:".into(),
                    "    def __init__(s): s.v = 0".into(),
                    "    def __iadd__(s, o): s.v += o; return s".into(),
                    "c = C()".into(),
                    "d = c".into(),
                    format!("c += {n}"),
                    "print(d is c, c.v)".into(),
                ]
            } else {
                vec![
                    "class A:".into(),
                    "    def __init__(s, x): s.x = x".into(),
                    "    def __add__(s, o): return A(s.x + o)".into(),
                    format!("a = A({n})"),
                    "b = a".into(),
                    "a += 1".into(),
                    "print(b is a, a.x)".into(),
                ]
            }
        }
        // list / bytearray in-place mutation (identity preserved).
        1 => {
            let a = pick(r, POSINTS);
            let b = pick(r, POSINTS);
            let k = pick(r, &["1", "2", "3"]);
            match r.below(4) {
                0 => vec![
                    format!("l = [{a}]"),
                    "m = l".into(),
                    format!("l += [{b}]"),
                    "print(m is l, l)".into(),
                ],
                1 => vec![
                    format!("l = [{a}, {b}]"),
                    "m = l".into(),
                    format!("l *= {k}"),
                    "print(m is l, l)".into(),
                ],
                2 => vec![
                    "l = [1]".into(),
                    "l += (x for x in range(3))".into(),
                    "print(l)".into(),
                ],
                _ => vec![
                    "b = bytearray(b'ab')".into(),
                    "m = b".into(),
                    "b += b'cd'".into(),
                    "print(m is b, b)".into(),
                ],
            }
        }
        // set / dict in-place algebra (identity preserved; sets sorted).
        2 => {
            let op = pick(r, &["|=", "&=", "-=", "^="]);
            if r.below(2) == 0 {
                vec![
                    "s = {1, 2, 3}".into(),
                    "m = s".into(),
                    format!("s {op} {{2, 3, 4}}"),
                    "print(m is s, sorted(s))".into(),
                ]
            } else {
                vec![
                    "d = {'a': 1}".into(),
                    "m = d".into(),
                    "d |= {'b': 2}".into(),
                    "print(m is d, d)".into(),
                ]
            }
        }
        // immutable rebind: int / str / tuple never mutate in place.
        3 => {
            let a = pick(r, INTS);
            let b = pick(r, POSINTS);
            match r.below(3) {
                0 => vec![format!("x = {a}"), format!("x += {b}"), "print(x)".into()],
                1 => vec![
                    "s = 'a'".into(),
                    "s += 'b'".into(),
                    "print(s)".into(),
                ],
                _ => vec![
                    "t = (1,)".into(),
                    "u = t".into(),
                    "t += (2,)".into(),
                    "print(u is t, t)".into(),
                ],
            }
        }
        // chained comparison: interior operand evaluated EXACTLY once; a call in
        // last position exposes short-circuit (n stays 0 when an earlier link fails).
        4 => {
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            let c = pick(r, SMALLINTS);
            let mut out: Vec<String> = vec![
                "n = 0".into(),
                "def f(v):".into(),
                "    global n".into(),
                "    n += 1".into(),
                "    return v".into(),
            ];
            if r.below(2) == 0 {
                // interior call: always evaluated once regardless of outcome.
                out.push(format!("print({a} < f({b}) < {c}, n)"));
            } else {
                // trailing call: reached only if the first link holds.
                out.push(format!("print({a} < {b} < f({c}), n)"));
            }
            out
        }
        // with-statement: real triple to __exit__ and truthy-return suppression.
        _ => {
            let sup = if r.below(2) == 0 { "True" } else { "False" };
            if r.below(2) == 0 {
                vec![
                    "class CM:".into(),
                    "    def __enter__(s): return s".into(),
                    format!("    def __exit__(s, t, v, tb): return {sup}"),
                    "r = []".into(),
                    "try:".into(),
                    "    with CM():".into(),
                    "        r.append(1)".into(),
                    "        raise ValueError('x')".into(),
                    "    r.append(2)".into(),
                    "except ValueError:".into(),
                    "    r.append(3)".into(),
                    "print(r)".into(),
                ]
            } else {
                vec![
                    "seen = []".into(),
                    "class CM:".into(),
                    "    def __enter__(s): return 7".into(),
                    "    def __exit__(s, t, v, tb):".into(),
                    "        seen.append((t is ValueError, str(v)))".into(),
                    "        return True".into(),
                    "with CM() as x:".into(),
                    "    raise ValueError('boom')".into(),
                    "print(x, seen)".into(),
                ]
            }
        }
    }
}

const SMALLINTS: &[&str] = &[
    "0", "1", "2", "3", "4", "5", "6", "7", "-1", "-2", "-3", "10",
];

/// A user class with a rich dunder set: operator overloading, comparison,
/// `__repr__`, `__neg__`/`__abs__`, `__len__`/`__bool__`, `__format__`. Every
/// printed form is deterministic (always via `__repr__` or a scalar).
fn gen_classes(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    let b = pick(r, SMALLINTS);
    let c = pick(r, SMALLINTS);
    let vdef: Vec<String> = vec![
        "class V:".into(),
        "    def __init__(self, x): self.x = x".into(),
        "    def __repr__(self): return 'V(' + str(self.x) + ')'".into(),
        "    def __eq__(self, o): return self.x == o.x".into(),
        "    def __lt__(self, o): return self.x < o.x".into(),
        "    def __add__(self, o): return V(self.x + o.x)".into(),
        "    def __sub__(self, o): return V(self.x - o.x)".into(),
        "    def __mul__(self, k): return V(self.x * k)".into(),
        "    def __neg__(self): return V(-self.x)".into(),
        "    def __abs__(self): return V(abs(self.x))".into(),
        "    def __len__(self): return abs(self.x)".into(),
        "    def __bool__(self): return self.x != 0".into(),
        "    def __format__(self, s): return format(self.x, s)".into(),
    ];
    let mut out = vdef;
    match r.below(8) {
        0 => out.push(format!("print(V({a}) + V({b}) - V({c}))")),
        1 => out.push(format!("print(V({a}) == V({b}), V({a}) < V({b}))")),
        2 => out.push(format!("print(sorted([V({a}), V({b}), V({c})]))")),
        3 => out.push(format!("print(-V({a}), abs(V({b})), V({a}) * 3)")),
        4 => out.push(format!("print(bool(V({a})), len(V({b})))")),
        5 => out.push(format!("print(V({a}) != V({b}), V({a}) != V({a}))")),
        6 => out.push(format!(
            "print('{{:+d}}'.format(V({a})), format(V({b}), '03d'))"
        )),
        _ => {
            // property + inheritance/super.
            out.push("class C(V):".into());
            out.push("    @property".into());
            out.push("    def doubled(self): return self.x * 2".into());
            out.push("    def __add__(self, o): return C(super().__add__(o).x + 1)".into());
            out.push(format!("c = C({a})"));
            out.push(format!("print(c.doubled, (c + V({b})).x)"));
        }
    }
    out
}

/// `match`/`case` structural pattern matching (PEP 634): literal / capture /
/// wildcard / dotted-value patterns, sequence (with `*rest`), mapping (with
/// `**rest`), class patterns (positional via `__match_args__` and keyword),
/// OR / AS patterns, guards, nesting, singleton identity (`None`/`True`/`False`
/// matched by `is`, so `0` does NOT match `case False`), plus the compile-time
/// rejections (duplicate capture / mapping key / class-keyword, OR alternatives
/// binding different names) and runtime rejection (positional overflow). Every
/// branch prints a deterministic scalar or captured value — never an object
/// repr with an address — so output is byte-stable across both interpreters.
fn gen_match(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let mut out: Vec<String> = Vec::new();
    match r.below(18) {
        0 => {
            // Literal patterns + OR of literals + irrefutable capture tail.
            let s = pick(r, &["0", "1", "2", "3", "-1", "7", "99"]);
            out.push(format!("match {s}:"));
            out.push("    case 0:".into());
            out.push("        print('zero')".into());
            out.push("    case 1 | 2 | 3:".into());
            out.push("        print('small')".into());
            out.push("    case -1:".into());
            out.push("        print('neg')".into());
            out.push("    case x:".into());
            out.push("        print('other', x)".into());
        }
        1 => {
            // Singleton identity: None/True/False by `is`, ints by `==`.
            let s = pick(r, &["0", "1", "None", "True", "False", "2", "3"]);
            out.push(format!("match {s}:"));
            out.push("    case None:".into());
            out.push("        print('none')".into());
            out.push("    case True:".into());
            out.push("        print('true')".into());
            out.push("    case False:".into());
            out.push("        print('false')".into());
            out.push("    case 0:".into());
            out.push("        print('int0')".into());
            out.push("    case 1:".into());
            out.push("        print('int1')".into());
            out.push("    case _:".into());
            out.push("        print('other')".into());
        }
        2 => {
            // Sequence patterns with a star.
            let n = r.below(5);
            let items: Vec<String> = (0..n).map(|i| ((i * 3 + 1) % 10).to_string()).collect();
            out.push(format!("match [{}]:", items.join(", ")));
            out.push("    case []:".into());
            out.push("        print('empty')".into());
            out.push("    case [a]:".into());
            out.push("        print('one', a)".into());
            out.push("    case [a, b]:".into());
            out.push("        print('two', a, b)".into());
            out.push("    case [a, *rest]:".into());
            out.push("        print('many', a, rest)".into());
        }
        3 => {
            // Tuple / parenthesized sequence patterns.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push(format!("match ({a}, {b}):"));
            out.push("    case (0, 0):".into());
            out.push("        print('origin')".into());
            out.push("    case (0, y):".into());
            out.push("        print('yaxis', y)".into());
            out.push("    case (x, 0):".into());
            out.push("        print('xaxis', x)".into());
            out.push("    case (x, y):".into());
            out.push("        print('pt', x, y)".into());
        }
        4 => {
            // Mapping pattern with `**rest`.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push(format!("match {{'x': {a}, 'y': {b}}}:"));
            out.push("    case {'x': 0}:".into());
            out.push("        print('x0')".into());
            out.push("    case {'x': xv, **rest}:".into());
            out.push("        print('x', xv, rest)".into());
            out.push("    case _:".into());
            out.push("        print('none')".into());
        }
        5 => {
            // Class pattern, positional via __match_args__.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push("class Point:".into());
            out.push("    __match_args__ = ('x', 'y')".into());
            out.push("    def __init__(self, x, y):".into());
            out.push("        self.x = x".into());
            out.push("        self.y = y".into());
            out.push(format!("match Point({a}, {b}):"));
            out.push("    case Point(0, 0):".into());
            out.push("        print('origin')".into());
            out.push("    case Point(x, 0):".into());
            out.push("        print('xaxis', x)".into());
            out.push("    case Point(0, y):".into());
            out.push("        print('yaxis', y)".into());
            out.push("    case Point(x, y):".into());
            out.push("        print('pt', x, y)".into());
        }
        6 => {
            // Class pattern, keyword sub-patterns + guard.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push("class P:".into());
            out.push("    __match_args__ = ('x', 'y')".into());
            out.push("    def __init__(self, x, y):".into());
            out.push("        self.x = x".into());
            out.push("        self.y = y".into());
            out.push(format!("match P({a}, {b}):"));
            out.push("    case P(x=0, y=yv):".into());
            out.push("        print('x0', yv)".into());
            out.push("    case P(x=xv, y=yv) if xv > yv:".into());
            out.push("        print('gt', xv, yv)".into());
            out.push("    case P(x=xv, y=yv):".into());
            out.push("        print('pt', xv, yv)".into());
        }
        7 => {
            // OR patterns (same-name / no-name alternatives) + AS.
            let s = pick(r, &["1", "4", "7", "0", "5", "9", "2"]);
            out.push(format!("match {s}:"));
            out.push("    case 1 | 2 | 3:".into());
            out.push("        print('low')".into());
            out.push("    case 4 | 5 | 6:".into());
            out.push("        print('mid')".into());
            out.push("    case (7 | 8 | 9) as n:".into());
            out.push("        print('high', n)".into());
            out.push("    case _:".into());
            out.push("        print('other')".into());
        }
        8 => {
            // AS pattern + guard referencing captures.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push(format!("match [{a}, {b}]:"));
            out.push("    case [x, y] if x == y:".into());
            out.push("        print('eq', x)".into());
            out.push("    case [x, y] as pair:".into());
            out.push("        print('pair', x, y, len(pair))".into());
        }
        9 => {
            // Nested sequence patterns with captures at depth.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            let c = pick(r, SMALLINTS);
            out.push(format!("match [{a}, [{b}, {c}]]:"));
            out.push("    case [0, [y, z]]:".into());
            out.push("        print('zero', y, z)".into());
            out.push("    case [x, [0, z]]:".into());
            out.push("        print('midzero', x, z)".into());
            out.push("    case [x, [y, z]]:".into());
            out.push("        print('nested', x, y, z)".into());
        }
        10 => {
            // Dotted value patterns (enum-like class attributes).
            let v = pick(r, &["0", "1", "2", "3"]);
            out.push("class Color:".into());
            out.push("    RED = 0".into());
            out.push("    GREEN = 1".into());
            out.push("    BLUE = 2".into());
            out.push(format!("match {v}:"));
            out.push("    case Color.RED:".into());
            out.push("        print('red')".into());
            out.push("    case Color.GREEN:".into());
            out.push("        print('green')".into());
            out.push("    case Color.BLUE:".into());
            out.push("        print('blue')".into());
            out.push("    case _:".into());
            out.push("        print('unknown')".into());
        }
        11 => {
            // Mapping-of-sequence nesting.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push(format!("match {{'items': [{a}, {b}]}}:"));
            out.push("    case {'items': [0, y]}:".into());
            out.push("        print('head0', y)".into());
            out.push("    case {'items': [x, y]}:".into());
            out.push("        print('two', x, y)".into());
            out.push("    case _:".into());
            out.push("        print('no')".into());
        }
        12 => {
            // Builtin class patterns (int/str self-match) across subject types.
            let subj = if r.below(2) == 0 {
                pick(r, SMALLINTS).to_string()
            } else {
                "'hi'".to_string()
            };
            out.push(format!("match {subj}:"));
            out.push("    case int(v) if v > 0:".into());
            out.push("        print('pos', v)".into());
            out.push("    case int(v):".into());
            out.push("        print('int', v)".into());
            out.push("    case str(s):".into());
            out.push("        print('str', s)".into());
            out.push("    case _:".into());
            out.push("        print('other')".into());
        }
        13 => {
            // Runtime rejection: positional overflow -> TypeError (both reject).
            out.push("class P:".into());
            out.push("    __match_args__ = ('x',)".into());
            out.push("    def __init__(self):".into());
            out.push("        self.x = 1".into());
            out.push("match P():".into());
            out.push("    case P(a, b):".into());
            out.push("        print(a, b)".into());
        }
        14 => {
            // Compile-time rejection: duplicate capture -> SyntaxError.
            let a = pick(r, SMALLINTS);
            let b = pick(r, SMALLINTS);
            out.push(format!("match [{a}, {b}]:"));
            out.push("    case [x, x]:".into());
            out.push("        print(x)".into());
        }
        15 => {
            // Compile-time rejection: duplicate mapping key -> SyntaxError.
            let a = pick(r, SMALLINTS);
            out.push(format!("match {{'a': {a}}}:"));
            out.push("    case {'a': x, 'a': y}:".into());
            out.push("        print(x, y)".into());
        }
        16 => {
            // Compile-time rejection: repeated class-keyword -> SyntaxError.
            out.push("class P:".into());
            out.push("    def __init__(self):".into());
            out.push("        self.x = 1".into());
            out.push("match P():".into());
            out.push("    case P(x=a, x=b):".into());
            out.push("        print(a, b)".into());
        }
        _ => {
            // Compile-time rejection: OR alternatives bind different names.
            let s = pick(r, SMALLINTS);
            out.push(format!("match {s}:"));
            out.push("    case [x] | y:".into());
            out.push("        print('bad')".into());
            out.push("    case _:".into());
            out.push("        print('ok')".into());
        }
    }
    out
}

/// async/await/asyncio: coroutine result, `gather` ordering, `create_task`
/// interleaving, `Future` set_result, `async for`/`async with`, async
/// comprehensions — all driven by `asyncio.run` on the native event loop.
fn gen_async(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 2 + r.below(4); // 2..=5 items
    let mut out: Vec<String> = vec!["import asyncio".into()];
    match r.below(7) {
        6 => {
            // async generator consumed by an async comprehension.
            out.push("async def ag(n):".into());
            out.push("    for i in range(n):".into());
            out.push("        await asyncio.sleep(0)".into());
            out.push("        if i % 2 == 0:".into());
            out.push("            yield i * i".into());
            out.push("async def main():".into());
            out.push(format!("    return [x async for x in ag({})]", n + 2));
            out.push("print(asyncio.run(main()))".into());
        }
        0 => {
            // gather of N coros returning i*i, in order.
            out.push("async def sq(i):".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    return i * i".into());
            out.push("async def main():".into());
            let calls: Vec<String> = (0..n).map(|i| format!("sq({i})")).collect();
            out.push(format!(
                "    return await asyncio.gather({})",
                calls.join(", ")
            ));
            out.push("print(asyncio.run(main()))".into());
        }
        1 => {
            // create_task fan-out with sleep(0) interleaving; ordered prints.
            out.push("async def w(i):".into());
            out.push("    print('s', i)".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    print('e', i)".into());
            out.push("async def main():".into());
            out.push(format!(
                "    ts = [asyncio.create_task(w(i)) for i in range({n})]"
            ));
            out.push("    for t in ts:".into());
            out.push("        await t".into());
            out.push("asyncio.run(main())".into());
        }
        2 => {
            // async for over a custom async iterator.
            out.push("class R:".into());
            out.push("    def __init__(self, n):".into());
            out.push("        self.n = n".into());
            out.push("        self.i = 0".into());
            out.push("    def __aiter__(self):".into());
            out.push("        return self".into());
            out.push("    async def __anext__(self):".into());
            out.push("        if self.i >= self.n:".into());
            out.push("            raise StopAsyncIteration".into());
            out.push("        self.i += 1".into());
            out.push("        await asyncio.sleep(0)".into());
            out.push("        return self.i".into());
            out.push("async def main():".into());
            out.push("    acc = []".into());
            out.push(format!("    async for x in R({n}):"));
            out.push("        acc.append(x)".into());
            out.push("    return acc".into());
            out.push("print(asyncio.run(main()))".into());
        }
        3 => {
            // Future set_result awaited across a task.
            out.push("async def setter(fut, v):".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    fut.set_result(v)".into());
            out.push("async def main():".into());
            out.push("    fut = asyncio.Future()".into());
            out.push(format!("    asyncio.create_task(setter(fut, {n}))"));
            out.push("    return await fut".into());
            out.push("print(asyncio.run(main()))".into());
        }
        4 => {
            // Nested await chain + async comprehension.
            out.push("async def inner(i):".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    return i + 1".into());
            out.push("class R:".into());
            out.push("    def __init__(self, n):".into());
            out.push("        self.n = n".into());
            out.push("        self.i = 0".into());
            out.push("    def __aiter__(self):".into());
            out.push("        return self".into());
            out.push("    async def __anext__(self):".into());
            out.push("        if self.i >= self.n:".into());
            out.push("            raise StopAsyncIteration".into());
            out.push("        self.i += 1".into());
            out.push("        return self.i".into());
            out.push("async def main():".into());
            out.push(format!("    return [await inner(x) async for x in R({n})]"));
            out.push("print(asyncio.run(main()))".into());
        }
        _ => {
            // async with a custom async context manager.
            out.push("class CM:".into());
            out.push("    def __init__(self, v):".into());
            out.push("        self.v = v".into());
            out.push("    async def __aenter__(self):".into());
            out.push("        return self.v".into());
            out.push("    async def __aexit__(self, *a):".into());
            out.push("        return False".into());
            out.push("async def main():".into());
            out.push(format!("    async with CM({n}) as v:"));
            out.push("        return v * 2".into());
            out.push("print(asyncio.run(main()))".into());
        }
    }
    out
}

/// async round 2: `Task.cancel()` → `CancelledError` injection (caught /
/// propagated), async-generator `asend`/`athrow`/`aclose`, `wait_for` timeout /
/// success, bounded-`Queue` back-pressure, and `wait(return_when=…)`. Every case
/// drives all coroutines (no un-awaited leaks) and prints only order-stable
/// values (counts / result lists), never set `repr` or object identities.
fn gen_async2(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 1 + r.below(4); // 1..=4
    let mut out: Vec<String> = vec!["import asyncio".into()];
    match r.below(9) {
        0 => {
            // Task.cancel() caught inside the coroutine → returns normally.
            out.push("async def worker():".into());
            out.push("    try:".into());
            out.push("        await asyncio.sleep(10)".into());
            out.push("        return 'no'".into());
            out.push("    except asyncio.CancelledError:".into());
            out.push("        return 'caught'".into());
            out.push("async def main():".into());
            out.push("    t = asyncio.create_task(worker())".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    c = t.cancel()".into());
            out.push("    r = await t".into());
            out.push("    print(c, r, t.cancelled())".into());
            out.push("asyncio.run(main())".into());
        }
        1 => {
            // Task.cancel() propagates → awaiting the task raises CancelledError.
            out.push("async def worker():".into());
            out.push("    await asyncio.sleep(10)".into());
            out.push("    return 'no'".into());
            out.push("async def main():".into());
            out.push("    t = asyncio.create_task(worker())".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    t.cancel()".into());
            out.push("    try:".into());
            out.push("        await t".into());
            out.push("        print('no-raise')".into());
            out.push("    except asyncio.CancelledError:".into());
            out.push("        print('cancelled', t.cancelled())".into());
            out.push("asyncio.run(main())".into());
        }
        2 => {
            // Async generator `asend` round-trip: each send is echoed back.
            out.push("async def ag(k):".into());
            out.push("    for i in range(k):".into());
            out.push("        await asyncio.sleep(0)".into());
            out.push("        yield i".into());
            out.push("async def main():".into());
            out.push(format!("    g = ag({n})"));
            out.push("    acc = []".into());
            out.push("    try:".into());
            out.push("        v = await g.asend(None)".into());
            out.push("        while True:".into());
            out.push("            acc.append(v)".into());
            out.push("            v = await g.asend(v)".into());
            out.push("    except StopAsyncIteration:".into());
            out.push("        pass".into());
            out.push("    print(acc)".into());
            out.push("asyncio.run(main())".into());
        }
        3 => {
            // Async generator `athrow`: the body catches and yields once more.
            out.push("async def ag():".into());
            out.push("    try:".into());
            out.push("        while True:".into());
            out.push("            yield 1".into());
            out.push("    except ValueError:".into());
            out.push("        yield 2".into());
            out.push("async def main():".into());
            out.push("    g = ag()".into());
            out.push("    a = await g.asend(None)".into());
            out.push("    b = await g.athrow(ValueError)".into());
            out.push("    await g.aclose()".into());
            out.push("    print(a, b)".into());
            out.push("asyncio.run(main())".into());
        }
        4 => {
            // Async generator `aclose`: GeneratorExit finishes the body.
            out.push("async def ag():".into());
            out.push("    try:".into());
            out.push("        yield 1".into());
            out.push("        yield 2".into());
            out.push("    finally:".into());
            out.push("        pass".into());
            out.push("async def main():".into());
            out.push("    g = ag()".into());
            out.push("    print(await g.asend(None))".into());
            out.push("    await g.aclose()".into());
            out.push("    print('closed')".into());
            out.push("asyncio.run(main())".into());
        }
        5 => {
            // wait_for timeout → TimeoutError (inner task cancelled).
            out.push("async def slow():".into());
            out.push("    await asyncio.sleep(10)".into());
            out.push("    return 1".into());
            out.push("async def main():".into());
            out.push("    try:".into());
            out.push("        await asyncio.wait_for(slow(), timeout=1)".into());
            out.push("        print('no')".into());
            out.push("    except asyncio.TimeoutError:".into());
            out.push("        print('timeout')".into());
            out.push("asyncio.run(main())".into());
        }
        6 => {
            // wait_for success within the deadline.
            out.push("async def fast(v):".into());
            out.push("    await asyncio.sleep(0)".into());
            out.push("    return v".into());
            out.push("async def main():".into());
            out.push(format!(
                "    r = await asyncio.wait_for(fast({n}), timeout=5)"
            ));
            out.push("    print(r)".into());
            out.push("asyncio.run(main())".into());
        }
        7 => {
            // Bounded Queue: producer blocks on a full queue until the consumer
            // drains it; the consumed order is deterministic.
            out.push("async def main():".into());
            out.push("    q = asyncio.Queue(2)".into());
            out.push("    async def prod():".into());
            out.push(format!("        for i in range({}):", n + 3));
            out.push("            await q.put(i)".into());
            out.push("        await q.put(-1)".into());
            out.push("    async def cons():".into());
            out.push("        acc = []".into());
            out.push("        while True:".into());
            out.push("            v = await q.get()".into());
            out.push("            if v == -1:".into());
            out.push("                break".into());
            out.push("            acc.append(v)".into());
            out.push("            await asyncio.sleep(0)".into());
            out.push("        print(acc)".into());
            out.push("    await asyncio.gather(prod(), cons())".into());
            out.push("asyncio.run(main())".into());
        }
        _ => {
            // wait(return_when=FIRST_COMPLETED): one done, one pending.
            out.push("async def f(v, d):".into());
            out.push("    await asyncio.sleep(d)".into());
            out.push("    return v".into());
            out.push("async def main():".into());
            out.push("    t1 = asyncio.create_task(f(1, 3))".into());
            out.push("    t2 = asyncio.create_task(f(2, 1))".into());
            out.push(
                "    done, pending = await asyncio.wait([t1, t2], return_when=asyncio.FIRST_COMPLETED)"
                    .into(),
            );
            out.push("    print(len(done), len(pending))".into());
            out.push("    await asyncio.wait([t1, t2])".into());
            out.push("asyncio.run(main())".into());
        }
    }
    out
}

/// Custom-iterable protocol: `__getitem__`-sequence and `__iter__`/`__contains__`/
/// `__reversed__`, exercised through `for`, `list()`, `sum()`, `in`, `reversed()`.
fn gen_iterproto(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 1 + r.below(5);
    let probe = r.below(9) as i64;
    match r.below(2) {
        0 => vec![
            "class S:".into(),
            "    def __init__(self, n): self.n = n".into(),
            "    def __getitem__(self, i):".into(),
            "        if i >= self.n: raise IndexError".into(),
            "        return i * i".into(),
            format!("s = S({n})"),
            "print(list(s))".into(),
            "print(sum(s))".into(),
            format!("print({probe} in s)"),
            "print([x for x in s])".into(),
        ],
        _ => vec![
            "class R:".into(),
            "    def __init__(self, n): self.data = list(range(n))".into(),
            "    def __iter__(self): return iter(self.data)".into(),
            "    def __contains__(self, x): return x in self.data".into(),
            "    def __reversed__(self): return reversed(self.data)".into(),
            format!("r = R({n})"),
            "print(list(r))".into(),
            "print(list(reversed(r)))".into(),
            format!("print({probe} in r)"),
            "print(sorted(r, reverse=True))".into(),
        ],
    }
}

/// Generators & the yield protocol: `yield from` delegation (sent values, thrown
/// exceptions, and close forwarded into the sub-iterator per PEP 380), the
/// delegate's `return` surfacing as `StopIteration.value`, direct
/// `.send()`/`.throw()`/`.close()`/`.__next__()`, generator expressions, nested
/// `yield from` chains, `yield` as an expression, and try/finally cleanup on
/// close. Output is deterministic (ints, fixed strings, `type(e).__name__` — never
/// a generator repr with an address).
fn gen_generators(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let base = *pick(r, SMALLINTS);
    let v1 = *pick(r, SMALLINTS);
    let v2 = *pick(r, SMALLINTS);
    let v3 = *pick(r, SMALLINTS);
    let n = 2 + r.below(4); // 2..=5
    match r.below(10) {
        0 => vec![
            // `yield from` sent-value round-trip + delegate return value.
            "def sub(base):".into(),
            "    a = yield base".into(),
            "    b = yield a + 1".into(),
            "    c = yield b + 1".into(),
            "    return a + b + c".into(),
            "def deleg(base):".into(),
            "    r = yield from sub(base)".into(),
            "    print('ret', r)".into(),
            format!("g = deleg({base})"),
            "print(next(g))".into(),
            format!("print(g.send({v1}))"),
            format!("print(g.send({v2}))"),
            "try:".into(),
            format!("    g.send({v3})"),
            "except StopIteration as e:".into(),
            "    print('stop', e.value)".into(),
        ],
        1 => vec![
            // `yield from` throw forwarded into the sub, caught there, continues.
            "def sub():".into(),
            "    try:".into(),
            "        while True:".into(),
            "            x = yield".into(),
            "    except ValueError:".into(),
            "        yield 'recovered'".into(),
            format!("        return {v1}"),
            "def deleg():".into(),
            "    r = yield from sub()".into(),
            "    print('dret', r)".into(),
            "g = deleg()".into(),
            "next(g)".into(),
            format!("g.send({v2})"),
            "print(g.throw(ValueError))".into(),
            "try:".into(),
            "    next(g)".into(),
            "except StopIteration as e:".into(),
            "    print('stop', e.value)".into(),
        ],
        2 => vec![
            // `yield from` close forwarded → sub's try/finally cleanup runs.
            "def sub():".into(),
            "    try:".into(),
            "        yield 1".into(),
            "        yield 2".into(),
            "    finally:".into(),
            "        print('sub cleanup')".into(),
            "def deleg():".into(),
            "    yield from sub()".into(),
            "g = deleg()".into(),
            "print(next(g))".into(),
            "g.close()".into(),
            "print('closed')".into(),
        ],
        3 => vec![
            // Nested `yield from` chain (3 levels) with send + return threading.
            "def leaf():".into(),
            "    a = yield 10".into(),
            "    b = yield a + 1".into(),
            "    return b * 2".into(),
            "def mid():".into(),
            "    r = yield from leaf()".into(),
            format!("    return r + {v1}"),
            "def top():".into(),
            "    r = yield from mid()".into(),
            "    print('top', r)".into(),
            "g = top()".into(),
            "print(next(g))".into(),
            format!("print(g.send({v2}))"),
            "try:".into(),
            format!("    g.send({v3})"),
            "except StopIteration as e:".into(),
            "    print('stop', e.value)".into(),
        ],
        4 => vec![
            // Direct .send()/.throw()/.close() protocol on a plain generator.
            "def g():".into(),
            "    total = 0".into(),
            "    try:".into(),
            "        while True:".into(),
            "            x = yield total".into(),
            "            total += x".into(),
            "    except ValueError:".into(),
            "        yield -1".into(),
            "gen = g()".into(),
            "print(next(gen))".into(),
            format!("print(gen.send({v1}))"),
            format!("print(gen.send({v2}))"),
            "print(gen.throw(ValueError))".into(),
            "gen.close()".into(),
            "print('closed')".into(),
        ],
        5 => vec![
            // `return` inside a generator surfaces as StopIteration.value.
            "def g(n):".into(),
            "    for i in range(n):".into(),
            "        yield i".into(),
            format!("    return n * {v1}"),
            format!("gen = g({n})"),
            "acc = []".into(),
            "try:".into(),
            "    while True:".into(),
            "        acc.append(next(gen))".into(),
            "except StopIteration as e:".into(),
            "    print(acc, e.value)".into(),
        ],
        6 => vec![
            // Generator expressions: lazy, drained by list()/sum()/next().
            format!("gen = (x * x for x in range({n}))"),
            "print(next(gen))".into(),
            "print(list(gen))".into(),
            format!("print(sum(y + {v1} for y in range({n})))"),
            format!("print(list(z for z in range({n}) if z % 2 == 0))"),
        ],
        7 => vec![
            // `yield from` over plain iterables interleaved with own yields.
            "def g():".into(),
            "    yield -1".into(),
            format!("    yield from [{v1}, {v2}]"),
            format!("    yield from range({n})"),
            "    yield from 'ab'".into(),
            format!("    r = yield from (i for i in range({v3}))"),
            "    print('genexp done', r)".into(),
            "print(list(g()))".into(),
        ],
        8 => vec![
            // `yield` as an expression: a send-driven running accumulator.
            "def acc():".into(),
            "    total = 0".into(),
            "    while True:".into(),
            "        v = yield total".into(),
            "        if v is None:".into(),
            "            break".into(),
            "        total += v".into(),
            "gen = acc()".into(),
            "next(gen)".into(),
            format!("print(gen.send({v1}))"),
            format!("print(gen.send({v2}))"),
            format!("print(gen.send({v3}))"),
        ],
        _ => vec![
            // try/finally cleanup on an uncaught throw propagating out.
            "def g():".into(),
            "    try:".into(),
            "        yield 1".into(),
            "        yield 2".into(),
            "    finally:".into(),
            "        print('cleanup')".into(),
            "gen = g()".into(),
            "print(next(gen))".into(),
            "try:".into(),
            "    gen.throw(RuntimeError('x'))".into(),
            "except RuntimeError as e:".into(),
            "    print('propagated', type(e).__name__)".into(),
        ],
    }
}

/// Exception control flow: try/except/else/finally, multi-type handlers, and
/// bare-`raise` re-raise. Output is deterministic (type names + fixed messages,
/// never a raw traceback).
fn gen_exceptions(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let k = r.below(5) as i64;
    match r.below(3) {
        0 => vec![
            "def risky(k):".into(),
            "    if k == 0: raise ValueError('bad value')".into(),
            "    if k == 1: return 1 // 0".into(),
            "    if k == 2: return [1, 2][5]".into(),
            "    if k == 3: raise KeyError('missing')".into(),
            "    return 'ok'".into(),
            "try:".into(),
            format!("    print('result', risky({k}))"),
            "except ValueError as e:".into(),
            "    print('ValueError', e)".into(),
            "except (IndexError, ZeroDivisionError) as e:".into(),
            "    print('arith/index', type(e).__name__)".into(),
            "except Exception as e:".into(),
            "    print('other', type(e).__name__)".into(),
            "finally:".into(),
            "    print('done')".into(),
        ],
        1 => vec![
            "def g():".into(),
            "    try:".into(),
            format!("        if {k} < 3: raise ValueError('x')"),
            "        return 'clean'".into(),
            "    except ValueError:".into(),
            "        print('inner handling')".into(),
            "        raise".into(),
            "try:".into(),
            "    print('got', g())".into(),
            "except ValueError as e:".into(),
            "    print('reraised', e)".into(),
        ],
        _ => vec![
            "x = 0".into(),
            "try:".into(),
            format!("    x = 10 // {k}"),
            "except ZeroDivisionError:".into(),
            "    x = -1".into(),
            "else:".into(),
            "    x += 100".into(),
            "finally:".into(),
            "    x += 1".into(),
            "print(x)".into(),
        ],
    }
}

/// Sequence unpacking: starred targets, nested targets, call-site `*`/`**`
/// spreads, and literal spreads. Deterministic (ordered scalars/lists/dicts).
fn gen_unpacking(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    let b = pick(r, SMALLINTS);
    let c = pick(r, SMALLINTS);
    let n = 3 + r.below(4);
    match r.below(6) {
        0 => vec![format!("a, *b, c = [{a}, {b}, {c}, 7, 8]\nprint(a, b, c)")],
        1 => vec![format!("first, *rest = range({n})\nprint(first, rest)")],
        2 => vec![format!("*init, last = [{a}, {b}, {c}]\nprint(init, last)")],
        3 => vec![format!("(x, y), z = ({a}, {b}), {c}\nprint(x, y, z)")],
        4 => vec![
            format!("def f(p, q, r): return p * 100 + q * 10 + r"),
            format!("args = [{a}, {b}, {c}]"),
            "print(f(*args))".into(),
            format!("print(*[{a}, {b}, {c}], sep='-')"),
        ],
        _ => vec![
            format!("d1 = {{'a': {a}, 'b': {b}}}"),
            format!("d2 = {{'b': {c}, 'c': 9}}"),
            "print({**d1, **d2})".into(),
            format!("print([*[{a}, {b}], *[{c}]])"),
        ],
    }
}

/// Comprehensions: list/set/dict/nested + conditions + genexpr laziness. Set
/// outputs are always sorted (set iteration order is impl-defined).
fn gen_comprehension(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = 3 + r.below(5);
    let m = 2 + r.below(4);
    let k = pick(r, POSINTS);
    match r.below(6) {
        0 => vec![format!("print([x * x for x in range({n})])")],
        1 => vec![format!("print([x for x in range({n}) if x % {k} == 0])")],
        2 => vec![format!("print(sorted({{x % {k} for x in range({n})}}))")],
        3 => vec![format!("print({{x: x * x for x in range({n})}})")],
        4 => vec![format!(
            "print([x * y for x in range({m}) for y in range({m})])"
        )],
        _ => vec![format!("print(sum(x * x for x in range({n})))")],
    }
}

/// dict views + set algebra + frozenset. All set/frozenset results are printed
/// via a sorted list, a scalar, or a bool so output never depends on iteration
/// order (dict/dict-view order IS deterministic and printed directly).
fn gen_dictset(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    let b = pick(r, SMALLINTS);
    let c = pick(r, SMALLINTS);
    let d = pick(r, SMALLINTS);
    match r.below(8) {
        0 => vec![format!(
            "print(sorted({{{a}, {b}, {c}}} | {{{b}, {c}, {d}}}))"
        )],
        1 => vec![format!(
            "print(sorted({{{a}, {b}, {c}}} & {{{b}, {c}, {d}}}))"
        )],
        2 => vec![format!(
            "print({{{a}, {b}}} <= {{{a}, {b}, {c}}}, {{{a}, {b}}} < {{{a}, {b}}})"
        )],
        3 => vec![
            format!("fs = frozenset([{a}, {b}, {c}])"),
            format!("m = {{fs: 'v'}}"),
            format!("print(m[frozenset([{c}, {b}, {a}])])"),
            "print(isinstance(fs, frozenset), isinstance(fs, set))".into(),
        ],
        4 => vec![
            format!("dd = {{{a}: 1, {b}: 2, {c}: 3}}"),
            "print(sorted(dd.keys()))".into(),
            "print(sorted(dd.values()))".into(),
            format!("print(sorted(dd.keys() | {{{d}}}))"),
        ],
        5 => vec![
            format!("dd = {{{a}: 1, {b}: 2}}"),
            "print(type(dd.keys()).__name__)".into(),
            "print(len(dd.items()))".into(),
            format!("print(dict.fromkeys([{a}, {b}, {c}], 0))"),
        ],
        6 => vec![
            format!("d1 = {{'a': {a}}}"),
            format!("d2 = {{'b': {b}}}"),
            "print(d1 | d2)".into(),
            format!("d1.update(c={c})\nprint(d1)"),
        ],
        _ => vec![format!(
            "print(sorted({{{a}, {b}, {c}}}.symmetric_difference([{b}, {c}, {d}])))"
        )],
    }
}

/// Lazy iterators (`zip`/`map`/`filter`/`enumerate`/`reversed`) driven via
/// `next()`/`list()`, including an infinite generator source (no hang if lazy).
fn gen_itertools(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    let b = pick(r, SMALLINTS);
    let c = pick(r, SMALLINTS);
    let s = pick(r, POSINTS);
    match r.below(6) {
        0 => vec![
            format!("z = zip([{a}, {b}], [{c}, 9])"),
            "print(next(z))".into(),
            "print(list(z))".into(),
            "print(next(z, 'end'))".into(),
        ],
        1 => vec![format!(
            "print(list(map(lambda t: t * 2, [{a}, {b}, {c}])))"
        )],
        2 => vec![format!(
            "print(list(filter(lambda t: t > {b}, [{a}, {b}, {c}, 9])))"
        )],
        3 => vec![format!(
            "print(list(enumerate([{a}, {b}, {c}], start={s})))"
        )],
        4 => vec![
            format!("rv = reversed([{a}, {b}, {c}])"),
            "print(next(rv))".into(),
            "print(list(rv))".into(),
        ],
        _ => vec![
            "def cnt():".into(),
            "    i = 0".into(),
            "    while True:".into(),
            "        yield i".into(),
            "        i += 1".into(),
            format!("print(list(zip(cnt(), [{a}, {b}, {c}])))"),
        ],
    }
}

/// Complex arithmetic: `+ - * / **`, `complex()` constructor, `.real`/`.imag`/
/// `.conjugate()`, `abs`. `repr((a+bj))` is deterministic across impls.
fn gen_complexnum(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = 1 + r.below(5);
    let b = 1 + r.below(5);
    let c = 1 + r.below(5);
    let d = 1 + r.below(5);
    match r.below(6) {
        0 => vec![format!("print(({a}+{b}j) + ({c}+{d}j))")],
        1 => vec![format!("print(({a}+{b}j) * ({c}+{d}j))")],
        2 => vec![format!("print(({a}+{b}j) - ({c}+{d}j))")],
        3 => vec![format!("print(complex({a}, {b}).conjugate())")],
        4 => vec![format!(
            "z = {a}+{b}j\nprint(z.real, z.imag, abs(z) == abs(z))"
        )],
        _ => vec![format!("print(({a}+{b}j) ** 2)")],
    }
}

/// Exception chaining: `raise X from Y` (`__cause__`) and implicit `__context__`
/// during handling. Output is deterministic type names / booleans.
fn gen_exceptions2(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let excs = ["ValueError", "TypeError", "KeyError", "RuntimeError"];
    let e1 = excs[r.below(excs.len() as u64) as usize];
    let e2 = excs[r.below(excs.len() as u64) as usize];
    match r.below(3) {
        0 => vec![
            "try:".into(),
            "    try:".into(),
            format!("        raise {e1}('inner')"),
            format!("    except {e1} as e:"),
            format!("        raise {e2}('outer') from e"),
            format!("except {e2} as t:"),
            "    print(type(t.__cause__).__name__, t.__suppress_context__)".into(),
        ],
        1 => vec![
            "try:".into(),
            "    try:".into(),
            format!("        raise {e1}('a')"),
            format!("    except {e1}:"),
            format!("        raise {e2}('b')"),
            format!("except {e2} as t:"),
            "    print(type(t.__context__).__name__, t.__cause__)".into(),
        ],
        _ => vec![
            "class E(Exception): pass".into(),
            "try:".into(),
            format!("    raise E('x') from {e1}('c')"),
            "except E as e:".into(),
            "    print(type(e.__cause__).__name__)".into(),
        ],
    }
}

/// User exception subclasses: `.args`/`str`/`repr`/message, inheritance chains,
/// `isinstance`, `super().__init__`, custom `__str__`. Output is deterministic
/// (fixed messages, no traceback — everything is caught or printed).
fn gen_exceptions3(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, WORDS);
    let b = pick(r, WORDS);
    let n = pick(r, POSINTS);
    match r.below(6) {
        0 => vec![
            "class E(Exception): pass".into(),
            format!("e = E({a})"),
            "print(str(e), repr(e), e.args)".into(),
            "print(isinstance(e, Exception), isinstance(e, E))".into(),
        ],
        1 => vec![
            "class E(Exception): pass".into(),
            format!("e = E({a}, {b})"),
            "print(str(e))".into(),
            "print(repr(e))".into(),
            "print(e.args)".into(),
            "print(str(E()), repr(E()), E().args)".into(),
        ],
        2 => vec![
            "class E(Exception):".into(),
            "    def __init__(self, code):".into(),
            "        super().__init__('err:' + str(code))".into(),
            "        self.code = code".into(),
            format!("e = E({n})"),
            "print(str(e), e.args, e.code)".into(),
            format!(
                "try:\n    raise E({n})\nexcept E as x:\n    print('caught', x, x.code, x.args)"
            ),
        ],
        3 => vec![
            "class E(Exception):".into(),
            "    def __str__(self): return 'S:' + str(self.args)".into(),
            format!("e = E({a})"),
            "print(str(e), repr(e))".into(),
        ],
        4 => vec![
            "class Base(Exception): pass".into(),
            "class Mid(Base): pass".into(),
            "class Leaf(Mid): pass".into(),
            format!("e = Leaf({a})"),
            "print(isinstance(e, Base), isinstance(e, Mid), isinstance(e, Exception))".into(),
            "print(str(e), e.args, type(e).__name__)".into(),
        ],
        _ => vec![
            "class MyVal(ValueError): pass".into(),
            format!(
                "try:\n    raise MyVal({a})\nexcept ValueError as e:\n    print(str(e), e.args, isinstance(e, ValueError), type(e).__name__)"
            ),
        ],
    }
}

/// Exception control-flow & chaining: `try`/`except`/`else`/`finally` ordering
/// with `return`/`break`/`continue` crossing a `finally`; `finally`/`return`
/// override; tuple / subclass / bare `except` matching and `as e` name deletion;
/// implicit `__context__` (including a builtin error raised inside a handler),
/// explicit `raise X from Y` (`__cause__`); `KeyError` quoting; bare re-raise.
/// Output is deterministic scalars / ordered lists.
fn gen_exceptions4(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = pick(r, POSINTS);
    let k = r.below(4) as i64;
    let excs = ["ValueError", "TypeError", "KeyError", "IndexError"];
    let e1 = excs[r.below(excs.len() as u64) as usize];
    let e2 = excs[r.below(excs.len() as u64) as usize];
    match r.below(12) {
        // `finally` runs on a `return` from inside `try`; a plain return survives.
        0 => vec![
            "def f(k):".into(),
            "    try:".into(),
            "        if k: return 'from_try'".into(),
            "        return 'no_k'".into(),
            "    finally:".into(),
            "        print('finally ran')".into(),
            format!("print(f({}))", k),
        ],
        // `return` in `finally` overrides a `return` in `try`.
        1 => vec![
            "def f():".into(),
            "    try:".into(),
            "        return 'try'".into(),
            "    finally:".into(),
            "        return 'finally'".into(),
            "print(f())".into(),
        ],
        // `break`/`continue` inside `try` still run `finally`, in order.
        2 => vec![
            "log = []".into(),
            format!("for i in range({n}):"),
            "    try:".into(),
            format!("        if i == {k}: break"),
            "        log.append(i)".into(),
            "    finally:".into(),
            "        log.append('f%d' % i)".into(),
            "print(log)".into(),
        ],
        3 => vec![
            "log = []".into(),
            format!("for i in range({n}):"),
            "    try:".into(),
            format!("        if i == {k}: continue"),
            "        log.append(i)".into(),
            "    finally:".into(),
            "        log.append('f%d' % i)".into(),
            "    log.append('tail%d' % i)".into(),
            "print(log)".into(),
        ],
        // `else` runs only when the `try` body raised nothing.
        4 => vec![
            "def f(k):".into(),
            "    order = []".into(),
            "    try:".into(),
            "        if k: raise ValueError('x')".into(),
            "        order.append('body')".into(),
            "    except ValueError:".into(),
            "        order.append('except')".into(),
            "    else:".into(),
            "        order.append('else')".into(),
            "    finally:".into(),
            "        order.append('finally')".into(),
            "    return order".into(),
            format!("print(f({}))", k),
        ],
        // Tuple-of-types `except (A, B)` + subclass matching.
        5 => vec![
            "class MyErr(ValueError): pass".into(),
            "def f(k):".into(),
            "    try:".into(),
            format!("        if k == 0: raise {e1}('a')"),
            "        if k == 1: raise MyErr('b')".into(),
            "        return 'ok'".into(),
            format!("    except ({e1}, TypeError) as e:"),
            "        return ('tuple', type(e).__name__)".into(),
            "    except ValueError as e:".into(),
            "        return ('subclass', type(e).__name__)".into(),
            format!("print(f({}))", k),
        ],
        // The `as e` name is deleted once the handler exits.
        6 => vec![
            "try:".into(),
            format!("    raise {e1}('boom')"),
            format!("    _ = {e2}"),
            format!("except {e1} as e:"),
            "    print('inside', type(e).__name__)".into(),
            "print('e' in dir())".into(),
        ],
        // Bare `except:` catches anything.
        7 => vec![
            "def f(k):".into(),
            "    try:".into(),
            format!("        if k == 0: raise {e1}('a')"),
            "        if k == 1: return 1 // 0".into(),
            "        return [1][9]".into(),
            "    except:".into(),
            "        return 'caught'".into(),
            format!("print(f({}))", k),
        ],
        // Explicit `raise X from Y` sets `__cause__`; suppresses context.
        8 => vec![
            "try:".into(),
            "    try:".into(),
            format!("        raise {e1}('inner')"),
            format!("    except {e1} as e:"),
            format!("        raise {e2}('outer') from e"),
            format!("    except {e1}:"),
            "        print('unreached')".into(),
            format!("except {e2} as t:"),
            "    print(type(t.__cause__).__name__, t.__suppress_context__)".into(),
        ],
        // Implicit `__context__` when a *builtin* error is raised in a handler.
        9 => vec![
            "try:".into(),
            "    try:".into(),
            format!("        raise {e1}('first')"),
            format!("    except {e1}:"),
            "        d = {}".into(),
            "        d['missing']".into(),
            "except KeyError as e:".into(),
            "    print(type(e).__name__, str(e), type(e.__context__).__name__)".into(),
        ],
        // `KeyError` quoting: `str`/`repr`/`.args` for a missing key.
        10 => vec![
            format!("d = {{'a': {n}}}"),
            "try:".into(),
            "    d['zzz']".into(),
            "except KeyError as e:".into(),
            "    print(str(e), repr(e), e.args)".into(),
            "print(str(KeyError('k')), repr(KeyError('k')), KeyError(1, 2).args)".into(),
        ],
        // Bare re-raise from a handler propagates the same exception outward.
        _ => vec![
            "def g(k):".into(),
            "    try:".into(),
            format!("        if k: raise {e1}('deep')"),
            "        return 'clean'".into(),
            format!("    except {e1}:"),
            "        print('logging')".into(),
            "        raise".into(),
            "try:".into(),
            format!("    print('got', g({}))", k),
            format!("except {e1} as e:"),
            "    print('reraised', type(e).__name__, e)".into(),
        ],
    }
}

/// Closures: nested functions, `nonlocal` counters, late-binding loop captures,
/// default-arg early binding, decorators with arguments, `*args`/`**kw` wrappers,
/// and multi-level lexical capture. Deterministic scalar output.
fn gen_closures(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, POSINTS);
    let b = pick(r, POSINTS);
    let n = 2 + r.below(4);
    match r.below(6) {
        0 => vec![
            "def make():".into(),
            "    count = 0".into(),
            "    def inc():".into(),
            "        nonlocal count".into(),
            "        count += 1".into(),
            "        return count".into(),
            "    return inc".into(),
            "c = make()".into(),
            "print(c(), c(), c())".into(),
        ],
        // Late binding: every lambda sees the final loop value.
        1 => vec![
            "fs = []".into(),
            format!("for i in range({n}):"),
            "    fs.append(lambda: i)".into(),
            "print([f() for f in fs])".into(),
        ],
        // Default-arg early binding: each lambda captures its own value.
        2 => vec![
            "fs = []".into(),
            format!("for i in range({n}):"),
            "    fs.append(lambda i=i: i * 2)".into(),
            "print([f() for f in fs])".into(),
        ],
        // Decorator with arguments.
        3 => vec![
            "def mul(factor):".into(),
            "    def deco(fn):".into(),
            "        def wrap(x): return fn(x) * factor".into(),
            "        return wrap".into(),
            "    return deco".into(),
            format!("@mul({a})"),
            "def f(x): return x + 1".into(),
            format!("print(f({b}))"),
        ],
        // `*args`/`**kw` forwarding wrapper.
        4 => vec![
            "def logged(fn):".into(),
            "    def w(*args, **kw):".into(),
            "        return fn(*args, **kw)".into(),
            "    return w".into(),
            "@logged".into(),
            "def add(a, b): return a + b".into(),
            format!("print(add({a}, b={b}))"),
        ],
        // Three-level lexical capture.
        _ => vec![
            "def outer(x):".into(),
            "    def middle(y):".into(),
            "        def inner(z): return x + y + z".into(),
            "        return inner".into(),
            "    return middle".into(),
            format!("print(outer({a})({b})(1))"),
        ],
    }
}

/// Function parameters & calling conventions: `*args`/`**kwargs` collection,
/// positional-only (`/`) and keyword-only (`*`) params, defaults (incl. the
/// shared-mutable-default gotcha), call-site unpacking (`f(*it)`, `f(**map)`,
/// multiple `f(*a, *b, **c, **d)`), keywords passed positionally and vice-versa,
/// lambdas with the same features, and the full family of argument-binding
/// `TypeError`s (missing / too-many / multiple-values / unexpected keyword /
/// positional-only-as-keyword / duplicate `**` key). Error cases use only
/// top-level `def f`/`lambda` so the callable name matches CPython's under
/// `--stderr`. Every stdout case prints deterministic values (kwargs sorted).
fn gen_calls(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, POSINTS);
    let b = pick(r, POSINTS);
    let c = pick(r, POSINTS);
    let d = pick(r, POSINTS);
    match r.below(24) {
        // ---- stdout cases: successful binding, deterministic values ----------
        // Full mixed signature: posonly / pos-or-kw / *args / kwonly / **kwargs.
        0 => vec![
            "def f(p, q, /, r, *args, k, m=9, **kw):".into(),
            "    return (p, q, r, args, k, m, sorted(kw.items()))".into(),
            format!("print(f({a}, {b}, {c}, 100, 200, k={d}, x=1, y=2))"),
        ],
        // `*args`/`**kwargs` collection with a mix of positional & keyword.
        1 => vec![
            "def f(*args, **kw): return (args, sorted(kw.items()))".into(),
            format!("print(f({a}, {b}, {c}, u={d}, v=1, w=2))"),
        ],
        // Keyword-only after a named `*args`; defaults on some.
        2 => vec![
            "def f(a, b=2, *rest, c, d=4, **kw):".into(),
            "    return (a, b, rest, c, d, sorted(kw.items()))".into(),
            format!("print(f({a}, {b}, 7, 8, c={c}, z=9))"),
        ],
        // Positional-only params before `/`, then pos-or-kw.
        3 => vec![
            "def f(x, y, /, z): return (x, y, z)".into(),
            format!("print(f({a}, {b}, z={c}))"),
        ],
        // Call-site `*iterable` unpacking (list, tuple, range).
        4 => vec![
            "def f(a, b, c, d): return (a, b, c, d)".into(),
            format!("print(f(*[{a}, {b}], *({c},), {d}))"),
        ],
        // Call-site `**mapping` unpacking + explicit keyword.
        5 => vec![
            "def f(a, b, c): return (a, b, c)".into(),
            format!("print(f({a}, **{{'b': {b}, 'c': {c}}}))"),
        ],
        // Multiple unpackings: f(*a, *b, **c, **d) with disjoint keys.
        6 => vec![
            "def f(*args, **kw): return (args, sorted(kw.items()))".into(),
            format!("print(f(*[{a}], *[{b}, {c}], **{{'p': 1}}, **{{'q': 2}}))"),
        ],
        // Shared-mutable-default gotcha: list accumulates across calls.
        7 => vec![
            "def f(v, acc=[]):".into(),
            "    acc.append(v)".into(),
            "    return list(acc)".into(),
            format!("print(f({a}))"),
            format!("print(f({b}))"),
            format!("print(f({c}))"),
        ],
        // Default value referencing a module global (bound at def time).
        8 => vec![
            format!("BASE = {a}"),
            "def f(x, step=10): return x + step".into(),
            format!("print(f(BASE), f(BASE, {b}))"),
        ],
        // Keyword args passed in a different order than declared.
        9 => vec![
            "def f(a, b, c): return (a, b, c)".into(),
            format!("print(f(c={c}, a={a}, b={b}))"),
        ],
        // A positional-or-keyword param passed positionally then by keyword mix.
        10 => vec![
            "def f(a, b, c, d): return (a, b, c, d)".into(),
            format!("print(f({a}, {b}, d={d}, c={c}))"),
        ],
        // Lambda with the full param feature set.
        11 => vec![
            "f = lambda a, b=3, *c, d, **e: (a, b, c, d, sorted(e.items()))".into(),
            format!("print(f({a}, {b}, 9, d={d}, z=5))"),
        ],
        // Nested calls: forward *args/**kwargs through a wrapper.
        12 => vec![
            "def inner(a, b, c): return a * 100 + b * 10 + c".into(),
            "def outer(*args, **kw): return inner(*args, **kw)".into(),
            format!("print(outer({a}, {b}, c={c}))"),
        ],
        // Bare `*` keyword-only marker with defaults.
        13 => vec![
            "def f(a, *, b, c=7): return (a, b, c)".into(),
            format!("print(f({a}, b={b}))"),
        ],
        // `**kwargs` absorbing a name that shadows a positional-only param.
        14 => vec![
            "def f(a, /, **kw): return (a, sorted(kw.items()))".into(),
            format!("print(f({a}, a={b}, z={c}))"),
        ],
        // *args tuple is empty when no extra positionals are given.
        15 => vec![
            "def f(a, *args): return (a, args)".into(),
            format!("print(f({a}))"),
        ],
        // ---- error cases: argument-binding TypeErrors (top-level f / lambda) --
        // Missing required positional argument(s).
        16 => {
            let params = ["a, b", "a, b, c", "a, b, c, d"][r.below(3) as usize];
            let given = match r.below(2) {
                0 => String::new(),
                _ => a.to_string(),
            };
            vec![
                format!("def f({params}): return 0"),
                format!("print(f({given}))"),
            ]
        }
        // Too many positional arguments (with / without defaults, via unpacking).
        17 => match r.below(3) {
            0 => vec![
                "def f(a, b): return 0".into(),
                format!("print(f({a}, {b}, {c}))"),
            ],
            1 => vec![
                "def f(a, b=2): return 0".into(),
                format!("print(f({a}, {b}, {c}, {d}))"),
            ],
            _ => vec![
                "def f(a): return 0".into(),
                format!("print(f(*[{a}, {b}]))"),
            ],
        },
        // Multiple values for the same argument (positional + keyword).
        18 => vec![
            "def f(a, b): return 0".into(),
            format!("print(f({a}, a={b}))"),
        ],
        // Unexpected keyword argument.
        19 => vec![
            "def f(a, b): return 0".into(),
            format!("print(f({a}, {b}, zz={c}))"),
        ],
        // Positional-only argument passed as a keyword (no **kwargs to absorb).
        20 => vec![
            "def f(a, b, /): return 0".into(),
            format!("print(f({a}, b={b}))"),
        ],
        // Missing required keyword-only argument(s).
        21 => match r.below(2) {
            0 => vec![
                "def f(a, *, k): return 0".into(),
                format!("print(f({a}))"),
            ],
            _ => vec![
                "def f(*, k, m): return 0".into(),
                "print(f())".into(),
            ],
        },
        // Duplicate keyword via `**` merge / keyword + `**` (multiple values).
        22 => match r.below(2) {
            0 => vec![
                "def f(a, b): return 0".into(),
                format!("print(f(**{{'a': {a}}}, **{{'a': {b}, 'b': {c}}}))"),
            ],
            _ => vec![
                "def f(a): return 0".into(),
                format!("print(f(a={a}, **{{'a': {b}}}))"),
            ],
        },
        // Bare `*` marker must reject extra positionals (regression guard).
        _ => vec![
            "def f(a, *, k=1): return 0".into(),
            format!("print(f({a}, {b}, {c}))"),
        ],
    }
}

/// OOP internals: multiple-inheritance MRO + attribute order, `super()` in a
/// property, `__init_subclass__` with class kwargs, and classmethod alternate
/// constructors. Deterministic scalar/name output.
fn gen_oop2(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    match r.below(5) {
        // MRO order + cooperative dispatch.
        0 => vec![
            "class A:".into(),
            "    def who(self): return 'A'".into(),
            "class B(A):".into(),
            "    def who(self): return 'B'".into(),
            "class C(A):".into(),
            "    def who(self): return 'C'".into(),
            "class D(B, C): pass".into(),
            "print(D().who())".into(),
            "print([c.__name__ for c in D.__mro__])".into(),
        ],
        // super() inside a property getter.
        1 => vec![
            "class A:".into(),
            "    @property".into(),
            "    def v(self): return 10".into(),
            "class B(A):".into(),
            "    @property".into(),
            "    def v(self): return super().v + 1".into(),
            "print(B().v)".into(),
        ],
        // __init_subclass__ with a class keyword.
        2 => vec![
            "class P:".into(),
            "    def __init_subclass__(cls, /, tag=None, **kw):".into(),
            "        cls.tag = tag".into(),
            format!("class C(P, tag={a}): pass"),
            "print(C.tag)".into(),
        ],
        // Classmethod alternate constructors.
        3 => vec![
            "class Shape:".into(),
            "    def __init__(self, n): self.n = n".into(),
            "    @classmethod".into(),
            "    def unit(cls): return cls(1)".into(),
            "    @classmethod".into(),
            format!("    def scaled(cls, k): return cls({a} + k)"),
            "print(Shape.unit().n, Shape.scaled(3).n)".into(),
        ],
        // MI attribute resolution order.
        _ => vec![
            "class A:".into(),
            "    x = 1".into(),
            "class B:".into(),
            "    x = 2".into(),
            "    y = 3".into(),
            "class C(A, B): pass".into(),
            "print(C.x, C.y)".into(),
            "print([k.__name__ for k in C.__mro__])".into(),
        ],
    }
}

/// String formatting depth: `!r`/`!s`/`!a` on containers, positional `.format`
/// reuse, `%`-format of tuples, and int format specs. All values are plain
/// (ints/lists/dicts) so stdout is deterministic and stays clear of the
/// `%`-on-instance-dunder and nested-field-spec gaps.
fn gen_strfmt2(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let a = pick(r, SMALLINTS);
    let b = pick(r, SMALLINTS);
    match r.below(6) {
        // !r / !s on a list.
        0 => vec![format!("xs = [{a}, {b}]\nprint(f'{{xs!r}}|{{xs!s}}')")],
        // !r on a dict (insertion order is deterministic in both).
        1 => vec![format!("d = {{'k': {a}, 'j': {b}}}\nprint(f'{{d!r}}')")],
        // %-format of a tuple (plain values), both %r and %s.
        2 => vec![format!("t = ({a}, {b})\nprint('%r %s' % (t, t))")],
        // Positional field reuse in str.format.
        3 => vec![format!("print('{{0}}-{{1}}-{{0}}'.format({a}, {b}))")],
        // Int format specs via variables (no nested-field spec).
        4 => vec![
            format!("v = {a}"),
            format!("w = {b}"),
            "print(f'{v:+05d} {w:>6d}')".into(),
        ],
        // % conversion chars on ints.
        _ => vec![format!(
            "print('%d/%o/%x/%X' % ({a}, abs({b}), abs({a}), abs({b})))"
        )],
    }
}

/// `bytes`/`bytearray` literals with repr edge cases (quotes, control bytes,
/// non-ASCII). Every generated case has deterministic stdout in both engines.
const BLIT: &[&str] = &[
    "b'hello'",
    "b'World'",
    "b'abcabc'",
    "b''",
    "b'a'",
    "b'foo bar'",
    "b'a,b,c'",
    "b'  pad  '",
    "b'AbC'",
    "b'x-y-z'",
    "b'ab.cd.ef'",
    "b\"a'b\"",
    "b'a\"b'",
    "b'tab\\ther'",
    "b'\\x00\\xff'",
];
/// Non-empty single/short byte separators (never `b''`, which raises).
const BSEP: &[&str] = &["b','", "b'-'", "b'.'", "b' '", "b'a'", "b'X'", "b'cd'"];
/// ASCII-only byte literals safe to `.decode('utf-8')`/`'ascii'`.
const BDEC: &[&str] = &["b'hello'", "b'World'", "b'abc'", "b''", "b'A,B,C'"];

fn gen_bytesops(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let b = pick(r, BLIT);
    let b2 = pick(r, BLIT);
    let sep = pick(r, BSEP);
    let idx = pick(r, &["0", "1", "2", "-1", "-2", "3"]);
    let jdx = pick(r, &["0", "1", "2", "3", "-1", "5"]);
    let n = r.below(3);
    let e = match r.below(26) {
        0 => format!("print({b})"),
        1 => format!("print(bytearray({b}))"),
        2 => format!("print(repr({b}), repr(bytearray({b})))"),
        3 => format!("print({b}[{idx}:{jdx}], {b}[::-1], {b}[::2])"),
        4 => format!("print({b} + {b2}, bytearray({b}) + {b2})"),
        5 => format!("print({b} * {n}, {n} * bytearray({b}))"),
        6 => format!("print({sep} in {b}, 97 in {b}, 300 not in {b})"),
        7 => format!("print({b}.split({sep}), {b}.split())"),
        8 => format!("print({b}.rsplit({sep}, 1), {b}.rsplit())"),
        9 => format!("print({sep}.join([b'a', b'b', b'c']), bytearray({sep}).join([{b}]))"),
        10 => format!("print({b}.replace({sep}, b'YY'), {b}.replace({sep}, b'', 1))"),
        11 => format!("print({b}.find({sep}), {b}.rfind({sep}), {b}.count({sep}))"),
        12 => format!("print({b}.startswith({sep}), {b}.endswith({sep}))"),
        13 => format!("print({b}.strip(), {b}.lstrip(), {b}.rstrip())"),
        14 => "print(b'xxhelloxx'.strip(b'x'), b'--a--'.lstrip(b'-'))".to_string(),
        15 => format!("print({b}.upper(), {b}.lower())"),
        16 => "print(b'a\\nb\\r\\nc\\rd'.splitlines(), b'a\\nb\\n'.splitlines(True))".to_string(),
        17 => format!("print({b}.partition({sep}), {b}.rpartition({sep}))"),
        18 => format!("print({b}.removeprefix(b'a'), {b}.removesuffix(b'c'))"),
        19 => format!("print(bytes.fromhex('48656c6c6f20'), {b}.hex())"),
        20 => "print(bytes([72, 105]), bytes(3), bytearray([65, 66]))".to_string(),
        21 => format!("print(list({b}), len({b}))"),
        22 => {
            let d = pick(r, BDEC);
            format!("print({d}.decode('utf-8'), {d}.decode('ascii'))")
        }
        23 => {
            let v = 65 + r.below(20);
            format!("ba = bytearray(b'abcdef')\nba[{idx}] = {v}\nprint(ba)")
        }
        24 => format!("ba = bytearray(b'abcdef')\nba[{idx}:{jdx}] = b'XY'\nprint(ba)"),
        _ => format!("print({b} < {b2}, {b} == bytearray({b}), {b} != {b2})"),
    };
    vec![e]
}

/// The `bytes`/`bytearray` "tail": str-parallel methods (`swapcase`, `title`,
/// `center`/`ljust`/`rjust`, the `isX` predicates, `translate`/`maketrans`),
/// `%`-formatting, `del ba[i]` / `del ba[i:j]`, and `decode(errors=...)`. Every
/// case prints a deterministic value (no error paths) so output stays byte-stable.
fn gen_bytestail(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    // Case-varied literals for the case/title/predicate methods.
    const TLIT: &[&str] = &[
        "b'hello world'",
        "b'Hello World'",
        "b'ABC def'",
        "b\"they're bill's\"",
        "b'x1x y2y'",
        "b'MixedCase'",
        "b'  spaced  '",
        "b'abc123'",
        "b'ABC123'",
        "b'  '",
        "b''",
    ];
    let b = pick(r, TLIT);
    let fill = pick(r, &["b'*'", "b'.'", "b'-'", "b'0'"]);
    let w = pick(r, &["6", "8", "10", "3", "0"]);
    // Deterministic (format, args) pairs — all valid, all byte-stable.
    const PCT: &[&str] = &[
        "b'%d-%s' % (42, b'x')",
        "b'%5d|%-5d|%05d' % (3, 3, 3)",
        "b'%x/%X/%#o' % (255, 255, 8)",
        "b'%c%c%c' % (72, 105, 33)",
        "b'%b and %s' % (b'A', b'B')",
        "b'%a|%r' % (b'\\xff', b'ok')",
        "b'%.2f|%+.1f|% d' % (3.14159, 2.5, 7)",
        "b'%(x)s-%(y)d' % {b'x': b'hi', b'y': 9}",
        "b'%*d|%-*d|' % (6, 3, 6, 3)",
        "b'%s' % bytearray(b'ba')",
        "bytearray(b'%d.%d') % (1, 2)",
        "b'%%literal%% %d' % (5,)",
    ];
    let idx = pick(r, &["0", "1", "2", "-1", "-2", "7"]);
    let e = match r.below(24) {
        0 => format!("print({b}.swapcase())"),
        1 => format!("print({b}.title())"),
        2 => format!("print({b}.title(), bytearray({b}).title())"),
        3 => format!("print({b}.center({w}, {fill}), {b}.center({w}))"),
        4 => format!("print({b}.ljust({w}, {fill}), {b}.rjust({w}, {fill}))"),
        5 => format!("print({b}.ljust({w}), {b}.rjust({w}))"),
        6 => format!("print({b}.isalpha(), {b}.isdigit(), {b}.isalnum())"),
        7 => format!("print({b}.isspace(), {b}.isupper(), {b}.islower())"),
        8 => format!("print({b}.istitle(), {b}.isascii())"),
        9 => format!("print(bytearray({b}).swapcase(), bytearray({b}).isupper())"),
        10 => {
            "t = bytes.maketrans(b'abcABC', b'xyzXYZ')\n\
             print(b'aAbBcCd'.translate(t))"
                .to_string()
        }
        11 => {
            "t = bytes.maketrans(b'abc', b'xyz')\n\
             print(b'aabbccd'.translate(t, b'a'), b'aabbcc'.translate(None, b'b'))"
                .to_string()
        }
        12 => format!("print(bytearray({b}).translate(bytes.maketrans(b'lo', b'LO')))"),
        13 => "print(bytes.maketrans(b'', b''))".to_string(),
        14 => {
            let p = pick(r, PCT);
            format!("print({p})")
        }
        15 => {
            let p = pick(r, PCT);
            format!("print({p})")
        }
        16 => {
            let p = pick(r, PCT);
            format!("v = {p}\nprint(v, type(v).__name__)")
        }
        17 => format!("ba = bytearray(b'abcdefgh')\ndel ba[{idx}]\nprint(ba)"),
        18 => {
            let a = pick(r, &["1", "2", "3"]);
            let c = pick(r, &["4", "5", "6", "8"]);
            format!("ba = bytearray(b'abcdefgh')\ndel ba[{a}:{c}]\nprint(ba)")
        }
        19 => {
            let k = pick(r, &["2", "3", "-1", "-2"]);
            format!("ba = bytearray(b'abcdefgh')\ndel ba[::{k}]\nprint(ba)")
        }
        20 => "ba = bytearray(b'abcdef')\ndel ba[1:4]\ndel ba[0]\nprint(ba)".to_string(),
        21 => "print(b'a\\xffb'.decode('utf-8', 'ignore'), b'a\\xffb'.decode('utf-8', 'replace'))"
            .to_string(),
        22 => {
            "print(b'x\\x80y'.decode('ascii', 'ignore'), b'x\\x80y'.decode('ascii', errors='replace'))"
                .to_string()
        }
        _ => "print(b'\\xe2\\x28\\xa1'.decode('utf-8', 'replace'), b'ab\\xc3'.decode('utf-8', 'ignore'))"
            .to_string(),
    };
    vec![e]
}

/// `str.format` nested field specs plus keyword / positional-index / attribute /
/// subscript replacement fields.
fn gen_format2(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let w = pick(r, &["4", "6", "8", "10", "12"]);
    let p = pick(r, &["1", "2", "3", "4"]);
    let f = pick(r, &["3.14159", "2.5", "12345.678", "0.5", "-7.25"]);
    let s = pick(r, WORDS);
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let e = match r.below(15) {
        0 => format!("print('{{:{{}}}}'.format({s}, {w}))"),
        1 => format!("print('{{:.{{}}f}}'.format({f}, {p}))"),
        2 => format!("print('{{:{{}}.{{}}f}}'.format({f}, {w}, {p}))"),
        3 => format!("print('{{:>{{wd}}.{{pr}}f}}'.format({f}, wd={w}, pr={p}))"),
        4 => format!("print('{{name}}'.format(name={s}))"),
        5 => format!("print('{{0}}-{{1}}-{{0}}'.format({a}, {b}))"),
        6 => format!("print('{{0[1]}}'.format([{a}, {b}, 9]))"),
        7 => format!("print('{{d[k]}}'.format(d={{'k': {a}}}))"),
        8 => format!("print('{{0.real}}|{{0.imag}}'.format(complex({a}, {b})))"),
        9 => format!("print('{{:{{fill}}>{{wd}}}}'.format({s}, fill='-', wd={w}))"),
        10 => format!("print('{{v:{{aa}}.{{bb}}f}}'.format(v={f}, aa={w}, bb={p}))"),
        11 => format!("print('{{0:{{1}}}}'.format({s}, {w}))"),
        12 => format!("print('{{:+.{{}}e}}'.format({f}, {p}))"),
        13 => format!("print('{{o[0]}}/{{o[2]}}'.format(o=({a}, 0, {b})))"),
        _ => format!("print('{{:^{{}}}}'.format({s}, {w}), '{{:*^{{}}}}'.format({s}, {w}))"),
    };
    vec![e]
}

/// The attribute-access protocol and instance `__dict__`: the live `__dict__`
/// mapping (stable identity, write-through subscript/`del`, `vars()` aliasing),
/// the dunder hooks (`__getattr__` fallback, `__getattribute__`/`__setattr__`/
/// `__delattr__` interception cooperating with the `object.__dunder__` default),
/// the attribute builtins (`getattr`/`setattr`/`delattr`/`hasattr`/`vars`/`dir`),
/// class-vs-instance-attribute precedence, and `__slots__` enforcement. Every
/// program is byte-stable: it prints sorted `__dict__`/`dir` items, captured
/// values, and booleans — never a raw object repr (which carries an address).
fn gen_attr(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = pick(r, SMALLINTS);
    let m = pick(r, SMALLINTS);
    let e: String = match r.below(18) {
        // Live `__dict__`: stable identity + write-through via a captured alias.
        0 => format!(
            "class C:\n    def __init__(self): self.x = {n}\nc = C()\nprint(c.__dict__ is c.__dict__)\nd = c.__dict__\nc.y = {m}\nprint(sorted(d.items()))\nd['z'] = {n}\nprint(c.z, sorted(c.__dict__.keys()))"
        ),
        // `del` through `__dict__` persists; `hasattr` observes it.
        1 => format!(
            "class C:\n    def __init__(self):\n        self.a = {n}\n        self.b = {m}\nc = C()\ndel c.__dict__['a']\nprint(hasattr(c, 'a'), sorted(c.__dict__.items()))"
        ),
        // `getattr`/`setattr`/`hasattr` incl. the `getattr` default.
        2 => format!(
            "class C: pass\nc = C()\nsetattr(c, 'foo', {n})\nprint(getattr(c, 'foo'), hasattr(c, 'foo'), hasattr(c, 'bar'), getattr(c, 'bar', {m}))"
        ),
        // `vars(o)` is `o.__dict__`; mutation through it persists.
        3 => format!(
            "class C:\n    def __init__(self): self.a = {n}\nc = C()\nprint(vars(c) is c.__dict__)\nvars(c)['b'] = {m}\nprint(sorted(c.__dict__.items()))"
        ),
        // `__getattr__` fallback: fires only on a miss, not for a real attr.
        4 => format!(
            "class F:\n    def __getattr__(self, k): return 'missing:' + k\nf = F()\nf.real = {n}\nprint(f.real, f.ghost, getattr(f, 'x'))"
        ),
        // `__getattribute__` intercepts everything; cooperates via object default.
        5 => format!(
            "class G:\n    def __getattribute__(self, k):\n        if k == 'secret': return {m}\n        return object.__getattribute__(self, k)\ng = G()\ng.x = {n}\nprint(g.secret, g.x, sorted(g.__dict__.items()))"
        ),
        // `__setattr__` intercepts every store; object default writes through.
        6 => format!(
            "class S:\n    def __setattr__(self, k, v): object.__setattr__(self, k, v * 2)\ns = S()\ns.a = {n}\ns.b = {m}\nprint(sorted(s.__dict__.items()))"
        ),
        // `__delattr__` intercepts deletion.
        7 => format!(
            "class D:\n    def __delattr__(self, k):\n        print('del', k)\n        object.__delattr__(self, k)\nd = D()\nd.z = {n}\ndel d.z\nprint(hasattr(d, 'z'), sorted(d.__dict__.keys()))"
        ),
        // `__slots__`: reject unlisted names, and no `__dict__` when slots-only.
        8 => format!(
            "class P:\n    __slots__ = ('a', 'b')\n    def __init__(self): self.a = {n}\np = P()\np.b = {m}\nprint(p.a, p.b)\ntry:\n    p.c = 1\nexcept AttributeError as ex:\n    print('AE', 'c' in str(ex))\ntry:\n    p.__dict__\nexcept AttributeError:\n    print('no-dict')"
        ),
        // `dir()` — sorted, includes class + instance names (non-dunder subset).
        9 => format!(
            "class Base:\n    bx = {n}\n    def m(self): return 1\nclass C(Base):\n    cy = {m}\n    def __init__(self): self.i = {n}\nc = C()\nprint([x for x in dir(c) if not x.startswith('_')])\nprint([x for x in dir(C) if not x.startswith('_')])"
        ),
        // Class-attr vs instance-attr precedence + `__dict__` membership.
        10 => format!(
            "class C:\n    shared = {n}\nc = C()\nprint('shared' in c.__dict__, c.shared)\nc.shared = {m}\nprint('shared' in c.__dict__, c.shared, C.shared)\ndel c.shared\nprint('shared' in c.__dict__, c.shared)"
        ),
        // `setattr`/`delattr` by dynamic name, sorted keys across the batch.
        11 => format!(
            "class C: pass\nc = C()\nfor name in ['p', 'q', 'r']:\n    setattr(c, name, {n})\nprint(sorted(c.__dict__.keys()))\ndelattr(c, 'q')\nprint(sorted(c.__dict__.keys()), hasattr(c, 'q'))"
        ),
        // `__dict__` supports normal dict methods (update/pop) with write-through.
        12 => format!(
            "class C:\n    def __init__(self):\n        self.a = {n}\n        self.b = {m}\nc = C()\nc.__dict__.update({{'c': {n}}})\nprint(sorted(c.__dict__.items()))\nc.__dict__.pop('a')\nprint(sorted(c.__dict__.keys()), hasattr(c, 'a'))"
        ),
        // `__getattr__` does NOT fire when `__getattribute__` returns a value,
        // but DOES fire when `__getattribute__` raises AttributeError.
        13 => format!(
            "class C:\n    def __getattr__(self, k): return 'fallback:' + k\n    def __getattribute__(self, k):\n        if k == 'boom': raise AttributeError(k)\n        return object.__getattribute__(self, k)\nc = C()\nc.real = {n}\nprint(c.real, c.boom, c.other)"
        ),
        // Reassignment keeps insertion order (no key churn on update).
        14 => format!(
            "class C:\n    def __init__(self):\n        self.a = {n}\n        self.b = {m}\nc = C()\nc.a = {m}\nc.d = {n}\nprint(list(c.__dict__.keys()), sorted(c.__dict__.values()))"
        ),
        // len / iteration / membership over a live `__dict__`.
        15 => format!(
            "class C:\n    def __init__(self):\n        self.a = {n}\n        self.b = {m}\n        self.c = {n}\nc = C()\nprint(len(c.__dict__), 'a' in c.__dict__, 'z' in c.__dict__)\nprint(sorted(k for k in c.__dict__))"
        ),
        // Empty instance: `__dict__` starts empty and grows.
        16 => format!(
            "class C: pass\nc = C()\nprint(c.__dict__ == {{}}, len(c.__dict__))\nc.only = {n}\nprint(sorted(c.__dict__.items()))"
        ),
        // Inheritance: instance dict holds only its own names, not inherited.
        _ => format!(
            "class A:\n    def __init__(self): self.base = {n}\nclass B(A):\n    def __init__(self):\n        super().__init__()\n        self.own = {m}\nb = B()\nprint(sorted(b.__dict__.items()), hasattr(b, 'base'), hasattr(b, 'own'))"
        ),
    };
    vec![e]
}

/// Decorators and the descriptor protocol: `@property` get/set/delete,
/// `@staticmethod`/`@classmethod` (via class and instance, `cls` binding and
/// inheritance), function decorators (single, stacked, factories), class
/// decorators, and custom descriptors (`__get__`/`__set__`/`__delete__`/
/// `__set_name__`, data-vs-non-data precedence, class-level access).
/// All programs are byte-stable — they print transformed values, names, and
/// counts, never a raw object repr. Descriptor state lives on the descriptor
/// instance (one live object per test), never through `obj.__dict__`.
fn gen_descriptors(seed: u64) -> Vec<String> {
    let r = &mut Rng::new(seed);
    let n = pick(r, &["1", "2", "3", "5", "7", "10", "-4"]);
    let m = pick(r, &["2", "3", "4", "6"]);
    let e: String = match r.below(28) {
        0 => "class C:\n    def __init__(self): self._x = 10\n    @property\n    def x(self): return self._x * 2\nc = C()\nprint(c.x)"
            .to_string(),
        1 => format!(
            "class C:\n    def __init__(self): self._x = 1\n    @property\n    def x(self): return self._x\n    @x.setter\n    def x(self, v): self._x = v + {n}\nc = C()\nc.x = {m}\nprint(c.x)"
        ),
        2 => "class C:\n    def __init__(self): self._x = 5\n    @property\n    def x(self): return self._x\n    @x.setter\n    def x(self, v): self._x = v\n    @x.deleter\n    def x(self): self._x = -99\nc = C()\nprint(c.x)\nc.x = 7\nprint(c.x)\ndel c.x\nprint(c.x)"
            .to_string(),
        3 => "class C:\n    @property\n    def x(self): return 42\nc = C()\nprint(c.x)\ntry:\n    c.x = 1\nexcept AttributeError as ex:\n    print('AE', 'no setter' in str(ex) or 'has no setter' in str(ex))"
            .to_string(),
        4 => "class C:\n    @property\n    def x(self): return 1\nc = C()\ntry:\n    del c.x\nexcept AttributeError as ex:\n    print('AE', 'deleter' in str(ex))"
            .to_string(),
        5 => format!(
            "class C:\n    @staticmethod\n    def add(a, b): return a + b\nprint(C.add({n}, {m}), C().add({n}, {m}))"
        ),
        6 => format!(
            "class C:\n    @classmethod\n    def make(cls, v): return cls.__name__ + ':' + str(v)\nprint(C.make({n}), C().make({m}))"
        ),
        7 => "class A:\n    @classmethod\n    def who(cls): return cls.__name__\nclass B(A): pass\nprint(A.who(), B.who(), B().who())"
            .to_string(),
        8 => "class C:\n    kind = 'K'\n    @classmethod\n    def tag(cls): return cls.kind\n    @staticmethod\n    def s(): return 'S'\nprint(C.tag(), C.s(), C().tag(), C().s())"
            .to_string(),
        9 => format!(
            "def twice(f):\n    def w(*a, **k): return f(*a, **k) * 2\n    return w\n@twice\ndef g(x): return x + {n}\nprint(g({m}))"
        ),
        10 => "def a(f):\n    def w(): return 'a(' + f() + ')'\n    return w\ndef b(f):\n    def w(): return 'b(' + f() + ')'\n    return w\n@a\n@b\ndef g(): return 'g'\nprint(g())"
            .to_string(),
        11 => format!(
            "def rep(k):\n    def d(f):\n        def w(*a, **kw): return f(*a, **kw) * k\n        return w\n    return d\n@rep({m})\ndef g(x): return x + {n}\nprint(g({n}))"
        ),
        12 => "def tag(c):\n    c.marker = 'T'\n    return c\n@tag\nclass C:\n    pass\nprint(C.marker, C().marker)"
            .to_string(),
        13 => "def wrap(c):\n    class New(c):\n        extra = 'new'\n    return New\n@wrap\nclass C:\n    base = 'old'\nprint(C.base, C.extra)"
            .to_string(),
        14 => format!(
            "def twice_dec(f):\n    def w(*a, **kw): return f(*a, **kw) * 2\n    return w\ndef add(k):\n    def d(f):\n        def w(*a, **kw): return f(*a, **kw) + k\n        return w\n    return d\n@twice_dec\n@add({m})\ndef g(x): return x\nprint(g({n}))"
        ),
        15 => "class Named:\n    def __set_name__(self, owner, name): self.name = name\n    def __get__(self, obj, ot=None): return self.name\nclass C:\n    a = Named()\n    bb = Named()\nprint(C.a, C.bb)"
            .to_string(),
        16 => "class D:\n    def __get__(self, obj, ot=None):\n        return 'cls' if obj is None else 'inst'\nclass C:\n    x = D()\nprint(C.x, C().x)"
            .to_string(),
        17 => format!(
            "class Pos:\n    def __init__(self): self.v = 0\n    def __get__(self, obj, ot=None):\n        return self.v if obj is not None else self\n    def __set__(self, obj, val):\n        if val < 0: raise ValueError('neg')\n        self.v = val\nclass C:\n    p = Pos()\nc = C()\nc.p = {m}\nprint(c.p)\ntry:\n    c.p = {n} - 100\nexcept ValueError as ex:\n    print('VE', ex)\nprint(c.p)"
        ),
        18 => "class Log:\n    def __get__(self, obj, ot=None): return 5\n    def __delete__(self, obj): print('deleted')\nclass C:\n    x = Log()\nc = C()\nprint(c.x)\ndel c.x"
            .to_string(),
        19 => "class Data:\n    def __get__(self, o, t=None): return 'D-get'\n    def __set__(self, o, v): pass\nclass NonData:\n    def __get__(self, o, t=None): return 'ND-get'\nclass C:\n    d = Data()\n    nd = NonData()\nc = C()\nc.d = 'x'\nc.nd = 'y'\nprint(c.d, c.nd)"
            .to_string(),
        20 => "class C:\n    def m(self): return 'method'\nc = C()\nprint(C.m(c), c.m())\nc.m = lambda: 'shadow'\nprint(c.m())"
            .to_string(),
        21 => format!(
            "class Temp:\n    def __init__(self, c): self.c = c\n    @property\n    def f(self): return self.c * 9 / 5 + 32\nt = Temp({m})\nprint(t.f)"
        ),
        22 => "class C:\n    @classmethod\n    def alt(cls, v):\n        obj = cls()\n        obj.val = v\n        return obj\nclass D(C): pass\nprint(C.alt(3).val, D.alt(4).val, type(D.alt(5)).__name__)"
            .to_string(),
        23 => format!(
            "def deco(f):\n    def w(*a, **k):\n        return (len(a), sorted(k), f(*a, **k))\n    return w\n@deco\ndef g(x, y, z=0): return x + y + z\nprint(g({n}, {m}, z={n}))"
        ),
        24 => "class Const:\n    def __init__(self, v): self.v = v\n    def __get__(self, obj, ot=None): return self.v\n    def __set__(self, obj, val): raise AttributeError('read-only')\nclass C:\n    x = Const(7)\nc = C()\nprint(c.x)\ntry:\n    c.x = 1\nexcept AttributeError as ex:\n    print('AE', ex)"
            .to_string(),
        25 => "class C:\n    _n = 0\n    @property\n    def n(self): return self._n\n    @n.setter\n    def n(self, v): self._n = v\nclass D(C):\n    @property\n    def n(self): return super().n + 1\nd = D()\nd._n = 4\nprint(d.n)"
            .to_string(),
        26 => "class Counter:\n    def __init__(self): self.calls = 0\n    def __get__(self, obj, ot=None):\n        self.calls += 1\n        return self.calls\nclass C:\n    x = Counter()\nc = C()\nprint(c.x, c.x, c.x)"
            .to_string(),
        _ => format!(
            "def memo(f):\n    cache = []\n    def w(x):\n        cache.append(x)\n        return f(x) + len(cache)\n    return w\n@memo\ndef g(x): return x * {m}\nprint(g({n}), g({n}), g({m}))"
        ),
    };
    vec![e]
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
    Classes,
    Iterproto,
    Generators,
    Exceptions,
    Unpacking,
    Comprehension,
    Dictset,
    Itertools,
    Complexnum,
    Exceptions2,
    Exceptions3,
    Exceptions4,
    Closures,
    Oop2,
    Strfmt2,
    Bytesops,
    Bytestail,
    Format2,
    Async,
    Async2,
    Augwith,
    Descriptors,
    Attr,
    Calls,
    Match,
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
    Mode::Classes,
    Mode::Iterproto,
    Mode::Generators,
    Mode::Exceptions,
    Mode::Unpacking,
    Mode::Comprehension,
    Mode::Dictset,
    Mode::Itertools,
    Mode::Complexnum,
    Mode::Exceptions2,
    Mode::Exceptions3,
    Mode::Exceptions4,
    Mode::Closures,
    Mode::Oop2,
    Mode::Strfmt2,
    Mode::Bytesops,
    Mode::Bytestail,
    Mode::Format2,
    Mode::Async,
    Mode::Async2,
    Mode::Augwith,
    Mode::Descriptors,
    Mode::Attr,
    Mode::Calls,
    Mode::Match,
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
        Mode::Classes => gen_classes(seed),
        Mode::Iterproto => gen_iterproto(seed),
        Mode::Generators => gen_generators(seed),
        Mode::Exceptions => gen_exceptions(seed),
        Mode::Unpacking => gen_unpacking(seed),
        Mode::Comprehension => gen_comprehension(seed),
        Mode::Dictset => gen_dictset(seed),
        Mode::Itertools => gen_itertools(seed),
        Mode::Complexnum => gen_complexnum(seed),
        Mode::Exceptions2 => gen_exceptions2(seed),
        Mode::Exceptions3 => gen_exceptions3(seed),
        Mode::Exceptions4 => gen_exceptions4(seed),
        Mode::Closures => gen_closures(seed),
        Mode::Oop2 => gen_oop2(seed),
        Mode::Strfmt2 => gen_strfmt2(seed),
        Mode::Bytesops => gen_bytesops(seed),
        Mode::Bytestail => gen_bytestail(seed),
        Mode::Format2 => gen_format2(seed),
        Mode::Async => gen_async(seed),
        Mode::Async2 => gen_async2(seed),
        Mode::Augwith => gen_augwith(seed),
        Mode::Descriptors => gen_descriptors(seed),
        Mode::Attr => gen_attr(seed),
        Mode::Calls => gen_calls(seed),
        Mode::Match => gen_match(seed),
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
        Mode::Classes => "classes",
        Mode::Iterproto => "iterproto",
        Mode::Generators => "generators",
        Mode::Exceptions => "exceptions",
        Mode::Unpacking => "unpacking",
        Mode::Comprehension => "comprehension",
        Mode::Dictset => "dictset",
        Mode::Itertools => "itertools",
        Mode::Complexnum => "complexnum",
        Mode::Exceptions2 => "exceptions2",
        Mode::Exceptions3 => "exceptions3",
        Mode::Exceptions4 => "exceptions4",
        Mode::Closures => "closures",
        Mode::Oop2 => "oop2",
        Mode::Strfmt2 => "strfmt2",
        Mode::Bytesops => "bytesops",
        Mode::Bytestail => "bytestail",
        Mode::Format2 => "format2",
        Mode::Async => "async",
        Mode::Async2 => "async2",
        Mode::Augwith => "augwith",
        Mode::Descriptors => "descriptors",
        Mode::Attr => "attr",
        Mode::Calls => "calls",
        Mode::Match => "match",
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
        Mode::Classes,
        Mode::Iterproto,
        Mode::Generators,
        Mode::Exceptions,
        Mode::Unpacking,
        Mode::Comprehension,
        Mode::Dictset,
        Mode::Itertools,
        Mode::Complexnum,
        Mode::Exceptions2,
        Mode::Exceptions3,
        Mode::Exceptions4,
        Mode::Closures,
        Mode::Oop2,
        Mode::Strfmt2,
        Mode::Bytesops,
        Mode::Bytestail,
        Mode::Format2,
        Mode::Async,
        Mode::Async2,
        Mode::Augwith,
        Mode::Descriptors,
        Mode::Attr,
        Mode::Calls,
        Mode::Match,
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
         comparison, builtins, ternary, augassign, async, …\n\
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

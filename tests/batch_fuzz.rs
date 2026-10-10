//! Batched differential fuzz: seeded generator -> ONE program of many cases ->
//! run once under the reference `python3` and once under `python` -> compare
//! the output case by case.
//!
//! `src/bin/parity_fuzz.rs` spawns two interpreters per snippet, which caps it
//! at a few hundred cases per second and makes a wide sweep impractical. Here a
//! program holds `CASES_PER_PROGRAM` cases, each wrapped in its own function and
//! its own `try`, so one raising case cannot hide the ones after it, and an
//! interpreter is started once per program rather than once per case.
//!
//! Output per case is `#<n>` followed by whatever the case printed, with an
//! uncaught exception rendered as `!Type: message | cause=.. ctx=.. sup=..`.
//! A case that is missing from our output (crash, hang, panic) is re-run alone
//! so the report names it instead of a whole batch.
//!
//! Determinism: no `random`, `time`, `id()` or address-dependent output; every
//! `0x...` address is masked before comparing. `PYTHONHASHSEED` is pinned so
//! str/bytes hashing and string-keyed container order are comparable.
//!
//! Knobs (environment): `BATCH_FUZZ_SEED` first program seed (default 0),
//! `BATCH_FUZZ_PROGRAMS` number of programs (default 12), `BATCH_FUZZ_CASES`
//! cases per program (default 40), `BATCH_FUZZ_KEEP=<dir>` writes every
//! diverging program there. Message text is compared verbatim only against a
//! CPython 3.14 reference (the wording pythonrs targets); against another
//! version only the exception type is compared.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// PRNG (splitmix64)
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

/// Fill `@NAME@` placeholders in a template.
fn tpl(s: &str, subs: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (k, v) in subs {
        out = out.replace(&format!("@{k}@"), v);
    }
    out
}

// ---------------------------------------------------------------------------
// Operand pools
// ---------------------------------------------------------------------------

const INTS: &[&str] = &[
    "0",
    "1",
    "-1",
    "2",
    "-2",
    "7",
    "-7",
    "10",
    "255",
    "-256",
    "1000",
    "65535",
    "2**31",
    "-2**31",
    "2**32+5",
    "2**63-1",
    "-2**63",
    "2**64",
    "10**20",
    "-10**20+3",
    "2**100+1",
    "-3**50",
    "12345678901234567890",
    "10**30-1",
];

const FLOATS: &[&str] = &[
    "0.0",
    "-0.0",
    "1.0",
    "-1.5",
    "0.1",
    "0.5",
    "2.5",
    "3.5",
    "1e16",
    "1e-5",
    "123456789.125",
    "1.5e300",
    "5e-324",
    "2.2250738585072014e-308",
    "1.7976931348623157e308",
    "float('inf')",
    "float('-inf')",
    "float('nan')",
    "0.30000000000000004",
    "9007199254740993.0",
    "1e22",
    "1e23",
    "123456.789e-10",
    "4.35",
    "2.675",
    "0.000123",
    "1234567.0",
    "99999.99999",
];

const STRS: &[&str] = &[
    "''",
    "'a'",
    "'abc'",
    "'Hello, World'",
    "'  padded  '",
    "'a,b,,c'",
    "'x\\ty\\nz'",
    "'ß'",
    "'İstanbul'",
    "'ǅ'",
    "'ﬁne'",
    "'ΑΣ ΑΣ.'",
    "'\\u2003em\\u2003'",
    "'日本語'",
    "'a\\x85b'",
    "('ab' * 5)",
    "'123'",
    "'-12'",
    "'①②'",
    "'0x1F'",
    "'snake_case_name'",
    "'Title Case Words'",
    "'MiXeD'",
    "'\\x00\\x1f\\x7f'",
    "'😀 emoji'",
];

fn int(r: &mut Rng) -> &'static str {
    INTS[r.below(INTS.len())]
}
fn float(r: &mut Rng) -> &'static str {
    FLOATS[r.below(FLOATS.len())]
}
fn string(r: &mut Rng) -> &'static str {
    STRS[r.below(STRS.len())]
}

// ---------------------------------------------------------------------------
// Case generators. Each returns the statements of one case body.
// ---------------------------------------------------------------------------

/// A random printf-less format spec for `kind` ('i' int, 'f' float, 's' str).
fn spec(r: &mut Rng, kind: char) -> String {
    let mut s = String::new();
    if r.chance(35) {
        if r.chance(50) {
            s.push_str(r.pick(&["*", "0", "x", "_", "é"]));
        }
        s.push_str(r.pick(&["<", ">", "^", "="]));
    }
    if kind != 's' {
        if r.chance(35) {
            s.push_str(r.pick(&["+", "-", " "]));
        }
        if r.chance(15) {
            s.push('#');
        }
        if r.chance(20) {
            s.push('0');
        }
    }
    if r.chance(55) {
        s.push_str(&r.range(1, 14).to_string());
    }
    if kind != 's' && r.chance(25) {
        s.push_str(r.pick(&[",", "_"]));
    }
    if r.chance(45) {
        s.push('.');
        s.push_str(&r.range(0, 12).to_string());
    }
    match kind {
        'i' => {
            if r.chance(85) {
                s.push_str(r.pick(&["d", "d", "b", "o", "x", "X", "c", "n", "e", "f", "g", "%"]));
            }
        }
        'f' => {
            if r.chance(85) {
                s.push_str(r.pick(&["f", "F", "e", "E", "g", "G", "n", "%"]));
            }
        }
        _ => {
            if r.chance(60) {
                s.push('s');
            }
        }
    }
    s
}

fn gen_numfmt(r: &mut Rng) -> String {
    let mut out = String::new();
    for _ in 0..6 {
        let (v, k) = match r.below(3) {
            0 => (int(r).to_string(), 'i'),
            1 => (float(r).to_string(), 'f'),
            _ => (string(r).to_string(), 's'),
        };
        let sp = spec(r, k);
        match r.below(4) {
            0 => {
                out += &tpl(
                    "T(lambda: format(@V@, @S@))\n",
                    &[("V", &v), ("S", &format!("{sp:?}"))],
                )
            }
            1 => {
                out += &tpl(
                    "T(lambda: '{:@S@}'.format(@V@))\n",
                    &[("V", &v), ("S", &sp)],
                )
            }
            2 => out += &tpl("T(lambda: f'{@V@:@S@}')\n", &[("V", &v), ("S", &sp)]),
            _ => {
                out += &tpl(
                    "T(lambda: (@V@).__format__(@S@))\n",
                    &[("V", &v), ("S", &format!("{sp:?}"))],
                )
            }
        }
    }
    out
}

fn gen_floatrepr(r: &mut Rng) -> String {
    let mut out = String::new();
    let bits = r.next();
    let bits2 = r.next() & 0x400f_ffff_ffff_ffff;
    out += &tpl(
        "import struct\nfor _b in (@A@, @B@):\n    _f = struct.unpack('<d', struct.pack('<Q', _b))[0]\n    print(repr(_f), str(_f), float.hex(_f) if _f == _f else 'nan')\n    print('%r %s %.3f %g %e %.17g %10.4g' % (_f, _f, _f, _f, _f, _f, _f))\n    T(lambda: round(_f, 3))\n    T(lambda: round(_f))\n    T(lambda: int(_f))\n    T(lambda: _f.as_integer_ratio())\n    T(lambda: format(_f, '.0e'))\n    T(lambda: divmod(_f, 0.7))\n    T(lambda: _f % -1.3)\n    T(lambda: _f // 0.3)\n    T(lambda: float(repr(_f)) == _f)\n",
        &[("A", &bits.to_string()), ("B", &bits2.to_string())],
    );
    for _ in 0..3 {
        let a = float(r);
        let n = r.range(-3, 6);
        out += &tpl(
            "T(lambda: round(@A@, @N@))\nT(lambda: (@A@).is_integer())\nT(lambda: (@A@) ** 0.5)\nT(lambda: divmod(@A@, 0.1))\nT(lambda: str(@A@) + repr(-@A@))\n",
            &[("A", a), ("N", &n.to_string())],
        );
    }
    out
}

fn gen_bigint(r: &mut Rng) -> String {
    let mut out = String::new();
    for _ in 0..5 {
        let a = int(r);
        let b = int(r);
        let op = r.pick(&[
            "+", "-", "*", "//", "%", "&", "|", "^", "**", "<<", ">>", "/", "<", "==",
        ]);
        let (a, b) = match op {
            "**" => (a.to_string(), r.range(-3, 40).to_string()),
            "<<" | ">>" => (a.to_string(), r.range(-2, 130).to_string()),
            _ => (a.to_string(), b.to_string()),
        };
        out += &tpl(
            "T(lambda: (@A@) @O@ (@B@))\n",
            &[("A", &a), ("B", &b), ("O", op)],
        );
    }
    let a = int(r);
    let b = int(r);
    let m = int(r);
    out += &tpl(
        "T(lambda: divmod(@A@, @B@))\nT(lambda: pow(@A@, @S@, @M@))\nT(lambda: (@A@).bit_length())\nT(lambda: (@A@).bit_count())\nT(lambda: hex(@A@) + oct(@A@) + bin(@A@))\nT(lambda: (@A@).to_bytes(16, 'big', signed=True))\nT(lambda: int.from_bytes((@A@ % 2**64).to_bytes(8, 'little'), 'little'))\nT(lambda: __import__('math').isqrt(abs(@A@)))\nT(lambda: __import__('math').gcd(@A@, @B@))\nT(lambda: __import__('math').lcm(@A@, @B@))\nT(lambda: float(@A@))\nT(lambda: (@A@) / (@B@))\nT(lambda: round(@A@, -3))\nT(lambda: int(str(@A@), 10) == @A@)\nT(lambda: int(format(@A@, 'x'), 16) == @A@)\nT(lambda: ~(@A@))\nT(lambda: -(@A@) ** 2)\nT(lambda: (@A@) < float(@B@))\nT(lambda: hash(@A@))\n",
        &[("A", a), ("B", b), ("M", m), ("S", &r.range(0, 50).to_string())],
    );
    out += "T(lambda: str(3**4000)[-20:])\nT(lambda: len(str(7**9000)))\nT(lambda: int('9'*5000))\nT(lambda: int('1_000'))\nT(lambda: int(' 12 '))\nT(lambda: int('0b11', 0))\nT(lambda: int('z', 36))\nT(lambda: int('12', 1))\n";
    out
}

fn gen_strmeth(r: &mut Rng) -> String {
    let mut out = String::new();
    for _ in 0..6 {
        let s = string(r);
        let m = r.pick(&[
            "upper()",
            "lower()",
            "title()",
            "capitalize()",
            "swapcase()",
            "casefold()",
            "strip()",
            "lstrip()",
            "rstrip()",
            "split()",
            "splitlines()",
            "splitlines(True)",
            "isalpha()",
            "isdigit()",
            "isnumeric()",
            "isdecimal()",
            "isalnum()",
            "isidentifier()",
            "isprintable()",
            "isspace()",
            "istitle()",
            "isupper()",
            "islower()",
            "isascii()",
            "expandtabs(3)",
            "zfill(9)",
            "center(15, '*')",
            "ljust(12, '-')",
            "rjust(12)",
            "encode()",
            "encode('ascii', 'replace')",
            "encode('latin-1', 'backslashreplace')",
            "encode('utf-16')",
            "partition('b')",
            "rpartition(',')",
            "split(',')",
            "rsplit(',', 1)",
            "split(None, 1)",
            "find('b')",
            "rfind('a')",
            "count('a')",
            "count('')",
            "index('z')",
            "replace('a', 'XY')",
            "replace('', '-')",
            "replace('a', '', 1)",
            "startswith(('a', 'H'))",
            "endswith('d')",
            "strip('a ')",
            "removeprefix('ab')",
            "removesuffix('c')",
            "translate({97: 'Z', 98: None})",
            "__len__()",
            "__contains__('a')",
            "join(['1', '2', '3'])",
            "join('xyz')",
            "format_map({})",
            "__mod__(())",
            "__mul__(3)",
            "__getitem__(1)",
            "__getitem__(-1)",
            "__getitem__(slice(None, None, -2))",
            "__rmul__(0)",
            "ljust(3.0)",
            "center(5, 'ab')",
            "split('')",
            "find(1)",
            "join([1])",
            "zfill(-1)",
            "maketrans('ab', 'xy')",
            "splitlines(keepends=True)",
            "isdigit().__class__.__name__",
        ]);
        out += &tpl("T(lambda: (@S@).@M@)\n", &[("S", s), ("M", m)]);
    }
    let a = string(r);
    let b = string(r);
    out += &tpl(
        "T(lambda: (@A@) + (@B@))\nT(lambda: (@A@) < (@B@))\nT(lambda: sorted([@A@, @B@, 'a', 'B']))\nT(lambda: (@A@) in (@B@))\nT(lambda: ord((@A@)[0]))\nT(lambda: repr(@A@) + ascii(@B@))\nT(lambda: ('%s|%r|%a' % (@A@, @A@, @B@)))\nT(lambda: ('%5.2s|%-6s|' % (@A@, @B@)))\nT(lambda: (@A@).join([@B@, @B@]))\nT(lambda: len(@A@.encode('utf-8')))\n",
        &[("A", a), ("B", b)],
    );
    out
}

fn gen_printf(r: &mut Rng) -> String {
    let mut out = String::new();
    for _ in 0..6 {
        let conv = r.pick(&[
            "d", "i", "u", "x", "X", "o", "e", "E", "f", "F", "g", "G", "c", "r", "s", "a", "%",
        ]);
        let flags = r.pick(&["", "-", "+", " ", "0", "#", "-+", "0+", "# "]);
        let width = r.pick(&["", "5", "12", "*"]);
        let prec = r.pick(&["", ".0", ".3", ".10", ".*"]);
        let arg = match conv {
            "c" => r.pick(&["65", "'z'", "0x1F600", "'ab'", "-1"]).to_string(),
            "s" | "r" | "a" => string(r).to_string(),
            "%" => "()".to_string(),
            "d" | "i" | "u" | "x" | "X" | "o" => {
                if r.chance(25) {
                    float(r).to_string()
                } else {
                    int(r).to_string()
                }
            }
            _ => {
                if r.chance(25) {
                    int(r).to_string()
                } else {
                    float(r).to_string()
                }
            }
        };
        let mut args = arg;
        if width == "*" {
            args = format!("7, {args}");
        }
        if prec == ".*" {
            args = if width == "*" {
                format!("7, 3, {}", &args[3..])
            } else {
                format!("3, {args}")
            };
        }
        out += &tpl(
            "T(lambda: '%@F@@W@@P@@C@' % (@A@,))\n",
            &[
                ("F", flags),
                ("W", width),
                ("P", prec),
                ("C", conv),
                ("A", &args),
            ],
        );
    }
    out += "T(lambda: '%(a)s-%(b)05d' % {'a': 1, 'b': 42})\nT(lambda: '%s %s' % (1,))\nT(lambda: '%s' % (1, 2))\nT(lambda: '%d' % 'x')\nT(lambda: '%z' % 1)\nT(lambda: '%' % ())\nT(lambda: '%(a)s' % (1,))\nT(lambda: '%s' % {'a': 1})\nT(lambda: '%.2f%%' % 12.345)\n";
    out
}

fn gen_fstring(r: &mut Rng) -> String {
    let mut out = String::new();
    out += &tpl(
        "x = @I@\ny = @F@\ns = @S@\nw = @W@\np = @P@\nd = {'k': [1, 2, {'z': s}], 3: x}\nT(lambda: f'{x=}|{y=!r}|{s=:>10}|{w=}')\nT(lambda: f'{x:{w}}|{y:{w}.{p}f}|{s:^{w}}|{s!r:>{w}}|{s!a:.{p}}')\nT(lambda: f'{d[\"k\"][2][\"z\"]}|{d[3]}|{{literal}}|{x!s}|{x:#x}|{x:+,}')\nT(lambda: f'{x if x else y}|{[i*2 for i in range(3)]}|{ {1:2}[1] }|{x:{\"<\"}{w}}')\nT(lambda: f'{s!r:*^{w + 4}}|{len(s):03d}|{\"nested\" f\"{x}\"}')\nT(lambda: f'''{{{x}}}{{{{''')\nT(lambda: f'{x!r:>{w}} {x:=^{w}}')\n",
        &[("I", int(r)), ("F", float(r)), ("S", string(r)), ("W", &r.range(0, 12).to_string()), ("P", &r.range(0, 6).to_string())],
    );
    out
}

fn gen_slicing(r: &mut Rng) -> String {
    let mut out = String::new();
    let seqs = [
        "list(range(10))",
        "'abcdefghij'",
        "tuple(range(7))",
        "b'0123456789'",
        "bytearray(b'abcdef')",
        "[]",
    ];
    let idx = |r: &mut Rng| -> String {
        match r.below(6) {
            0 => "None".to_string(),
            1 => r.range(-14, 14).to_string(),
            2 => r.pick(&["2**70", "-2**70", "10**30"]).to_string(),
            _ => r.range(-4, 12).to_string(),
        }
    };
    for _ in 0..5 {
        let s = seqs[r.below(seqs.len())];
        let (a, b) = (idx(r), idx(r));
        let st = if r.chance(60) {
            idx(r)
        } else {
            "None".to_string()
        };
        out += &tpl(
            "T(lambda: (@S@)[@A@:@B@:@C@])\nT(lambda: slice(@A@, @B@, @C@).indices(len(@S@)))\n",
            &[("S", s), ("A", &a), ("B", &b), ("C", &st)],
        );
    }
    let (a, b, c) = (idx(r), idx(r), r.range(-3, 3).to_string());
    out += &tpl(
        "L = list(range(10))\nT(lambda: L.__setitem__(slice(@A@, @B@, @C@), [91, 92, 93]))\nprint(L)\nL = list(range(10))\nT(lambda: L.__delitem__(slice(@A@, @B@, @C@)))\nprint(L)\nL = list(range(10))\nL[@A@:@B@] = 'xy'\nprint(L)\nL = list(range(10))\nT(lambda: L.__setitem__(slice(None, None, 2), [0]))\nT(lambda: L[1.5])\nT(lambda: L['a'])\nT(lambda: 'abc'[5])\nT(lambda: slice(1, 2, 0).indices(5))\nT(lambda: range(10)[slice(@A@, @B@, @C@)])\nT(lambda: repr(slice(@A@, @B@)))\n",
        &[("A", &a), ("B", &b), ("C", &c)],
    );
    out
}

fn gen_containers(r: &mut Rng) -> String {
    let mut out = String::new();
    let n = r.range(3, 9);
    out += &tpl(
        "L = [(i * @M@) % @K@ for i in range(@N@)]\nD = {}\nfor i, v in enumerate(L):\n    D[v] = i\n    if i % 3 == 2:\n        D.pop(L[i - 1], None)\n        D[L[i - 1]] = 'again'\nprint(L, D, list(D.items()))\nS = set(L) | {-1, 100, 'a', (1, 2)}\nprint(sorted(map(str, S)), len(S))\nT(lambda: (D.popitem(), D))\nT(lambda: D.setdefault(5, []))\nT(lambda: list(reversed(D)))\nT(lambda: D | {'new': 1})\nT(lambda: dict.fromkeys(L, 0))\nT(lambda: {**D, 'q': 0})\nT(lambda: L.index(L[-1], 1, 3))\nT(lambda: L.remove(12345))\nT(lambda: L.pop(@N@ + 5))\nT(lambda: [L.insert(-@K@, 'i'), L][1])\nT(lambda: L.count(0))\nT(lambda: sorted(L, key=lambda v: -v, reverse=True))\nT(lambda: sorted(L + [0.0, False, True]))\nT(lambda: L * -2)\nT(lambda: L.copy() == L[:])\nT(lambda: {1: 'a', 1.0: 'b', True: 'c'})\nT(lambda: {0.0: 1, -0.0: 2, float('nan'): 3})\nT(lambda: D.keys() & {@K@, 1})\nT(lambda: sorted(D.items() ^ {(1, 'x')}, key=repr))\nT(lambda: min(L, key=lambda v: v % 3))\nT(lambda: max(L, default=None) if L else 0)\nT(lambda: {1, 2, 3} < {1, 2, 3, 4})\nT(lambda: frozenset([3, 1, 2]) | {9})\nT(lambda: {i: i for i in range(@N@)}.get(@K@))\nT(lambda: sorted(set(range(@N@)) ^ set(range(@K@))))\nT(lambda: list(set(range(-2, @N@ * 13, 5))))\nT(lambda: range(-3, 14, 3)[@N@:@K@:@M@])\nT(lambda: list({'a', 'bb', 'ccc', 'dddd'}))\nT(lambda: [].pop())\nT(lambda: {}.popitem())\nT(lambda: set().pop())\nT(lambda: {[]: 1})\nT(lambda: {1: 2}[3])\n",
        &[("M", &r.range(1, 9).to_string()), ("K", &r.range(2, 11).to_string()), ("N", &n.to_string())],
    );
    out
}

fn gen_sorting(r: &mut Rng) -> String {
    let n = r.range(5, 16);
    tpl(
        "import functools, heapq, bisect\nrecs = [(i, (i * @A@) % @B@, 'k%d' % ((i * @A@) % 3)) for i in range(@N@)]\nT(lambda: sorted(recs, key=lambda t: t[1]))\nT(lambda: sorted(recs, key=lambda t: t[2], reverse=True))\nT(lambda: sorted(recs, key=lambda t: (t[2], -t[1])))\nT(lambda: sorted(recs, key=functools.cmp_to_key(lambda a, b: (a[1] > b[1]) - (a[1] < b[1]))))\nT(lambda: sorted(recs, key=lambda t: t[1], reverse=True)[:5])\nT(lambda: sorted([3, 'a', 1]))\nT(lambda: sorted([None, 1]))\nT(lambda: sorted([[1], [0, 5], []]))\nT(lambda: sorted([1.5, 1, True, 2**70, -0.0, 0]))\nT(lambda: sorted(['b', 'B', 'a', 'A', 'ä', 'z']))\nT(lambda: sorted({'b': 1, 'a': 2}.items(), key=lambda kv: kv[1]))\nT(lambda: sorted(range(@N@), key=lambda v: (v * @A@) % 4))\nT(lambda: heapq.nlargest(3, recs, key=lambda t: t[1]))\nT(lambda: heapq.nsmallest(3, recs, key=lambda t: t[1]))\nT(lambda: heapq.merge([1, 4, 7], [2, 4, 8], [0, 9]) and list(heapq.merge([1, 4, 7], [2, 4, 8], [0, 9])))\nT(lambda: bisect.bisect_left([1, 2, 2, 2, 3], 2))\nT(lambda: bisect.bisect_right([1, 2, 2, 2, 3], 2))\nT(lambda: [bisect.insort(L2 := [1, 3, 5], 4), L2][1])\nT(lambda: min(recs, key=lambda t: t[2]))\nT(lambda: max(recs, key=lambda t: t[2]))\nT(lambda: sorted(recs, key=len))\nT(lambda: sorted([1, 2], key=1))\nT(lambda: sorted([2, 1], 1))\nL = [3, 1, 2]\nT(lambda: L.sort(key=lambda v: (L.append(9) if len(L) < 4 else 0, v)))\nprint(L)\nT(lambda: sorted(iter('hello'), reverse=True))\n",
        &[("A", &r.range(1, 9).to_string()), ("B", &r.range(2, 8).to_string()), ("N", &n.to_string())],
    )
}

fn gen_comp_scope(r: &mut Rng) -> String {
    let k = r.range(2, 5);
    tpl(
        "x = 'global'\nclass K:\n    x = 'cls'\n    ys = [x for _ in range(2)]\n    zs = [i for i in range(@K@) if x]\n    try:\n        ws = [x + y for y in 'ab']\n    except NameError as e:\n        ws = str(e)\nprint(K.ys, K.zs, K.ws)\nfs = [lambda: i for i in range(@K@)]\nprint([f() for f in fs])\nfs = [lambda i=i: i for i in range(@K@)]\nprint([f() for f in fs])\ndef g():\n    acc = []\n    for i in range(@K@):\n        acc.append(lambda: i)\n    return [f() for f in acc]\nprint(g())\ni = 'outer'\n_ = [i for i in range(3)]\nprint(i)\nprint([(j := n) for n in range(@K@)], j)\nprint({n: [m for m in range(n)] for n in range(@K@)})\nprint([[a * b for a in range(3)] for b in range(@K@)])\nT(lambda: [q for q in range(3)] and q)\nprint(list(n for n in range(@K@)) , sum(n * n for n in range(@K@)))\ng2 = (n * 2 for n in range(@K@))\nprint(next(g2), list(g2), list(g2))\nlst = [1, 2, 3]\ngen = (v for v in lst)\nlst = [9]\nprint(list(gen))\nprint([x for x in range(3) for x in range(x)])\nprint([c for c in 'abc' if c != (z := 'b')], z)\ndef h():\n    return [locals().keys() for _ in range(1)][0]\nprint(sorted(h()))\n",
        &[("K", &k.to_string())],
    )
}

fn gen_generators(r: &mut Rng) -> String {
    let n = r.range(1, 5);
    tpl(
        "log = []\ndef sub(n):\n    try:\n        for i in range(n):\n            got = yield i\n            log.append(('sub', got))\n        return 'sub-done'\n    finally:\n        log.append('sub-finally')\ndef outer(n):\n    r = yield from sub(n)\n    log.append(('r', r))\n    r2 = yield from iter([10, 20])\n    log.append(('r2', r2))\n    return 'outer'\ng = outer(@N@)\nprint(next(g))\nprint(g.send('s1'))\nT(lambda: g.throw(ValueError('boom')))\nprint(log)\ng = outer(@N@)\nT(lambda: list(g))\nprint(log, g.gi_frame is None)\nT(lambda: next(g))\ng = outer(@N@)\nnext(g)\nprint(g.close(), log[-1])\nT(lambda: next(g))\ndef gen2():\n    try:\n        yield 1\n    except GeneratorExit:\n        print('exit')\n        yield 2\ng = gen2(); next(g)\nT(lambda: g.close())\ndef gen3():\n    x = yield\n    while True:\n        x = yield x * 2\ng = gen3()\nT(lambda: g.send(5))\nprint(next(g), g.send(4), g.send(1))\ndef gen4():\n    yield from range(3)\n    return (yield)\ng = gen4()\nprint(list(zip(g, 'ab')))\nprint(next(g))\nT(lambda: g.send('end'))\ndef gen5():\n    raise StopIteration('x')\n    yield\nT(lambda: list(gen5()))\ndef gen6():\n    yield 1\n    1/0\nT(lambda: [*gen6()])\nT(lambda: next(gen6(), 'dflt'))\ng = (i for i in range(3))\nT(lambda: g.send(1))\nT(lambda: (lambda: (yield))().send(None))\nimport inspect\nprint(inspect.getgeneratorstate(outer(1)))\n",
        &[("N", &n.to_string())],
    )
}

fn gen_closures(r: &mut Rng) -> String {
    let n = r.range(1, 5);
    tpl(
        "def counter(start=@N@):\n    n = start\n    def inc(by=1):\n        nonlocal n\n        n += by\n        return n\n    def get():\n        return n\n    return inc, get\ni1, g1 = counter()\ni2, g2 = counter(10)\nprint(i1(), i1(5), g1(), i2(), g2())\nprint(i1.__closure__[0].cell_contents, i1.__code__.co_freevars, g1.__code__.co_freevars)\ndef mk():\n    fs = []\n    for k in range(@N@):\n        def f(k=k, *a, **kw):\n            return k\n        fs.append(f)\n    return fs\nprint([f() for f in mk()])\ndef bad():\n    print(zz)\n    zz = 1\nT(bad)\ndef bad2():\n    def inner():\n        nonlocal q\n    q = 1\n    return inner\ndef outer():\n    v = 1\n    def a():\n        nonlocal v\n        v = 2\n    def b():\n        return v\n    a()\n    return b()\nprint(outer())\ndef free():\n    def f():\n        return late\n    T(f)\n    late = 'bound'\n    return f()\nprint(free())\ndef deleter():\n    d = 1\n    def f():\n        return d\n    del d\n    return f\nT(deleter())\nglobal_v = 1\ndef setg():\n    global global_v\n    global_v += 1\nsetg(); print(global_v)\nT(lambda: (lambda x, /, y, *, z=3: (x, y, z))(1, 2, z=@N@))\nT(lambda: (lambda x, /: x)(x=1))\nprint(counter.__defaults__, i1.__kwdefaults__, i1.__qualname__, mk.__code__.co_varnames)\nclass C:\n    def m(self):\n        return __class__\n    y = 5\n    def n(self, v=y):\n        return v\nprint(C().m().__name__, C().n())\n",
        &[("N", &n.to_string())],
    )
}

fn gen_exc(r: &mut Rng) -> String {
    let n = r.range(0, 3);
    tpl(
        "import traceback\ndef show(e):\n    return ''.join(traceback.format_exception_only(type(e), e)).strip()\ndef a():\n    try:\n        {1: 2}[@N@ + 5]\n    except KeyError as e:\n        raise ValueError('wrapped') from e\ndef b():\n    try:\n        a()\n    except ValueError:\n        1 / 0\ntry:\n    b()\nexcept ZeroDivisionError as e:\n    print(show(e), '|', show(e.__context__), '|', show(e.__context__.__cause__), e.__suppress_context__)\ntry:\n    try:\n        raise OSError(2, 'nofile', 'f.txt')\n    except OSError as e:\n        raise RuntimeError('r') from None\nexcept RuntimeError as e:\n    print(repr(e), e.__suppress_context__, repr(e.__context__), e.__cause__)\ndef fin():\n    try:\n        return 'try'\n    finally:\n        print('fin')\nprint(fin())\ndef fin2():\n    for i in range(3):\n        try:\n            continue\n        finally:\n            print('f', i)\n    try:\n        raise KeyError('k')\n    finally:\n        return 'swallowed'\nprint(fin2())\nT(lambda: [][@N@ + 1])\nT(lambda: int('x'))\nT(lambda: None.foo)\nT(lambda: (1).real.nope)\nT(lambda: len(5))\nT(lambda: 1 + 'a')\nT(lambda: 'a' + 1)\nT(lambda: [1] + (2,))\nT(lambda: {}.nope)\nT(lambda: undefined_name)\nT(lambda: (1, 2)[5])\nT(lambda: 'x'.nope())\nT(lambda: abs('a'))\nT(lambda: iter(5))\nT(lambda: next(5))\nT(lambda: 5())\nT(lambda: sum(['a']))\nT(lambda: max([]))\nT(lambda: [1, 2][None])\nT(lambda: 1 < 'a')\nT(lambda: -'a')\nT(lambda: float('1_0x'))\nT(lambda: chr(-1))\nT(lambda: ord('ab'))\nT(lambda: 'abc'.index('z'))\nT(lambda: [1, 2].index(7))\nT(lambda: (lambda x: x)())\nT(lambda: (lambda x: x)(1, 2))\nT(lambda: (lambda x: x)(y=1))\nT(lambda: dict(a=1, **{'a': 2}))\nT(lambda: int(None))\nT(lambda: 2 ** 10000 * 1.0)\nT(lambda: 1 % 0)\nT(lambda: 1.0 // 0)\nT(lambda: divmod(1, 0.0))\nT(lambda: round(float('nan')))\nT(lambda: int(float('inf')))\nT(lambda: a.__nope__)\nT(lambda: type('A', (), {}).x)\nT(lambda: ValueError('a', 1).args)\nT(lambda: str(KeyError('a', 'b')))\nT(lambda: repr(OSError(2, 'x')))\nT(lambda: str(OSError(2, 'x', 'f')))\nT(lambda: [str(UnicodeDecodeError('utf-8', b'a\\xff', 1, 2, 'bad'))])\nT(lambda: StopIteration(3).value)\ndef raiser():\n    raise ExceptionGroup('eg', [ValueError(1), TypeError(2)])\ntry:\n    raiser()\nexcept* ValueError as e:\n    print('V', repr(e))\nexcept* TypeError as e:\n    print('T', repr(e))\nclass MyE(Exception):\n    def __init__(self, a, b):\n        super().__init__(a)\n        self.b = b\nT(lambda: (_ for _ in ()).throw(MyE(1, 2)))\nprint(repr(MyE(1, 2)), MyE(1, 2).args)\ndef w():\n    try:\n        raise ValueError('a')\n    except ValueError:\n        try:\n            raise TypeError('b')\n        except TypeError as e2:\n            return repr(e2.__context__)\nprint(w())\nT(lambda: (_ for _ in ()).throw(ValueError))\nT(lambda: exec('raise SystemExit(3)'))\nT(lambda: BaseException().with_traceback(5))\n",
        &[("N", &n.to_string())],
    )
}

fn gen_classes(r: &mut Rng) -> String {
    // A random DAG of up to 6 classes; each base is an earlier class, so
    // inconsistent linearizations arise naturally and must raise identically.
    let n = r.range(3, 6) as usize;
    let names = ["A", "B", "C", "D", "E", "F"];
    let mut out = String::new();
    for i in 0..n {
        let mut bs: Vec<&str> = Vec::new();
        if i > 0 {
            let k = if r.chance(50) { 1 } else { r.range(1, 3) };
            for _ in 0..k {
                let c = names[r.below(i)];
                if !bs.contains(&c) {
                    bs.push(c);
                }
            }
        }
        let b = if bs.is_empty() {
            "object".to_string()
        } else {
            bs.join(", ")
        };
        out += &tpl(
            "def _w_@N@(self):\n    return '@N@' + (super(@N@, self).who() if hasattr(super(@N@, self), 'who') else '')\ntry:\n    @N@ = type('@N@', (@B@,), {'who': _w_@N@})\n    print('@N@', [c.__name__ for c in @N@.__mro__])\n    print(@N@().who())\nexcept TypeError as e:\n    print('@N@', 'TypeError', e)\n    @N@ = None\n",
            &[("N", names[i]), ("B", &b)],
        );
    }
    out += &tpl(
        "T(lambda: [c.__name__ for c in A.__subclasses__()])\nT(lambda: issubclass(@L@, A))\nT(lambda: isinstance(@L@(), A))\nT(lambda: @L@.mro()[-1].__name__)\n",
        &[("L", names[n - 1])],
    );
    out
}

fn gen_dunder(r: &mut Rng) -> String {
    let a = r.range(-5, 9);
    let b = r.range(-5, 9);
    tpl(
        "class V:\n    def __init__(self, v): self.v = v\n    def __repr__(self): return 'V(%r)' % (self.v,)\n    def __eq__(self, o): return isinstance(o, V) and self.v == o.v\n    def __hash__(self): return hash(self.v)\n    def __lt__(self, o): return self.v < (o.v if isinstance(o, V) else o)\n    def __add__(self, o): return V(self.v + (o.v if isinstance(o, V) else o))\n    def __radd__(self, o): return V(o + self.v)\n    def __iadd__(self, o): self.v += 1000; return self\n    def __neg__(self): return V(-self.v)\n    def __bool__(self): return bool(self.v)\n    def __len__(self): return abs(self.v)\n    def __index__(self): return self.v\n    def __int__(self): return self.v * 2\n    def __float__(self): return self.v / 2\n    def __format__(self, s): return 'F<%s:%s>' % (self.v, s)\n    def __contains__(self, x): return x == self.v\n    def __getitem__(self, i): return ('item', i)\n    def __call__(self, *a, **k): return (a, tuple(sorted(k)))\n    def __enter__(self): print('enter'); return self\n    def __exit__(self, t, e, tb): print('exit', t.__name__ if t else None); return t is KeyError\n    def __missing__(self, k): return 'missing'\n    def __round__(self, n=None): return ('round', n)\n    def __divmod__(self, o): return ('divmod', o)\n    def __matmul__(self, o): return 'matmul'\n    def __getattr__(self, n): return 'ga:' + n\nx, y = V(@A@), V(@B@)\nprint(x + y, x + 3, 3 + x, -x, bool(x), len(y), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10][y.v % 10], int(x), float(x))\nprint(f'{x}|{x:>5}|{x!r}|{x!s}', format(y, 'zz'), '%s %r %d' % (x, x, x))\nprint(x == y, x != y, x == V(@A@), {x: 1}.get(V(@A@)), x < y, y > x, sorted([x, y]), max(x, y))\nT(lambda: x <= y)\nprint(@A@ in x, x[1], x[1:2], x['k'], x(1, 2, k=3), round(x), round(x, 2), divmod(x, 3), x @ 1, x.anything)\nz = x\nz += 1\nprint(z is x, x)\nwith x as t:\n    pass\nwith x:\n    raise KeyError('swallowed')\nT(lambda: [1, 2, 3][V(1)])\nT(lambda: 'abc' * V(2))\nT(lambda: range(V(3)))\nT(lambda: hex(V(255)))\nT(lambda: x.__dict__)\nT(lambda: vars(x))\nT(lambda: sorted([V(2), V(1)], reverse=True))\nT(lambda: {V(1), V(1), V(2)})\nT(lambda: [V(1)] == [V(1)])\nT(lambda: (V(1),) < (V(2),))\nT(lambda: abs(x))\nT(lambda: x * 2)\nT(lambda: 2 ** x)\nT(lambda: iter(x))\nT(lambda: reversed(x))\nclass NoEq:\n    pass\nT(lambda: NoEq() == NoEq())\nT(lambda: hash(NoEq()) == hash(NoEq()))\nT(lambda: NoEq() < NoEq())\nclass Eq:\n    def __eq__(self, o): return True\nT(lambda: hash(Eq()))\nT(lambda: Eq() in {Eq()})\nclass Seq:\n    def __getitem__(self, i):\n        if i > 3: raise IndexError\n        return i * i\nT(lambda: list(Seq()))\nT(lambda: 4 in Seq())\nT(lambda: sum(Seq()))\nT(lambda: dict(zip(Seq(), 'abcd')))\nT(lambda: str(NoEq()).startswith('<'))\n",
        &[("A", &a.to_string()), ("B", &b.to_string())],
    )
}

fn gen_descr(r: &mut Rng) -> String {
    let k = r.range(1, 5);
    tpl(
        "class D:\n    def __set_name__(self, owner, name): self.name = name; print('set_name', owner.__name__, name)\n    def __get__(self, obj, typ=None):\n        print('get', self.name, obj is None, typ.__name__)\n        return self if obj is None else obj.__dict__.get('_' + self.name, @K@)\n    def __set__(self, obj, v):\n        print('set', self.name, v); obj.__dict__['_' + self.name] = v\n    def __delete__(self, obj):\n        print('del', self.name); del obj.__dict__['_' + self.name]\nclass ND:\n    def __get__(self, obj, typ=None): return 'nd'\nclass P:\n    a = D()\n    b = ND()\n    @property\n    def p(self): return 'prop'\n    @p.setter\n    def p(self, v): print('p set', v)\n    @staticmethod\n    def s(x): return ('s', x)\n    @classmethod\n    def c(cls, x): return (cls.__name__, x)\n    __slots__ = ('__dict__',)\nclass Q(P): pass\no = Q()\nprint(o.a, Q.a is P.__dict__['a'])\no.a = 7\nprint(o.a)\ndel o.a\nprint(o.a)\nT(lambda: o.__delattr__('a'))\nprint(o.b)\no.b = 'shadow'\nprint(o.b, o.__dict__)\no.p = 3\nT(lambda: setattr(o, 'p', 1) or o.p)\nprint(o.s(1), Q.s(2), o.c(3), Q.c(4), o.p)\nT(lambda: P.p.fget(o))\nT(lambda: P.__dict__['p'].fset)\nT(lambda: type(P.__dict__['s']).__name__)\nclass R:\n    @property\n    def ro(self): return 1\nT(lambda: setattr(R(), 'ro', 2))\nT(lambda: delattr(R(), 'ro'))\nclass S:\n    __slots__ = ('x',)\nT(lambda: S().x)\nT(lambda: setattr(S(), 'y', 1))\nclass M(type):\n    def __getattr__(cls, n): return 'meta:' + n\n    def __call__(cls, *a): return ('called', a)\n    def __repr__(cls): return '<M %s>' % cls.__name__\nclass UsesM(metaclass=M): pass\nprint(UsesM.zzz, UsesM(1, 2), UsesM)\nclass Init:\n    def __init_subclass__(cls, tag=None, **kw):\n        print('init_subclass', cls.__name__, tag, kw)\nclass Sub(Init, tag='t'): pass\nT(lambda: type('X', (Init,), {}, tag=1, z=2))\nclass G:\n    def __class_getitem__(cls, i): return (cls.__name__, i)\nprint(G[int], G[1, 2])\n",
        &[("K", &k.to_string())],
    )
}

fn gen_stdlib_iter(r: &mut Rng) -> String {
    let n = r.range(2, 5);
    tpl(
        "import itertools as it, functools as ft, collections as co, operator as op\nA = list(range(@N@ + 2))\nT(lambda: list(it.combinations(A, 2)))\nT(lambda: list(it.permutations(A[:@N@], 2)))\nT(lambda: list(it.combinations_with_replacement('abc', 2)))\nT(lambda: list(it.product('ab', repeat=2)))\nT(lambda: list(it.product(A[:2], 'xy', [None])))\nT(lambda: [(k, list(g)) for k, g in it.groupby('aabbbcaa')])\nT(lambda: [(k, list(g)) for k, g in it.groupby(A, key=lambda v: v // 2)])\nT(lambda: list(it.accumulate(A, op.mul)))\nT(lambda: list(it.accumulate(A, initial=100)))\nT(lambda: list(it.chain.from_iterable([A, 'ab'])))\nT(lambda: list(it.islice(it.cycle('xy'), 5)))\nT(lambda: list(it.islice(A, 1, None, 2)))\nT(lambda: list(it.zip_longest(A, 'ab', fillvalue='-')))\nT(lambda: list(it.starmap(pow, [(2, 3), (3, 2)])))\nT(lambda: list(it.takewhile(lambda v: v < 3, A)))\nT(lambda: list(it.dropwhile(lambda v: v < 3, A)))\nT(lambda: list(it.compress('abcdef', [1, 0, 1, 0, 1])))\nT(lambda: list(it.pairwise(A)))\nT(lambda: list(it.batched(A, 3)))\nT(lambda: list(it.filterfalse(None, [0, 1, '', 'a'])))\nT(lambda: [list(t) for t in it.tee(A, 3)])\nT(lambda: list(it.repeat('r', 3)))\nT(lambda: list(it.islice(it.count(5, -2), 4)))\nT(lambda: list(it.islice(A, -1)))\nT(lambda: ft.reduce(op.add, A))\nT(lambda: ft.reduce(op.add, []))\nT(lambda: ft.reduce(op.add, [], 'init'))\nT(lambda: ft.partial(pow, 2)(10))\nT(lambda: ft.partial(int, base=2)('101'))\nT(lambda: repr(ft.partial(int, base=2)))\n@ft.lru_cache(maxsize=2)\ndef fib(n): return n if n < 2 else fib(n - 1) + fib(n - 2)\nprint(fib(@N@ * 5), fib.cache_info())\nfib.cache_clear()\nprint(fib.cache_info())\n@ft.total_ordering\nclass TO:\n    def __init__(self, v): self.v = v\n    def __eq__(self, o): return self.v == o.v\n    def __lt__(self, o): return self.v < o.v\nT(lambda: (TO(1) <= TO(2), TO(3) >= TO(2), TO(1) > TO(2)))\n@ft.singledispatch\ndef sd(x): return 'obj'\n@sd.register\ndef _(x: int): return 'int'\n@sd.register(str)\ndef _(x): return 'str'\nT(lambda: (sd(1), sd('a'), sd(1.0), sd(True)))\nC = co.Counter('abracadabra')\nT(lambda: C.most_common(3))\nT(lambda: sorted(C.items()))\nT(lambda: C + co.Counter('aaz') - co.Counter('b'))\nT(lambda: list(C.elements()))\nd = co.OrderedDict.fromkeys('abc'); d.move_to_end('a')\nT(lambda: (list(d), d.popitem(last=False)))\ndq = co.deque(range(5), maxlen=@N@ + 1); dq.rotate(2); dq.appendleft('x')\nT(lambda: (list(dq), dq.maxlen, dq))\ndd = co.defaultdict(list); dd['a'].append(1)\nT(lambda: (dd, dd['zz'], dict(dd)))\nPt = co.namedtuple('Pt', 'x y', defaults=[0])\nT(lambda: (Pt(1), Pt(1, 2)._replace(y=9), Pt._fields, Pt(3)._asdict(), Pt._make([5, 6])))\nT(lambda: Pt(1, 2, 3))\nT(lambda: co.ChainMap({'a': 1}, {'a': 2, 'b': 3}).maps)\nT(lambda: dict(co.ChainMap({'a': 1}, {'a': 2, 'b': 3})))\nT(lambda: op.itemgetter(1, 0)('ab'))\nT(lambda: op.attrgetter('real', 'imag')(3))\nT(lambda: op.methodcaller('split', ',')('a,b'))\n",
        &[("N", &n.to_string())],
    )
}

fn gen_stdlib_misc(r: &mut Rng) -> String {
    let f = float(r);
    let i = int(r);
    tpl(
        "import math, json, re\nT(lambda: math.floor(@F@))\nT(lambda: math.ceil(@F@))\nT(lambda: math.trunc(@F@))\nT(lambda: math.fsum([0.1] * 10))\nT(lambda: math.prod([1.5, 2, 3]))\nT(lambda: math.frexp(@F@))\nT(lambda: math.ldexp(@F@, 3))\nT(lambda: math.fmod(@F@, 0.7))\nT(lambda: math.remainder(@F@, 0.7))\nT(lambda: math.copysign(3, @F@))\nT(lambda: math.log(abs(@I@) + 1))\nT(lambda: math.log(@I@, 2))\nT(lambda: math.sqrt(@F@))\nT(lambda: math.isclose(@F@, @F@ * (1 + 1e-10)))\nT(lambda: math.comb(@S@, 5) + math.perm(@S@, 3))\nT(lambda: math.factorial(@S@))\nT(lambda: math.hypot(3, 4, @F@))\nT(lambda: math.nextafter(@F@, 0) - @F@)\nT(lambda: math.ulp(@F@))\nT(lambda: math.exp(@F@))\nT(lambda: math.atan2(@F@, -0.0))\nT(lambda: math.modf(@F@))\nT(lambda: (math.inf, -math.inf, math.pi, math.e, math.tau))\nT(lambda: json.dumps({'b': [1, 2.5, None, True], 'a': 'é\\n\"', 3: float('inf')}, sort_keys=True))\nT(lambda: json.dumps({'a': [1, {'b': 2}], 'c': []}, indent=2))\nT(lambda: json.dumps([@F@, @I@, @S2@], ensure_ascii=False, separators=(',', ':')))\nT(lambda: json.dumps({(1, 2): 3}))\nT(lambda: json.dumps({1: 1, 1.5: 2, True: 3, None: 4}))\nT(lambda: json.dumps(object()))\nT(lambda: json.loads('[1, 2.0, 1e2, \"x\", null, {\"a\": [true]}]'))\nT(lambda: json.loads('{\"a\": 1,}'))\nT(lambda: json.loads(''))\nT(lambda: json.loads('[1, 2'))\nT(lambda: json.loads('NaN'))\nT(lambda: json.loads('123456789012345678901234567890'))\nT(lambda: json.loads('\"\\\\ud83d\\\\ude00\"'))\nT(lambda: json.loads('{\"a\": 1, \"a\": 2}'))\nT(lambda: re.match(r'(?P<w>\\w+)-(\\d+)?', 'abc-').groups())\nT(lambda: re.sub(r'(a)(b)?', lambda m: '<%s|%s>' % m.groups(), 'abaab'))\nT(lambda: re.sub(r'(?P<x>\\d)', r'[\\g<x>\\1]', 'a1b2'))\nT(lambda: re.split(r'(,)\\s*', 'a, b,c'))\nT(lambda: re.findall(r'(\\w)(\\d)', 'a1 b2 c'))\nT(lambda: [m.span() for m in re.finditer(r'\\b\\w', 'ab cd  ef')])\nT(lambda: re.fullmatch(r'a*?', 'aaa'))\nT(lambda: re.search(r'(?<=x)y+', 'xyyz').group())\nT(lambda: re.sub('', '-', 'abc'))\nT(lambda: re.split(r'\\s*', 'a b'))\nT(lambda: re.compile('[').pattern)\nT(lambda: re.match(r'(?i)ß', 'SS'))\nT(lambda: re.findall(r'^\\w+$', 'ab\\ncd\\n', re.M))\nT(lambda: re.escape('a.b*c d-é'))\nT(lambda: re.sub(r'\\s+', ' ', '  a \\t\\n b  ').strip())\nT(lambda: re.match(r'(a)|(b)', 'b').groups())\nT(lambda: re.match(r'(a)|(b)', 'b').lastindex)\nT(lambda: re.subn(r'a', 'b', 'aaa', count=2))\nT(lambda: re.match(r'(?P<a>.)(?P=a)', 'xx').groupdict())\nT(lambda: re.match(r'\\d+', '١٢٣').group())\nT(lambda: re.match(r'\\d+', '١٢٣', re.A))\n",
        &[("F", f), ("I", i), ("S", &r.range(0, 30).to_string()), ("S2", string(r))],
    )
}

/// A random arithmetic/bitwise expression tree over ints, bools and floats,
/// each leaf and operator drawn from the seeded stream. Every expression is
/// evaluated under `T`, so a raising one (`ZeroDivisionError`, `OverflowError`,
/// `ValueError: negative shift count`) must raise identically.
fn expr_tree(r: &mut Rng, depth: u32) -> String {
    if depth == 0 || r.chance(25) {
        return match r.below(5) {
            0 => float(r).to_string(),
            1 => "True".into(),
            2 => "False".into(),
            _ => int(r).to_string(),
        };
    }
    match r.below(10) {
        0 => format!("-({})", expr_tree(r, depth - 1)),
        1 => format!("~({})", expr_tree(r, depth - 1)),
        2 => format!("abs({})", expr_tree(r, depth - 1)),
        3 => format!(
            "divmod({}, {})",
            expr_tree(r, depth - 1),
            expr_tree(r, depth - 1)
        ),
        4 => format!(
            "pow({}, {}, {})",
            expr_tree(r, depth - 1),
            r.range(0, 40),
            int(r)
        ),
        5 => format!("round({}, {})", expr_tree(r, depth - 1), r.range(-3, 5)),
        _ => {
            let op = r.pick(&[
                "+", "-", "*", "//", "%", "**", "<<", ">>", "&", "|", "^", "/", "<", "<=", "==",
                "!=", ">", ">=", "and", "or",
            ]);
            let (a, b) = (expr_tree(r, depth - 1), expr_tree(r, depth - 1));
            // Keep the exponent/shift small so the evaluation stays fast.
            let b = if matches!(op, "**" | "<<" | ">>") {
                format!("({}) % 70", b)
            } else {
                b
            };
            format!("({a}) {op} ({b})")
        }
    }
}

fn gen_exprtree(r: &mut Rng) -> String {
    let mut out = String::new();
    for _ in 0..8 {
        let e = expr_tree(r, 3);
        out += &tpl(
            "T(lambda: @E@)\nT(lambda: type(@E@).__name__)\n",
            &[("E", &e)],
        );
    }
    out
}

fn gen_numparse(r: &mut Rng) -> String {
    let mut out = String::new();
    let strs = [
        "'1e400'",
        "' 1_0.5 '",
        "'nan'",
        "'-inf'",
        "'infinity'",
        "'1e'",
        "'.5'",
        "'5.'",
        "'0x10'",
        "'1__0'",
        "'+1.5e-3'",
        "'١٢٣'",
        "'1,5'",
        "'  -7  '",
        "'0b101'",
        "'0o17'",
        "'1_000_000'",
        "'٣.١٤'",
        "'1e-400'",
        "'-0'",
        "'-0.0'",
        "('9'*400)",
        "'inf'",
        "'+nan'",
        "'1E5'",
        "'0.1e1_0'",
        "'\\n12\\t'",
        "''",
        "' '",
        "'_1'",
        "'1_'",
        "'１２３'",
    ];
    for _ in 0..6 {
        let s = strs[r.below(strs.len())];
        let base = r.pick(&["10", "2", "8", "16", "36", "0", "7"]);
        out += &tpl(
            "T(lambda: float(@S@))\nT(lambda: int(@S@))\nT(lambda: int(@S@, @B@))\nT(lambda: complex(@S@))\nT(lambda: float(@S@.encode()))\nT(lambda: int(@S@.encode(), @B@))\n",
            &[("S", s), ("B", base)],
        );
    }
    out += "T(lambda: float.fromhex('0x1.8p3'))\nT(lambda: float.fromhex('-0x.1p-1074'))\nT(lambda: (1.5).hex())\nT(lambda: float.hex(5e-324))\nT(lambda: int.from_bytes(b'\\xff\\xfe', 'little', signed=True))\nT(lambda: (255).to_bytes(1, 'big'))\nT(lambda: (256).to_bytes(1, 'big'))\nT(lambda: (-1).to_bytes(2, 'big'))\nT(lambda: complex('1+2j') * 1j)\nT(lambda: complex(1e308, 1e308) * 10)\nT(lambda: abs(complex(3, 4)))\nT(lambda: complex(1, 2) ** 2)\nT(lambda: complex(0, 0) ** -1)\nT(lambda: repr(complex(-0.0, -0.0)))\nT(lambda: complex(float('nan'), 1))\nT(lambda: round(complex(1, 2)))\n";
    out
}

fn gen_dictops(r: &mut Rng) -> String {
    // A random op sequence over a dict, an OrderedDict and a list.
    let mut out = String::from(
        "import collections\nd = {}\nod = collections.OrderedDict()\nl = []\nlog = []\n",
    );
    for i in 0..14 {
        let k = r.range(0, 7);
        let v = r.range(0, 99);
        match r.below(11) {
            0 | 1 => {
                out += &format!("d[{k}] = {v}\nod[{k}] = {v}\nl.append(({k}, {v}))\n");
            }
            2 => out += &format!("d.pop({k}, None)\nod.pop({k}, None)\n"),
            3 => {
                out += &format!(
                    "T(lambda: d.setdefault({k}, {v}))\nT(lambda: od.setdefault({k}, {v}))\n"
                )
            }
            4 => out += "T(lambda: d.popitem())\nT(lambda: od.popitem(last=False))\n",
            5 => {
                out += &format!(
                    "T(lambda: od.move_to_end({k}, last={}))\n",
                    r.chance(50) && true
                )
                .replace("true", "True")
                .replace("false", "False")
            }
            6 => {
                out += &format!(
                    "d.update({{{k}: {v}, {}: {i}}})\nod.update({{{k}: {v}, {}: {i}}})\n",
                    k + 1,
                    k + 1
                )
            }
            7 => out += &format!("T(lambda: l.pop({}))\n", r.range(-3, 3)),
            8 => out += &format!("T(lambda: l.insert({}, ({k}, {v})))\n", r.range(-3, 3)),
            9 => out += &format!("T(lambda: (d.get({k}), od.get({k}), {k} in d, {k} in od))\n"),
            _ => out += "T(lambda: (list(d.items()), list(od.items()), len(l)))\n",
        }
    }
    out += "print(d, od, l)\nprint(list(reversed(d)), list(reversed(od)))\nT(lambda: sorted(l, key=lambda t: t[0]))\nT(lambda: sorted(l, key=lambda t: t[0], reverse=True))\nT(lambda: sorted(d.items(), key=lambda kv: kv[1] % 3))\nT(lambda: dict(l) == dict(d))\nT(lambda: od == dict(od))\nT(lambda: list(od.keys()) == list(d.keys()))\nT(lambda: d | od)\nT(lambda: dict(zip(d, od.values())))\n";
    out
}

fn gen_super(r: &mut Rng) -> String {
    let n = r.range(0, 3);
    tpl(
        "log = []\nclass Base:\n    def __init__(self, *a, **k):\n        log.append(('Base', a, tuple(sorted(k))))\n        super().__init__()\n    def go(self, x):\n        return ['Base'] + [x]\n    @classmethod\n    def make(cls, *a): return cls(*a)\n    @staticmethod\n    def st(v): return ('st', v)\nclass L(Base):\n    def __init__(self, *a, **k):\n        log.append(('L', a))\n        super().__init__(*a, **k)\n    def go(self, x):\n        return ['L'] + super().go(x)\nclass R(Base):\n    def __init__(self, *a, **k):\n        log.append(('R', a))\n        super().__init__(*a, **k)\n    def go(self, x):\n        return ['R'] + super().go(x)\nclass D(L, R):\n    def __init__(self, *a, **k):\n        log.append(('D', a))\n        super().__init__(*a, **k)\n    def go(self, x):\n        return ['D'] + super().go(x)\nd = D(@N@, z=1)\nprint(log)\nprint(d.go(@N@), D.__mro__ == (D, L, R, Base, object))\nprint(D.make(1).go(2), D.st(3), d.st(4), type(D.make()).__name__)\nT(lambda: super(L, d).go(0))\nT(lambda: super(R, d).go(0))\nT(lambda: super(D, D).make)\nT(lambda: super(Base, d).go)\nT(lambda: super(int, d))\nT(lambda: super().__init__())\nT(lambda: D.go(d, 7))\nT(lambda: D.go(Base(), 7))\nT(lambda: D.go(7))\nclass Meta(type):\n    def __new__(m, name, bases, ns, **kw):\n        ns['tag'] = name.lower()\n        return super().__new__(m, name, bases, ns)\n    def __call__(cls, *a, **k):\n        o = super().__call__(*a, **k)\n        o.called = True\n        return o\n    def __init__(cls, name, bases, ns, **kw):\n        super().__init__(name, bases, ns)\nclass M(metaclass=Meta):\n    def __init__(self): self.x = 1\nm = M()\nprint(M.tag, m.x, m.called, type(M).__name__, isinstance(M, Meta))\nclass S1:\n    __slots__ = ('a',)\nclass S2(S1):\n    __slots__ = ('b',)\nclass S3(S2):\n    pass\ns3 = S3(); s3.a = 1; s3.b = 2; s3.c = 3\nT(lambda: (s3.a, s3.b, s3.c, hasattr(s3, '__dict__')))\nT(lambda: setattr(S2(), 'c', 1))\nclass New:\n    def __new__(cls, *a):\n        return a if a else super().__new__(cls)\n    def __init__(self, *a): print('init', a)\nT(lambda: New(1, 2))\nT(lambda: type(New()).__name__)\nclass Cnt:\n    n = 0\n    def __init_subclass__(cls, /, step=1, **kw):\n        super().__init_subclass__(**kw)\n        cls.n = Cnt.n + step\n        Cnt.n = cls.n\nclass C1(Cnt, step=@N@ + 1): pass\nclass C2(Cnt): pass\nT(lambda: (C1.n, C2.n, Cnt.n))\nT(lambda: type('X', (Cnt,), {}, step=5).n)\nT(lambda: type('X', (Cnt,), {}, bad=5))\n",
        &[("N", &n.to_string())],
    )
}

fn gen_traceback(r: &mut Rng) -> String {
    let a = r.range(0, 4);
    tpl(
        "import traceback, sys\ndef fmt(e):\n    return ''.join(traceback.format_exception(e)).replace(__file__, 'F')\ndef inner(n):\n    return [1, 2, 3][n] // (n - @A@)\ndef mid(n):\n    try:\n        return inner(n)\n    except IndexError as e:\n        raise ValueError('mid %d' % n) from e\ndef outer(n):\n    try:\n        return mid(n)\n    except ZeroDivisionError:\n        raise KeyError(n)\nfor n in (0, @A@, 5):\n    try:\n        outer(n)\n    except BaseException as e:\n        print(fmt(e))\nclass E(Exception):\n    def __str__(self): return 'custom'\ntry:\n    try:\n        raise E('a')\n    finally:\n        print('fin', sys.exc_info()[0])\nexcept E as e:\n    print(fmt(e), repr(e), e.args)\ntry:\n    {}['k']\nexcept KeyError as e:\n    print(fmt(e))\ntry:\n    None + 1\nexcept TypeError as e:\n    print(fmt(e))\ntry:\n    int('x')\nexcept ValueError as e:\n    print(fmt(e))\ntry:\n    [].nope\nexcept AttributeError as e:\n    print(fmt(e), e.name, type(e.obj).__name__)\ntry:\n    undefined_thing\nexcept NameError as e:\n    print(fmt(e), e.name)\ntry:\n    1 / 0\nexcept ZeroDivisionError:\n    print(''.join(traceback.format_exc()).replace(__file__, 'F'))\ntry:\n    raise ExceptionGroup('g', [ValueError('v'), TypeError('t')])\nexcept ExceptionGroup as e:\n    print(fmt(e))\n    print(e.exceptions, e.message, e.subgroup(ValueError))\ntry:\n    assert 1 == 2, 'msg'\nexcept AssertionError as e:\n    print(fmt(e))\nprint(traceback.format_exception_only(ValueError, ValueError('x')), traceback.format_exception_only(KeyError('a')))\n",
        &[("A", &a.to_string())],
    )
}

fn gen_strformat_syntax(r: &mut Rng) -> String {
    let mut out = String::new();
    let fields = [
        "{}",
        "{0}",
        "{:>8}",
        "{0:^9.3f}",
        "{a}",
        "{a.real}",
        "{l[1]}",
        "{d[k]}",
        "{0!r}",
        "{a!s:>6}",
        "{{x}}",
        "{:{w}}",
        "{:{w}.{p}f}",
        "{a:#x}",
        "{0:,}",
        "{:%}",
        "{0:.2%}",
        "{l[0]!r:>5}",
        "{:}",
        "{0:}",
        "{!r}",
        "{a.nope}",
        "{9}",
        "{b}",
        "{",
        "}",
        "{0",
        "{:z}",
        "{0:d}",
        "{0.real}",
        "{l[9]}",
        "{d[x]}",
        "{:{}}",
        "{0:{1}}",
        "{a:{w}x}",
    ];
    for _ in 0..8 {
        let f = fields[r.below(fields.len())];
        let v = match r.below(3) {
            0 => int(r),
            1 => float(r),
            _ => string(r),
        };
        out += &tpl(
            "T(lambda: @F@.format(@V@, 2, a=@V@, l=[@V@, 'q'], d={'k': @V@}, w=7, p=2))\nT(lambda: @F@.format_map({'a': @V@, 'w': 5, 'p': 1, 'l': [1], 'd': {'k': 1}}))\n",
            &[("F", &format!("{f:?}")), ("V", v)],
        );
    }
    out
}

type Gen = fn(&mut Rng) -> String;

const GENERATORS: &[(&str, Gen)] = &[
    ("numfmt", gen_numfmt),
    ("floatrepr", gen_floatrepr),
    ("bigint", gen_bigint),
    ("strmeth", gen_strmeth),
    ("printf", gen_printf),
    ("fstring", gen_fstring),
    ("slicing", gen_slicing),
    ("containers", gen_containers),
    ("sorting", gen_sorting),
    ("comp_scope", gen_comp_scope),
    ("generators", gen_generators),
    ("closures", gen_closures),
    ("exc", gen_exc),
    ("classes", gen_classes),
    ("dunder", gen_dunder),
    ("descr", gen_descr),
    ("stdlib_iter", gen_stdlib_iter),
    ("stdlib_misc", gen_stdlib_misc),
    ("exprtree", gen_exprtree),
    ("numparse", gen_numparse),
    ("dictops", gen_dictops),
    ("super", gen_super),
    ("traceback", gen_traceback),
    ("strformat_syntax", gen_strformat_syntax),
];

// ---------------------------------------------------------------------------
// Program assembly and execution
// ---------------------------------------------------------------------------

const PRELUDE: &str = "\
import sys
def _x(e):
    c, k = e.__cause__, e.__context__
    try:
        m = str(e)
    except BaseException as e2:
        m = '<str raised %s>' % type(e2).__name__
    return '!%s: %s | cause=%s ctx=%s sup=%s' % (
        type(e).__name__, m,
        type(c).__name__ if c is not None else None,
        type(k).__name__ if k is not None else None,
        e.__suppress_context__)
def T(f):
    n = sys._getframe(1).f_lineno
    try:
        print('@%d %r' % (n, f()))
    except BaseException as e:
        print('@%d %s' % (n, _x(e)))
def _run(n, f):
    print('#%d' % n)
    try:
        f()
    except BaseException as e:
        print(_x(e))
";

fn case_source(n: usize, body: &str) -> String {
    let mut s = format!("def _case{n}():\n");
    let mut any = false;
    for line in body.lines() {
        any = true;
        s.push_str("    ");
        s.push_str(line);
        s.push('\n');
    }
    if !any {
        s.push_str("    pass\n");
    }
    s.push_str(&format!("_run({n}, _case{n})\n"));
    s
}

/// One case: generator name and body, chosen from the seed.
fn make_case(seed: u64) -> (&'static str, String) {
    let mut r = Rng::new(seed);
    let (name, g) = GENERATORS[r.below(GENERATORS.len())];
    (name, g(&mut r))
}

fn program(seed0: u64, cases: usize) -> (String, Vec<(&'static str, String)>) {
    let mut prog = PRELUDE.to_string();
    let mut made = Vec::new();
    for n in 0..cases {
        let (name, body) = make_case(seed0.wrapping_mul(1_000_003).wrapping_add(n as u64));
        prog.push_str(&case_source(n, &body));
        made.push((name, body));
    }
    (prog, made)
}

struct Out {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    timed_out: bool,
}

fn run(bin: &Path, script: &Path, timeout: Duration) -> Out {
    let mut child = Command::new(bin)
        .arg(script)
        .env("PYTHONHASHSEED", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONWARNINGS", "ignore")
        .env("TZ", "UTC")
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let h1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut so, &mut b).ok();
        b
    });
    let h2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut se, &mut b).ok();
        b
    });
    let start = Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    Out {
        stdout: String::from_utf8_lossy(&h1.join().unwrap_or_default()).into_owned(),
        stderr: String::from_utf8_lossy(&h2.join().unwrap_or_default()).into_owned(),
        code,
        timed_out,
    }
}

/// `0x...` addresses are masked so object reprs compare.
fn mask_addr(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '0' && i + 2 < b.len() && b[i + 1] == 'x' && b[i + 2].is_ascii_hexdigit() {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - i - 2 >= 6 {
                out.push_str("0xADDR");
                i = j;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Split output into `(case number, text)` chunks on the `#<n>` markers.
fn chunks(out: &str, exact: bool) -> Vec<(usize, String)> {
    let mut v: Vec<(usize, String)> = Vec::new();
    for line in out.lines() {
        if let Some(n) = line.strip_prefix('#').and_then(|t| t.parse::<usize>().ok()) {
            v.push((n, String::new()));
            continue;
        }
        let line = mask_addr(line);
        let line = if !exact && line.starts_with('!') {
            line.split(':').next().unwrap_or("").to_string()
        } else {
            line
        };
        if let Some(last) = v.last_mut() {
            last.1.push_str(&line);
            last.1.push('\n');
        }
    }
    v
}

fn ours() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_python"))
}

fn env_num(k: &str, default: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn batched_programs_match_the_reference_python3() {
    let Ok(oracle) = pythonrs::oracle::resolve() else {
        eprintln!("batch_fuzz: SKIP - no usable python3, nothing to compare against");
        return;
    };
    let Some((major, minor)) = pythonrs::oracle::version_info(&oracle) else {
        eprintln!("batch_fuzz: SKIP - oracle version unknown");
        return;
    };
    let exact = (major, minor) >= (3, 14);
    println!("batch_fuzz: oracle {}", pythonrs::oracle::identify(&oracle));

    let seed0 = env_num("BATCH_FUZZ_SEED", 0);
    let programs = env_num("BATCH_FUZZ_PROGRAMS", 12);
    let cases = env_num("BATCH_FUZZ_CASES", 40) as usize;
    let keep = std::env::var("BATCH_FUZZ_KEEP").ok().map(PathBuf::from);
    let dir = tempfile::tempdir().expect("tempdir");
    let timeout = Duration::from_secs(60);
    let mut failures: Vec<String> = Vec::new();

    for p in 0..programs {
        let seed = seed0 + p;
        let (src, made) = program(seed, cases);
        let path = dir.path().join(format!("batch_{seed}.py"));
        std::fs::write(&path, &src).unwrap();
        let want = run(&oracle, &path, timeout);
        assert!(
            !want.timed_out && want.code == Some(0),
            "reference failed on generated program seed {seed}: {:?}\n{}",
            want.code,
            want.stderr
        );
        let got = run(&ours(), &path, timeout);
        let wc = chunks(&want.stdout, exact);
        let gc = chunks(&got.stdout, exact);
        let mut bad = false;
        let src_lines: Vec<&str> = src.lines().collect();
        for (i, (n, w)) in wc.iter().enumerate() {
            let g = gc.get(i).filter(|(m, _)| m == n);
            let gt = g.map(|(_, t)| t.as_str());
            if gt == Some(w.as_str()) {
                continue;
            }
            bad = true;
            let (name, _) = &made[*n];
            let mut msg = format!("-- seed {seed} case {n} [{name}]");
            match gt {
                None => {
                    msg += &format!(
                        "\n  our output missing for this case; stderr: {}",
                        got.stderr.lines().last().unwrap_or("").trim()
                    );
                }
                Some(gt) => {
                    let wl: Vec<&str> = w.lines().collect();
                    let gl: Vec<&str> = gt.lines().collect();
                    let mut shown = 0;
                    let mut last_marker = None;
                    for k in 0..wl.len().max(gl.len()) {
                        let (a, b) = (wl.get(k).copied(), gl.get(k).copied());
                        if let Some(m) = a.and_then(|l| l.strip_prefix('@')) {
                            last_marker = m.split(' ').next().and_then(|d| d.parse::<usize>().ok());
                        }
                        if a != b {
                            shown += 1;
                            let at = last_marker
                                .and_then(|ln| src_lines.get(ln - 1))
                                .map(|l| l.trim())
                                .unwrap_or("");
                            msg += &format!(
                                "\n  src    : {at}\n  python3 : {}\n  pythonrs: {}",
                                a.unwrap_or("<none>"),
                                b.unwrap_or("<none>")
                            );
                            if shown >= 4 {
                                break;
                            }
                        }
                    }
                }
            }
            failures.push(msg);
            if gt.is_none() {
                break;
            }
        }
        if got.code != Some(0) && !bad {
            bad = true;
            failures.push(format!(
                "-- seed {seed}: exit {:?} timed_out={}\n{}",
                got.code,
                got.timed_out,
                got.stderr.lines().last().unwrap_or("")
            ));
        }
        if bad {
            if let Some(k) = &keep {
                let _ = std::fs::create_dir_all(k);
                let _ = std::fs::write(k.join(format!("batch_{seed}.py")), &src);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "pythonrs diverged from the reference on {} case(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The generator itself must be deterministic and non-trivial, or a clean run
/// proves nothing.
#[test]
fn generator_is_deterministic_and_covers_every_family() {
    let (a, _) = program(7, 30);
    let (b, _) = program(7, 30);
    assert_eq!(a, b);
    let mut seen = std::collections::HashSet::new();
    for s in 0..400u64 {
        seen.insert(make_case(s).0);
    }
    assert_eq!(seen.len(), GENERATORS.len());
}

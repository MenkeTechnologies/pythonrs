# pythonrs → CPython drop-in checklist

**Goal:** pythonrs becomes the `python3` that gets invoked for real work — every
script an agent or a human hands to `python3` runs on pythonrs, byte-for-byte
identical to CPython, with no fallback to the reference interpreter. This file is
the ordered, grounded gap list between here and that goal.

**How progress is measured (no vibes):**
- `parity-fuzz` — the differential fuzzer (`src/bin/parity_fuzz.rs`). It generates
  deterministic Python programs per mode, runs each through pythonrs and the
  reference `python3`, and reports every stdout/exit divergence, minimized. A
  mode at `divergences : 0` is a surface at parity. Re-run to re-measure; never
  weaken the comparison to move a number.
- **Whole-script readiness corpus** — `tests/dropin/*.py` + `scripts/dropin_check.sh`.
  Representative whole scripts of the kind an agent actually writes (file I/O,
  argv, subprocess, the common stdlib, real composites like read→count→sort).
  The runner runs each through pythonrs and `python3` with identical argv and an
  isolated cwd, diffs stdout + exit, and reports per-category readiness. This is
  what "can pythonrs transparently shadow `python3`" actually means: the fuzzer
  proves per-expression parity, the corpus proves whole-script parity. Where the
  fuzzer tests one expression at a time it can miss composite gaps — the corpus
  caught sort **stability**, `json.dumps(sort_keys=...)`, and f-string `#` alt-form
  that the fuzzer's mode outputs did not.
- Import/execution probes — `python -c 'import X'`, script argv, exit codes.

**Readiness snapshot — 2026-07-19: `3/30 OK (10%)`** against committed `main`
(19 ERR = missing `open()`/module wall, largely the Tier 1/2 items several of
which are being wired now; 8 DIFF = the behavior gaps below). Re-run:
`cargo build && ./scripts/dropin_check.sh`.

Tiers are ordered by drop-in impact: Tier 0/1 block *most* real scripts; the
language-semantics gaps in Tier 3 are narrow and already localized by the fuzzer.

---

## Tier 0 — Execution surface (the CLI contract every script assumes)

- [ ] **`sys.argv` population.** Returns `[]` today, even for `python script.py foo bar`
      and `python -c '...'`. CPython gives `['script.py', 'foo', 'bar']` and `['-c']`.
      Nearly every non-trivial script reads argv.
- [ ] **`sys.exit(code)`** — `AttributeError: module 'sys' has no attribute 'exit'`.
      Scripts signal failure this way; the exit code must propagate.
- [ ] **Traceback format on uncaught exceptions.** pythonrs prints the terse
      `python: ValueError: boom` on stderr; CPython prints the full
      `Traceback (most recent call last):` frame block. Tooling that greps
      tracebacks (and humans) expect the CPython shape.
- [ ] **`sys` completeness** — `sys.stdin/stdout/stderr` as writable file objects,
      `sys.path`, `sys.version`/`version_info`, `sys.platform`, `sys.maxsize`.
- [x] `python -c`, `python file.py`, and stdin-as-script dispatch all run.
- [x] Exit code is non-zero on uncaught error.

## Tier 1 — File & process I/O (the top blocker for real scripts)

- [ ] **`open()` / file I/O.** `NameError: name 'open' is not defined`. No read,
      write, append, context-manager (`with open(...)`), or iteration over lines.
      This is the single largest drop-in blocker — most scripts touch a file.
- [ ] **File objects** — `.read/.readline/.readlines/.write/.writelines/.close`,
      iteration, `.seek/.tell`, text vs binary mode, encodings.
- [ ] **`subprocess`** — `ModuleNotFoundError`. `run`, `Popen`, `check_output`,
      `PIPE`, return codes. Scripts that shell out need this.
- [ ] **`os` expansion** — beyond the current POSIX subset: `os.environ`,
      `os.listdir/scandir/walk`, `os.makedirs`, `os.remove/rename`, `os.getcwd/chdir`,
      `os.path.exists/isfile/isdir/join/basename/dirname/abspath`.
- [ ] **`pathlib`** — `ModuleNotFoundError`. `Path`, `/` joining, `.name/.stem/.suffix`,
      `.exists/.read_text/.write_text/.glob/.iterdir`.
- [ ] **`io`** — `StringIO`, `BytesIO`.

## Tier 2 — Core stdlib modules scripts reach for

Registered and importable today: `math`, `os`, `sys`, `json`, `random`, `string`,
`itertools`, `functools`.

- [ ] **`re`** — module file `src/stdlib/re.rs` exists but `import re` →
      `ModuleNotFoundError`; wire it into the import dispatch. `search/match/findall/
      finditer/sub/split/compile`, groups, flags.
- [ ] **`collections`** — `Counter`, `defaultdict`, `OrderedDict`, `deque`,
      `namedtuple`. Needs the new container types.
- [ ] **`argparse`** — `ArgumentParser`, `add_argument`, `parse_args`. Standard for
      any script with a CLI.
- [ ] **`datetime`** — `src/stdlib/datetime.rs` exists but not importable; wire it.
      `date/time/datetime/timedelta`, `.strftime/.strptime`, arithmetic.
- [ ] **Wire the already-written modules into import dispatch** — `bisect`, `heapq`,
      `statistics`, `textwrap` have `src/stdlib/*.rs` files but `import` fails
      (`ModuleNotFoundError`). Register them.
- [ ] **`time`** — `time`, `sleep`, `perf_counter`, `strftime`. (Keep any
      wall-clock-dependent output out of the parity corpus.)
- [ ] **`hashlib`**, **`base64`**, **`csv`** — common in data/glue scripts.
- [ ] **`typing`** (accept-and-ignore is enough for most scripts), **`dataclasses`**,
      **`enum`**, **`copy`**, **`shutil`**, **`tempfile`**, **`glob`**,
      **`urllib`/`http`** (longer tail).

## Tier 3 — Language semantics gaps (narrow, fuzzer-localized)

The fuzzer shows language semantics are largely at parity (see snapshot). The
concentrated gaps:

- [ ] **Integer floor-division `//` and modulo `%` sign semantics.** Python floors
      toward −∞ and `%` takes the divisor's sign; pythonrs uses C-style truncation.
      `-7 % -100` → `93` (pythonrs) vs `-7` (CPython); `-3 // 5` and friends. This is
      the `arith` mode's entire divergence set (~10/150). Arithmetic correctness —
      fix first in this tier.
- [ ] **`%`-operator string formatting.** `'%.2f' % x`, `'%d %s' % (...)` is a no-op
      today — returns the format string verbatim. Drives the `formatspec` mode
      (~46/150), the largest single divergence surface in the fuzzer.
- [ ] **3-argument `pow(a, b, m)`** — modular exponentiation ignores the modulus
      (`pow(2, 5, 5)` → `32` instead of `2`). Drives the `builtins` mode (~12/150).
- [ ] **`sorted`/`.sort` stability.** `sorted(pairs, key=...)` reorders equal-key
      elements (`[('alice',30),('carol',25),('bob',25)]` vs CPython's
      `[...,('bob',25),('carol',25)]`). Python guarantees a stable sort; use a
      stable algorithm. Found by the corpus, not the fuzzer.
- [ ] **`json.dumps(sort_keys=True)`** ignored — pythonrs emits insertion order.
      Common in config/serialization round-trips. Found by the corpus.
- [ ] **f-string / format `#` alternate form** — `f"{n:#x}"` yields `2a`, not `0x2a`
      (`#o`/`#b` likewise). The `#` flag of the format mini-language is dropped.
- [ ] **`str.format` keyword fields** — `'{name}'.format(name=...)` not bound (no
      kwargs plumbing through `.format`).
- [ ] **`//`, `%`, and bitwise ops on bignums** fall back to i64 range (correct only
      up to i64). BUGS.md.
- [ ] **`bytes`** — literals parse to a placeholder; bytes operations unimplemented.
      Blocks binary file I/O and `hashlib`/`base64` round-trips.
- [ ] **`async`/`await`** — parsed, but `await` is a passthrough and there is no
      event loop (`async def` runs synchronously). BUGS.md.
- [ ] **`yield from` delegation value** — iteration works; the sub-generator's
      `return` value is always `None`; sent values not forwarded. BUGS.md.
- [ ] **Dunder long tail** — `NotImplemented`-driven reflected-op negotiation,
      instance `__hash__` (as dict keys / set members), in-place `__iadd__` etc.,
      `with`'s `__exit__` receiving the real `(type, value, tb)` triple. BUGS.md.
- [ ] **Chained comparison interior operand** re-evaluated (wrong only when the
      middle operand has side effects). BUGS.md.
- [ ] **Nested f-string / format-spec fields** `f"{x:{w}}"` not expanded. BUGS.md.

## Tier 4 — Surfaces already at parity (regression-guard, keep at 0)

Verified `divergences : 0` by the fuzzer — do not regress. Each has a `parity-fuzz`
mode; wire these into CI as a regression gate once the runner is set up.

- [x] `floatfmt` — float `repr` / shortest-round-trip, division results.
- [x] `fstring` — `f"{x}"`, `:.2f`, `!r`, nested values.
- [x] `strmeth` — `upper/lower/split/join/replace/strip/find/count/startswith/zfill/title`.
- [x] `strings` — indexing, slicing (incl. negative/step), `*`, `+`, `in`.
- [x] `comparison` — chained comparisons, cross-type `==`, tuple comparison.
- [x] `bignum` — arbitrary-precision `+ - * **`, huge-int `str()`.
- [x] `listcomp` / `dictcomp` / `setcomp` — comprehensions with conditions/nesting.
- [x] `sorting` — `sorted`, `reverse`, `key`, `min`/`max`.
- [x] `boolint`, `ranges`, `ternary`, `augassign`.
- [x] `slice` — near parity (~1/150 edge case remaining).

---

## parity-fuzz snapshot — 2026-07-19

Oracle: reference `python3` (3.14.6). 150 cases/mode. `divergences : 0` = at parity.

| mode | div/150 | dominant gap |
|---|---|---|
| formatspec | 46 | `%`-operator string formatting is a no-op |
| builtins | 12 | 3-arg `pow(a,b,m)` ignores modulus |
| arith | 10 | `//`/`%` sign semantics (C-trunc vs Python floor) |
| slice | 1 | edge case |
| floatfmt, fstring, strmeth, strings, comparison, bignum, listcomp, dictcomp, setcomp, sorting, boolint, ranges, ternary, augassign | 0 | at parity |

Re-measure any mode: `cargo build && ./target/debug/parity-fuzz --<mode> --count 500`.
Replay one divergence: `./target/debug/parity-fuzz --<mode> --once --seed <N>`.

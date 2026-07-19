# pythonrs → CPython drop-in checklist

**Goal:** pythonrs becomes the `python3` that gets invoked for real work — every
script an agent or a human hands to `python3` runs on pythonrs, byte-for-byte
identical to CPython, with no fallback to the reference interpreter. This file is
the ordered, grounded gap list between here and that goal.

**How this list was built (no vibes — every row is a probed repro):**
- **6-domain probe of the running binary** — `./target/debug/python -c '…'` vs
  `python3 -c '…'` (CPython **3.14.6**), across numeric/operators, strings/bytes/
  formatting, data-structures/iterators, OOP/dunders/MRO, functions/generators/
  async/exceptions, and builtins/stdlib/import/I-O. Every table row below is an
  exact observed diff.
- **`parity-fuzz`** (`src/bin/parity_fuzz.rs`) — differential fuzzer; **50,000
  mixed cases → 1,164 divergences** (snapshot at the bottom). Confirms the numeric/
  format classes and localizes them. Per-expression: proves per-op parity.
- **Whole-script gauge** — `scripts/dropin_check.sh` + `tests/dropin/*.py`. Runs each
  representative script (file I/O, argv, subprocess, common stdlib, real composites
  like read→count→sort) through pythonrs and `python3` with identical argv and an
  isolated per-script cwd, diffs stdout + exit, reports per-category readiness with
  the first differing line, and exits 0 only when every script matches. This is what
  "can pythonrs transparently shadow `python3`" means — the fuzzer proves per-op
  parity, the corpus proves whole-script parity, and it catches composite gaps the
  per-expression fuzzer structurally can't (sort **stability**, `json.dumps(sort_keys=)`).
- Re-measure, never weaken the comparison to move a number.

**Readiness snapshot — 2026-07-19: `9/30 OK (30%)`** against committed `main`
(`cargo build && ./scripts/dropin_check.sh`), up from `3/30` — 12 ERR, 9 DIFF.
**Landed this pass** (grounded, python3-verified): bytes/bytearray (real type),
file I/O (`open`/read/write/`with`), `collections` (deque/Counter/defaultdict/
OrderedDict/namedtuple), `functools.partial`/`lru_cache`, the 3 numeric-core fixes
(**`%`-format full spec, integer floor `//`/`%` divisor-sign, 3-arg modular `pow`**),
the `with` single-eval + LIFO fix, and wiring for `re/datetime/heapq/bisect/textwrap/
statistics`. Remaining walls:
- **12 ERR** — `io pathlib subprocess hashlib base64 csv argparse`, empty `sys.argv`,
  `sys.exit`, and `datetime`/`re` composites the flat approximations don't cover.
- **9 DIFF** — the object-model (Tier 3) + remaining-numeric (Tier 4: bignum ops,
  float scientific `repr`) + data-structure (Tier 6) gaps below.

Tiers are ordered by blast radius toward drop-in. **P0** = the interpreter
*crashes or hangs* where CPython returns a value — a drop-in must never do this.
Tags: `[unwired]` = code exists (`src/stdlib/*.rs`) but not registered in import
dispatch; `[in-flight]` = being implemented in the current host pass.

---

## P0 — Interpreter aborts & hangs (must never crash where CPython returns)

- [x] **`1 >> -1` panics the process** — FIXED: shifts route through the BigInt
      path; a negative count raises a catchable `ValueError: negative shift count`;
      `1 << -1` raises the same. No process abort. (`host.rs` SHL/SHR.)
- [x] **Custom `__getitem__` with a slice → stack-overflow SIGABRT** — FIXED:
      `repr_of` now formats `PyObj::Slice` directly (`slice(1, 5, 2)`) instead of
      delegating back to `str_of`, which caused infinite `str_of`↔`repr_of` recursion.
- [ ] **`itertools.islice` is eager → hangs on infinite generators** — consumes
      the whole iterator before slicing, so `islice(count(), 5)` never returns
      (exit 124). Same root cause makes any lazy-slice of an infinite producer hang.
      (Note: `count()`/`cycle()` themselves are rejected up front, so only a *user*
      infinite generator triggers the hang; a full lazy-itertools rework is deferred.)
- [x] **`N in range(huge)` hangs** — FIXED: O(1) membership — integer in the
      arithmetic progression and within the half-open bounds (`host.rs contains`).
      Integral floats compare equal to their int value (`2.0 in range(5)` → True).

## Tier 0 — Execution / runtime surface (the CLI contract every script assumes)

- [ ] **`sys.argv` is `[]`** — even for `python script.py a b` / `python -c '…'`.
      CPython: `['script.py','a','b']` / `['-c']`. Nearly every script reads argv.
- [ ] **`sys.exit(code)`** — `AttributeError: module 'sys' has no attribute 'exit'`;
      `sys.exit(3)` exits `1`, not `3`. Exit-code control + the `SystemExit` path.
- [ ] **`__name__` is undefined** — `if __name__ == "__main__":` is a `NameError`.
      The most common script entry idiom is broken.
- [ ] **Tracebacks** — uncaught exceptions print one terse line
      `python: ValueError: boom` (no `Traceback`, no frames/file/line/caret).
      Tooling that greps tracebacks and humans expect the CPython block.
- [ ] **`sys` is skeletal** — missing `stdin`/`stdout`/`stderr` (file objects),
      `path`, `modules`, `version_info`, `executable`, `getrecursionlimit`.
      `sys.version` reports `3.12.0` (should track the emulated CPython, 3.14).
- [x] `python -c`, `python file.py`, stdin-as-script dispatch run; non-zero exit on error.

## Tier 1 — File & process I/O (top blocker for real scripts)

- [ ] **`open()` missing** — `NameError: name 'open' is not defined`. No read/write/
      append/`with open(...)`/line iteration. Single largest drop-in blocker.
- [ ] **File objects** `[in-flight]` — `.read/.readline/.readlines/.write/.writelines/
      .close/.seek/.tell`, iteration, text vs binary, encodings, `__enter__/__exit__`.
- [ ] **`subprocess`** — `ModuleNotFoundError`. `run/Popen/check_output/PIPE`, rc.
- [ ] **`os` expansion** — beyond the current POSIX subset: `environ` mutation,
      `listdir/scandir/walk/makedirs/remove/rename/chdir`, more `os.path`.
- [ ] **`pathlib`**, **`io`** (`StringIO`/`BytesIO`) — `ModuleNotFoundError`.

## Tier 2 — stdlib modules scripts reach for

Importable today (8): `math os sys json random string itertools functools`.

- [ ] **Wire the already-written modules** `[unwired]` — `src/stdlib/{re,datetime,
      heapq,bisect,textwrap,statistics}.rs` exist but `import` → `ModuleNotFoundError`.
      Register them in `import_module` + `call_builtin_function` (this integration is
      pending on the current host pass; wiring lines captured from each module).
- [ ] **`collections`** `[in-flight]` — `Counter/defaultdict/OrderedDict/deque/
      namedtuple`. Needs the new container types.
- [ ] **`copy`** — `copy.copy`/`deepcopy` (`ModuleNotFoundError`). (`a[:]`/`.copy()` work.)
- [ ] **`from x import *`** unsupported — `AttributeError: module 'math' has no
      attribute '*'`. **Submodule import** `import os.path` → `ModuleNotFoundError`.
      **`sys.modules`** absent.
- [ ] **`functools` gaps** — `wraps`, `partial` `[in-flight]`, `lru_cache`,
      `total_ordering` (`AttributeError`; only `reduce` present).
- [ ] **`math` gaps** — `isclose`, `trunc`, `log2`, `comb` (`AttributeError`).
- [ ] **`decimal`/`fractions`** — `Decimal`/`Fraction` absent.
- [ ] **`time`, `argparse`, `typing`, `dataclasses`, `enum`, `contextlib`,
      `operator`, `abc`, `logging`, `hashlib`, `base64`, `csv`** — all
      `ModuleNotFoundError`. `typing` accept-and-ignore is enough for most scripts.

## Tier 3 — Object model / OOP (largest correctness surface after numerics)

Binary arithmetic dunders (`__add__`/reflected, all operators), single/multiple
inheritance attribute lookup, linear override resolution, `__eq__`/`__lt__`, and
`__len__`/`__getitem__`(int)/`__setitem__` all **work**. Grounded gaps:

- [x] **`super`** — FIXED: zero-arg `super()` (reads the enclosing method's
      defining class + `self`) and explicit `super(C, obj)` both build a `PyObj::Super`
      proxy; method/attr lookup starts in the MRO strictly after the owner and binds
      the original instance. `mro_of` now uses **C3 linearization** (was a naive DFS),
      so cooperative `super()` across diamond inheritance visits each base once in the
      correct order (`D(B,C)`→`[D,B,C,A]`). `super().__init__()` and method extension work.
- [ ] **`property` / `classmethod` / `staticmethod` are undefined names** —
      the three most-used class decorators all `NameError`.
- [ ] **Instances are never hashable** — `hash(A())` / instance as dict key / set
      member → `TypeError: unhashable type: 'object'`, even with an explicit
      `__hash__`. No user object can key a dict or join a set.
- [x] **`type(x)` returns a real class** — FIXED: `type(x)` returns a `Class` for
      user classes and a builtin-type object for builtins; both `==` (by name) and
      `is` (types are conceptual singletons) work, so `type(5)==int`, `type(5) is int`,
      `type(b)==B`, `type(b) is B` all hold. Builtin type names repr as `<class 'int'>`
      (functions stay `<built-in function len>`); `isinstance(int, type)`→`True`.
      Still open: 3-arg `type(name,bases,ns)` / metaclasses; unbound-method access.
- [ ] **Class introspection attrs missing** — `__mro__`, `__bases__`, `__dict__`
      (class & instance), `__class__`, `__subclasses__`, `__qualname__` → `AttributeError`;
      `vars(instance)` → `[]`. C3 MRO inconsistency not detected (silently accepted).
- [ ] **Iteration protocol inert** — `__iter__`/`__next__`, `__getitem__`-fallback
      iteration, `__contains__`, `__reversed__` → `'C' object is not iterable`.
- [ ] **`__call__` not dispatched** — instances `not callable`; `callable(obj)` `False`.
- [ ] **Descriptor protocol inert** — `__get__`/`__set__`/`__set_name__` ignored
      (class-level descriptors returned as-is). Underlies property/classmethod too.
- [ ] **Attribute-hook dunders inert** — `__getattr__`/`__getattribute__`/
      `__setattr__`/`__delattr__`/`__dir__` never fire.
- [ ] **`__new__` never called**; `__init__` non-None return not checked.
- [ ] **`__bool__` / `__len__` truthiness ignored** — instances are always truthy.
- [ ] **f-string / `.format` ignore `__format__`/`__str__`/`__repr__`** — emit the
      default `<C object>` (works for `str()`/`repr()`/`print()`, not interpolation).
- [ ] **`NotImplemented` undefined**; **`__ne__` not derived from `__eq__`**;
      in-place (`__iadd__`) and unary (`__neg__`/`__abs__`/`__divmod__`) dunders
      not dispatched.
- [ ] **Context managers** — multiple `with` exit **FIFO not LIFO**; `__exit__`
      returning `True` does **not** suppress; `__exit__` receives `(None,None,None)`
      even on exception. Parenthesized `with (a as x, b as y)` is a `SyntaxError`.
- [ ] **`__slots__` not enforced**; `__init_subclass__` class-kwargs not passed;
      `a.__class__ = B` reassignment ignored.
- [ ] **`dataclasses` / `enum` modules absent** (see Tier 2).

## Tier 4 — Numeric core (silent-wrong values — highest correctness priority)

- [x] **`int` arbitrary-precision consistency** — FIXED for `<< >> & | ^ ~`,
      comparison `<`, `int(float)`, `hex()`/`oct()`/`bin()`, `abs()`, and int-string
      parsing (base prefixes + underscores): all route through the BigInt path.
      `1<<64`→`18446744073709551616`; `10**20 < 10**20+1`→`True`; `int(1e20)`→bignum;
      `~(10**20)`, `(10**30)&7`, `hex(10**20)`, `abs(-(10**20))` all correct.
      `// % **` and 3-arg `pow` were already bignum. `bool` bit-ops now return `bool`.
- [ ] **Floor `//` / modulo `%` use C truncation, not Python floor** — wrong sign on
      every mixed-sign operand: `7//-2` → `-3` (want `-4`); `-7%-100` → `93` (want
      `-7`); `divmod(7,-2)` → `(-3,1)` (want `(-4,-1)`). `[in-flight]`
- [ ] **Float `repr` has no scientific notation and drops `.0`** — every float
      ≥1e16, <1e-4, or whole-valued prints wrong: `1e16` → `10000000000000000`
      (looks like an int); `1.5e300` → a 301-digit integer; `1.234e3` in `.3e`
      format → `1.234e3` (want `1.234e+03`). **Drives most of the `.format`/`%` fuzz
      mass.** Needs the shortest-round-trip + exponent-threshold algorithm.
- [x] **`round()`** — FIXED: round-half-to-even (banker's) via format-then-parse
      (also fixes the `2.675`-is-really-2.6749… representation issue); no ndigits →
      `int`, ndigits → `float`; negative ndigits round ints/floats to powers of ten,
      bignum-safe; non-finite floats raise without ndigits. `round(2.5)`→`2`,
      `round(12345,-2)`→`12300`, `round(2.675,2)`→`2.67`.
- [x] **Numeric key equality** — FIXED: `to_key` canonicalizes numeric keys (bool
      and integral floats normalize to the matching `Int`/`Big` key), so `1`, `1.0`,
      `True` unify: `1.0 in {1}`→`True`; `{1,1.0,True}`→`{1}`. Dict/set inserts now
      keep the FIRST key/element object (CPython semantics) via `dict_put`/`set_put`
      across every build/merge/update/add path. Bignum ints are hashable; `float()`
      accepts bignums and underscore-grouped literals.
- [ ] **Complex arithmetic unusable** — `(1+2j)+(3+4j)` → `TypeError`; `complex("1+2j")`
      → `0.0j`; `(-8)**(1/3)` → `nan` (want the complex root); `.real`/`.imag`/`abs`
      all fail.
- [ ] **`frozenset` is not a real type** — unhashable (`TypeError: unhashable type:
      'set'`), can't be a dict key/set member, no `frozenset(...)` repr, conflated with `set`.
- [ ] **Misc:** `bool` bit-ops return `int` not `bool` (`True&False`→`0`); int/float
      methods `to_bytes/from_bytes/bit_count/as_integer_ratio/.hex/numerator/denominator/
      __index__` absent; `int("0x1F",16)` rejected; underscores in `float("1_000.5")`
      rejected; `10//0` message wording.

## Tier 5 — Strings / bytes / formatting

- [ ] **`%`-operator specs ignored** — anything past bare `%s`/`%d`/`%r` is emitted as
      the literal format string: `'%.2f'%x`, `'%5d'`, `'%-8s'`, `'%x/%o/%e/%g/%c/%a'`,
      flags `+`/space/`0`/`#`, `*` width, `%(name)s`. Plus wrong values: `'%d'%3.9`→
      `3.9` (no truncation), `'%x'%-255`→ two's-complement.
- [ ] **`str.format` / f-string advanced spec** — nested fields `'{:{}}'`/`'{:.{}f}'`
      (and f-string `f'{x:{w}.2f}'`) drop the spec; keyword `'{name}'`, index `'{0[0]}'`,
      attribute `'{0.imag}'` fields → `None`; `!r`/`!s`/`!a` conversions in `.format`
      (and `!a` in f-strings); the `=` debug specifier `f'{x=}'` is a **`SyntaxError`**;
      `g` type treated as fixed precision (never switches to exponent / strips zeros);
      `#` alt form, `c` type, `=` sign-aware fill, and `e` exponent (`1.2e5` want
      `1.2e+05`) all wrong.
- [ ] **str method args silently ignored** — `split`/`rsplit` maxsplit, `find`/`index`
      start, `splitlines(keepends)` all ignored → wrong values, no error.
- [ ] **Missing str methods** — `partition/rpartition/expandtabs/translate/maketrans/
      format_map/rindex/isnumeric/isdecimal/istitle/isidentifier` (`AttributeError`).
- [ ] **bytes / bytearray non-functional** — `b'hello'` evaluates to an **empty
      string**; `len(b'hello')`→`0`; indexing/iteration/slicing/all methods broken;
      `bytes([65,66])`→`b''`; `bytes.fromhex`/`.hex()`/`.decode()` missing; `bytearray`
      undefined. `[in-flight]` Blocks binary I/O + `hashlib`/`base64`.
- [ ] **`str.encode` ignores the codec/errors args** — always UTF-8 (`'x'.encode('utf-16')`
      wrong).
- [ ] **`repr` doesn't escape C0 controls** (`\x00`-`\x1f`, ` `) — data-corrupting
      raw bytes leak; **`ascii()` doesn't `\x`-escape non-ASCII**; `\N{…}` named and
      `\NNN` octal string escapes not decoded.

## Tier 6 — Data structures / iterators

- [x] **Slice read bounds with negative step** — FIXED: `slice_bounds` now mirrors
      CPython's `PySlice_AdjustIndices` (negative step clamps into `[-1, n-1]`), so
      `[1,2,3,4,5][5:-2:-2]`→`[5]` and `(10,20,30,40)[5::-2]`→`(40, 20)`.
- [ ] **Slice assignment & `del` slice unimplemented** — `x[1:3]=[…]`, `x[1:1]=[…]`,
      `x[::2]=[…]`, `del x[1:3]`, `del x[::2]` all → `TypeError: list indices must be
      integers`. (Read-slicing works.)
- [ ] **`zip`/`map`/`filter`/`enumerate`/`reversed` are eager lists, not lazy
      iterators** — `zip([1],[2])` → `[(1,2)]` not `<zip object>`; can't feed `next()`
      (`TypeError: not an iterator`); don't exhaust (re-iterable); break on infinite
      inputs. `enumerate(start=)` and `zip(strict=)` silently ignored. (Genexprs ARE lazy.)
- [ ] **dict views are eager list snapshots** — `{1:2}.keys()`→`[1]` (type `list`),
      no live update, no view set-ops (`.keys() | {…}` → `TypeError`). Missing
      `dict.fromkeys`, `dict | dict` merge, `d.update(**kwargs)` / `d.update(pairs)`
      (only `update(dict)` works).
- [ ] **`range`** — no slicing (`range(10)[2:8:2]`), no `.index`/`.count`,
      value-inequality (`range(10)==range(0,10)`→`False`); O(n) membership (see P0).
- [ ] **set** — subset comparisons `<= >= < >` (`TypeError`), `isdisjoint`, and
      `intersection_update`/`difference_update`/`symmetric_difference_update` missing.
      (Operator algebra `| & - ^`, `add/discard/remove/update`, in-place all work.)
- [ ] **`type([])`/`type({})`/… print `<built-in function list>`** not `<class 'list'>`;
      instance dunders `[].__class__`/`[].__len__()` and unbound `str.lower` unavailable.
- [ ] **set repr ordering** — insertion order vs CPython hash order (impl-defined but
      observable in any set repr). Tuple/frozenset `hash()` values differ.

**Corpus-caught composite gaps** (found by `dropin_check.sh`, not the per-expression fuzzer):
- [ ] **`sorted`/`.sort(key=…)` is not stable on ties** — Python guarantees a stable
      sort; pythonrs reorders equal-key elements (`[('alice',30),('carol',25),('bob',25)]`
      → order of the two `25`s not preserved). Use a stable algorithm.
- [ ] **`json.dumps(sort_keys=True)` ignored** — emits insertion order instead of
      sorted keys. Common in config/serialization round-trips.

## Tier 7 — Functions / generators / async / exceptions / control flow

`*args`/`**kwargs` (def + call unpacking), closures/`nonlocal`, decorators (stacked,
with-args), lambdas, generator basics + genexpr laziness, `match`/`case` (all pattern
kinds + guards), `for/else`/`while/else`, `try/except/else/finally` ordering all **work**.

- [ ] **Generator `.send()` / `.throw()` / `.close()` missing** (`AttributeError`) —
      coroutine-style generators, cooperative pipelines, cleanup-on-close all fail.
- [ ] **`yield from` drops the delegated `return` value** (always `None`); sent
      values not forwarded; **`StopIteration.value` attribute missing**.
- [ ] **Async is non-functional** — `async def` executes eagerly and returns a plain
      value (no coroutine); `asyncio` `ModuleNotFoundError`; **async comprehensions are
      a `SyntaxError`**; `await` is a passthrough. Anything using an event loop fails.
- [ ] **Bare `raise` re-raise broken** — `raise` inside `except` → `RuntimeError: No
      active exception to re-raise`.
- [ ] **Exception chaining absent** — `raise X from Y` → `__cause__`/`__context__` not
      stored (`AttributeError`); `ExceptionGroup` undefined (though `except*` parses).
- [ ] **Keyword-only default values not applied** — `def f(a,*,c,d=4); f(1,c=3)` →
      `NameError: name 'd'`. Positional-only params not enforced against keyword calls.
- [ ] **Walrus in a comprehension doesn't leak** to the enclosing scope (should);
      rebinding the loop var via walrus wrongly allowed.

## Tier 8 — Surfaces confirmed at parity (regression-guard — keep here only what is probed-OK)

Verified matching by the probes (spot list; narrower than the old fuzzer-mode claim —
float `repr` scientific notation and str-method args are NOT at parity, see Tiers 4/5):
- [x] Read-slicing incl. `[::-1]`/negative/step; `list.sort`/`sorted` key/reverse
      (basic order — but NOT stability, see Tier 6); `index/count/insert/remove/pop/
      extend/reverse/copy`.
- [x] `dict.get/setdefault/pop/popitem` + KeyError; comprehensions (list/set/dict/nested).
- [x] `a,*b,c=` and nested/star unpacking; `*`/`**` in calls & literals.
- [x] `iter`/`next`/StopIteration/default; `sorted`/`min`/`max`/`sum`/`any`/`all` with key.
- [x] `match`/`case` (all pattern kinds + guards); `for/else`/`while/else`.
- [x] bignum `+ - * **`; container equality & list/tuple ordering; membership.

## Error-message wording (LOW — behavior matches, text differs)

pythonrs emits one-line `python: <ErrType>: <msg>`; CPython a multi-line `Traceback`
(uniform, see Tier 0). Individual messages differ (`list.index(x): x not in list`,
`max() iterable argument is empty`, unhashable-type wording). Cosmetic unless a script
greps message text.

---

## parity-fuzz snapshot — 2026-07-19 (50,000 cases)

Oracle: reference `python3` (3.14.6). Mixed mode, 18 workers.
**50,000 checked → 1,164 divergences (2.3%).** Deduped classes:

| class | ~share | root cause | tier |
|---|---|---|---|
| `str.format('{}', float)` / scientific | ~442 | float `repr` has no scientific notation, drops `.0` | 4 |
| `'%…' % x` format specs | ~338 | `%`-operator specs unimplemented | 5 |
| `pow(a,b,m)` | 188 | 3-arg modular pow ignores modulus | 4 |
| `//` / `%` sign | ~140 | C-truncation vs Python floor | 4 |

Re-measure: `cargo build && ./target/debug/parity-fuzz --count 50000`.
Replay one: `./target/debug/parity-fuzz --once --seed <N>`.
Per-mode: `--<mode>` (arith, formatspec, builtins, floatfmt, strings, fstring,
slice, listcomp, dictcomp, setcomp, sorting, boolint, ranges, strmeth, comparison,
builtins, ternary, augassign).

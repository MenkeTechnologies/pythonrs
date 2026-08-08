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
  mixed cases → 0 divergences** as of 2026-08-02 (snapshot at the bottom). It drove
  the numeric/format classes to zero; per-expression it proves per-op parity, and it
  is now a regression net rather than a discovery tool.
- **Whole-script gauge** — `scripts/dropin_check.sh` + `tests/dropin/*.py`. Runs each
  representative script (file I/O, argv, subprocess, common stdlib, real composites
  like read→count→sort) through pythonrs and `python3` with identical argv and an
  isolated per-script cwd, diffs stdout + exit, reports per-category readiness with
  the first differing line, and exits 0 only when every script matches. This is what
  "can pythonrs transparently shadow `python3`" means — the fuzzer proves per-op
  parity, the corpus proves whole-script parity, and it catches composite gaps the
  per-expression fuzzer structurally can't (sort **stability**, `json.dumps(sort_keys=)`).
- Re-measure, never weaken the comparison to move a number.

**Readiness snapshot — 2026-08-08: `30/30 OK (100%)`** against committed `main`
(`cargo build && ./scripts/dropin_check.sh`, reference CPython 3.14.6) — 0 DIFF,
0 ERR, 0 SKIP. Up from `13/30 (43%)` on 2026-08-02, `9/30 (30%)` on 2026-07-19,
and `3/30` before that. Every category is complete: `lang 7/7`, `os 3/3`,
`real 3/3`, `collections 2/2`, `data 2/2`, `io 2/2`, and `argparse argv base64
csv datetime hashlib json pathlib re subprocess sys` at `1/1` each.

The last two ERR rows (`csv_parse.py`, `real_json_config.py`) were one defect: a
native `open()` handle could not cross into a CPython stdlib call, so
`json.dump(cfg, f)` and `csv.writer(f)` raised `TypeError: cannot pass
'TextIOWrapper' to a CPython stdlib call`. A native file now marshals as a
`PyrsFile` proxy (`src/ffi.rs`) whose read/write/iteration route back to the
same `file_method` the interpreter uses.

**Landed previously** (grounded, python3-verified): bytes/bytearray (real type),
file I/O (`open`/read/write/`with`), `collections` (deque/Counter/defaultdict/
OrderedDict/namedtuple), `functools.partial`/`lru_cache`, the 3 numeric-core fixes
(**`%`-format full spec, integer floor `//`/`%` divisor-sign, 3-arg modular `pow`**),
the `with` single-eval + LIFO fix, and wiring for `re/datetime/heapq/bisect/textwrap/
statistics`. The corpus has no remaining ERR or DIFF rows; grow it before
reading the 100% as "done" — it is 30 scripts, not a conformance suite.

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
- [x] **`itertools.islice` is eager → hangs on infinite generators** — RESOLVED via
      the `stdlib-ffi` bridge: `itertools` is now the real CPython module, so
      `islice`/`count`/`cycle` are natively lazy and `islice(count(), 5)` returns.
      (The old hand-rolled eager `itertools` shadow was deleted.)
- [x] **`N in range(huge)` hangs** — FIXED: O(1) membership — integer in the
      arithmetic progression and within the half-open bounds (`host.rs contains`).
      Integral floats compare equal to their int value (`2.0 in range(5)` → True).

## Tier 0 — Execution / runtime surface (the CLI contract every script assumes)

- [x] **`sys.argv`** — populated from the process args: `python script.py a b` →
      `['script.py','a','b']`; `python -c '…' x y` → `['-c','x','y']`;
      `python` (repl)/stdin → `['']`/`['-']`. Wired through `host::init_runtime`
      (`main.rs` builds argv; the `sys` module reads `h.argv`).
- [x] **`sys.exit(code)`** — raises a catchable `SystemExit`; an uncaught one exits
      `n` for an int, `0` for `None`/no-arg, `1` + the message on a str. `.code`
      exposes the arg. Exit-code propagated through `main.rs`.
- [x] **`__name__`** — the top-level script/`-c`/stdin runs as `__main__`, so
      `if __name__ == "__main__":` fires; `__file__` set (abs path) for a file run.
- [x] **Tracebacks** — an uncaught exception prints CPython's
      `Traceback (most recent call last):` block: the header, one
      `  File "<path>", line N, in <scope>` + the source line per frame (outermost
      first), then `ErrorType: message`. Line info comes from the compiler's
      per-op line metadata (call/binop/subscript/attr/name ops carry the stmt
      line); caret (`^^^`) markers are omitted for a first pass. Exit stays 1.
- [x] **`sys` completeness** — `stdin`/`stdout`/`stderr` file objects
      (`print(file=sys.stderr)` routes correctly), `version` (reports the emulated
      `3.14.6`), `version_info` (a namedtuple), `platform`, `maxsize`, `path`
      (list), `executable`, `modules`, `getrecursionlimit()`/`setrecursionlimit()`.
- [x] `python -c`, `python file.py`, stdin-as-script dispatch run; non-zero exit on error.
- [x] **`python -m MODULE [args…]`** — delegates to the embedded CPython
      (`runpy._run_module_as_main`, the same entry CPython's own `-m` uses), so
      `-m pip`/`-m venv`/`-m http.server`/`-m json.tool`/`-m calendar` run on the
      real interpreter. `-m` terminates interpreter-option parsing (raw-arg
      interception in `main.rs`), so every token after the module is the module's
      verbatim `sys.argv` (`pip install --upgrade`, `json.tool --sort-keys`);
      exit code propagates (`SystemExit.code` / uncaught → 1); piped `stdout` is
      flushed before exit (the interpreter is never `Py_Finalize`d). Requires the
      `stdlib-ffi` bridge; a `--no-default-features` build reports and exits 1.
- [x] **CPython interpreter flags** `-u -E -I -O/-OO -S -B -W <action>` — accepted
      for drop-in compatibility (previously hard-errored via clap). `-u` →
      `PYTHONUNBUFFERED`, `-W` → `PYTHONWARNINGS` (real effect on the embedded
      interpreter); `-E/-I/-O/-S/-B` are tolerated no-ops. (`src/cli.rs`,
      `src/main.rs`.)

## Tier 1 — File & process I/O (top blocker for real scripts)

**`subprocess`, `pathlib`, `io` (`StringIO`/`BytesIO`), and the full `os` surface
are CLOSED via the `stdlib-ffi` bridge** (real CPython modules). `open()` + file
objects are native (landed). Items below track the native default-build surface.

- [x] **`open()`** — FIXED: `open(path, mode)` returns a real file object;
      `with open(...)`, read/write/append, and line iteration all work.
- [x] **File objects** — FIXED: `.read/.readline/.readlines/.write/.writelines/
      .close/.seek/.tell`, iteration, text vs binary, encodings, `__enter__/__exit__`
      (`src/stdlib/pyio.rs`).
- [x] **`subprocess`** — resolves through the `stdlib-ffi` bridge (no more
      `ModuleNotFoundError`); `from subprocess import run` binds. `subprocess.run(...)`
      runs, returning a real `CompletedProcess` (`stdout` is `bytes` under
      `capture_output=True`).
- [x] **`os` expansion** — the real CPython `os` arrives over the bridge:
      `listdir/scandir/walk/makedirs/remove/rename/chdir` and the full `os.path` all
      bind via `from os import …`. Still open: `environ` item assignment
      (`environ["X"]="1"` → `TypeError: '_Environ' object does not support item
      assignment` — the marshalled mapping is read-only).
- [x] **`pathlib`**, **`io`** (`StringIO`/`BytesIO`) — both import and bind
      (`from pathlib import Path; Path("/a/b").name` → `b`). Method calls on the
      returned objects work too (`Path("/a/b.txt").with_suffix(".md")` → `/a/b.md`).

## Tier 2 — stdlib modules scripts reach for

**CLOSED via the CPython stdlib FFI bridge (feature `stdlib-ffi`).** pythonrs no
longer reimplements the stdlib. With the feature on, any module pythonrs does not
serve natively (`math`/`sys`/`collections` stay native; `textwrap`/`statistics`
kept hand-rolled) is imported from the **real CPython stdlib** — pure `.py` **and**
the C accelerators (`_sre`/`_hashlib`/`_datetime`/`_json`/…) — over an embedded
libpython (`src/ffi.rs`). `import <anything>`, `from x import y`, submodules
(`os.path`), and `sys.modules` all fall out of CPython's own importer. Results
marshal to pythonrs values by-value (int/float/bool/None/str/bytes/list/tuple/dict/
set); everything else stays a `PyObj::Foreign` handle whose attr/call/index/iter/
len/str/repr/membership **and binary / comparison / unary operators** route back
through the bridge (via CPython's `operator` module). A pythonrs callable passed
as a stdlib callback (`functools.reduce(f, …)`, `sorted(key=f)`) is wrapped so
CPython calls back into fusevm. Verified byte-identical to `python3` (3.14.6):
`re.findall`, `hashlib.sha256`, `argparse`, `json.dumps/loads`, `textwrap`,
`itertools.chain/combinations/permutations`, `functools.reduce/partial/lru_cache`,
`os.path.*`, `string.*`, `datetime.date + timedelta`, `datetime - datetime`,
`date < date`, `Decimal('0.1') + Decimal('0.2')` (exact), `Fraction + Fraction`,
`timedelta * 3`, `Decimal % Decimal`, `abs(Decimal('-5'))`.

Build/run with the feature: `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo build
--features stdlib-ffi` (CI has a dedicated `stdlib-ffi` job). Default builds never
pull pyo3 or need libpython, so they import only the native set below.

- The former hand-rolled shadows `src/stdlib/{json,os,random,string,itertools,
  functools}.rs` were **deleted** — the real CPython modules replace them.
- Native (available in every build): `math`, `sys`, `collections`
  (`Counter/defaultdict/OrderedDict/deque/namedtuple`), `textwrap`, `statistics`,
  plus the built-in `bytes`/`bytearray` and file I/O.
- [x] **Non-callable objects passed *into* a CPython call** — FIXED: `list`/`dict`/
  `tuple`/`set`/`str`/`bytes`/`int`/`float`/`None` already crossed by value; now
  `range`/`complex`/`collections.deque`/`frozenset` do too (`json.dumps({…})`,
  `functools.reduce(f, range(…))`, `"".join(list)`, `sorted(list, key=f)` all
  byte-identical to `python3`). An in-place stdlib mutator (`heapq.heapify`,
  `random.shuffle`, `struct.pack_into`) now **writes its mutation back** into the
  pythonrs object — by-value marshaling copies the argument, so the bridge re-reads
  the (mutated) CPython object and overwrites the heap slot in place (aliases see it
  too). Write-back marshals by value only; it never allocates a `Foreign` handle.
- **Known bridge limits:** the side-table is bounded for the value-marshaled path
  (`heapq.heapify`/`json.dumps`/`reduce` in a 2000-iteration loop add only the
  one-time module handle, never one-per-iter). It is **not** reclaimed for stdlib
  calls that *return* a live CPython object (`re.match` match objects, `datetime`,
  file handles): each distinct returned object takes a permanent slot, growing 1:1
  with the pythonrs host heap. That host heap is a pure arena — `host.rs` never
  frees any object (`heap`/`io_handles`/`lru_caches` are all append-only), `Value`
  is a `Copy` handle with no `Drop`, and `PyObj::Foreign` carries only a bare id, so
  the bridge has no signal for when a handle dies and cannot safely reclaim (a live
  host reference would dangle). Reclaiming those needs host-side object lifetime
  (a `Foreign`-drop callback / arena GC in `host.rs`), out of the bridge's scope.
  Module bundling for release artifacts (ship `lib/python3.14` + `libpython`) is
  future work per FFI_STDLIB.md §6.

**Known deferrals (intentional, not gaps to fake-close):**
- **FFI Foreign-handle reclamation** — needs host-side object lifetime (a
  `Foreign`-drop callback / arena GC in `host.rs`), a major architectural change
  to the pure-arena heap. Correctly deferred; see the bridge-limits note above.
- **Linux release bundle** — no Linux runner in this environment to build/verify a
  Linux artifact; deferred until one is available.

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
- [x] **`classmethod` / `staticmethod`** — FIXED: both are builtins that wrap the
      function in a `PyObj::StaticMethod`/`ClassMethod` marker; method dispatch
      (call_method + get_attr, instance and class receivers) honors them — static gets
      no implicit arg, classmethod binds the receiver's class as `cls` (derived-class
      aware, so `D.g` sees `D`). Alternate constructors `cls()` work.
- [x] **`property` + descriptor protocol** — FIXED: `PyObj::Property{fget,fset,fdel}`
      (a data descriptor) + `@property`/`@x.setter`/`@x.deleter` + the functional
      `property(fget,fset,fdel)` form. `plan_attr_get`/`plan_attr_set` implement the
      full protocol precedence (data descriptor > instance dict > non-data descriptor
      > class attr), fired from `b_getattr`/`b_setattr` (out of any host borrow so the
      accessor runs user code). User `__get__`/`__set__` descriptors and `__set_name__`
      (fired at class creation, definition order) work. Missing-getter raises the 3.14
      `property '<n>' of '<C>' object has no getter`. `getattr`/`hasattr`/`setattr`
      builtins route through the same path. A getter/setter now runs as a *bound*
      method (self + its defining class on the frame, `owner` carried in
      `AttrGet/AttrSet::Property`), so a zero-arg `super()` inside an accessor
      resolves — including `super().<some_property>`, which invokes the parent
      property's getter via the same out-of-borrow path.
- [x] **Instances are hashable** — FIXED: a new `PKey::Instance{hash,id}` keys a
      user instance by its `__hash__()` result plus a collapsed identity. Because
      `to_key` is `&self` and cannot run user code, the boundary op handlers
      (`b_getitem`/`b_setitem`/`b_delitem`/`b_contains`, set/dict literals,
      `set.add`/`discard`/`remove`, `set`/`frozenset`/`dict` ctors, `dict.get`/
      `setdefault`/`pop`/`fromkeys`) call `host::prepare_key` first — it runs
      `__hash__` (and `__eq__` against the container's existing instance keys to
      collapse a value-equal entry) outside the borrow and stashes the resolved
      key in a thread-local pending table that `to_key` reads. Default identity
      hashing (no user `__hash__`) is resolved inline; `__eq__` without `__hash__`
      or `__hash__ = None` raises `unhashable type`. `hash(inst)` returns the raw
      `__hash__` result. Boundary: instance↔builtin cross-type key unification
      (`{1: 'a'}[C()]` where `C().__eq__(1)`) is not collapsed — instance keys
      only collapse onto other instance keys.
- [x] **`type(x)` returns a real class** — FIXED: `type(x)` returns a `Class` for
      user classes and a builtin-type object for builtins; both `==` (by name) and
      `is` (types are conceptual singletons) work, so `type(5)==int`, `type(5) is int`,
      `type(b)==B`, `type(b) is B` all hold. Builtin type names repr as `<class 'int'>`
      (functions stay `<built-in function len>`); `isinstance(int, type)`→`True`.
      **3-arg `type(name, bases, ns)` now builds a real class** (`type_new` →
      `register_class`): attrs, methods, and base inheritance work.
- [x] **Metaclasses** — FIXED: `class A(metaclass=M)` (compiler passes the
      metaclass to `BUILD_CLASS`, cache v10) constructs the class via
      `M(name, bases, ns)` — `M.__new__`/`M.__init__` fire and `type(A) is M`
      (`ClassDef.metaclass`, `type_name(Class)` returns it). A cooperative
      `super().__new__(mcls, name, bases, ns)` / `super().__init__(...)` /
      `super().__call__(...)` in a metaclass method falls through to `type.__new__`
      (builds + tags the class) / a no-op / plain instantiation. A metaclass
      `__call__` controls instantiation (`instantiate` dispatches to it; singleton
      pattern works). Metaclass attributes/methods are visible through the class
      (`cls._registry`, `A.meta_method()` bound to the class). A subclass inherits
      the most-derived metaclass of its bases. Class objects are hashable dict/set
      keys (`PKey::Class`, by name). `__new__` now runs with `cls` as the frame
      `self` so zero-arg `super().__new__(cls)` resolves in ordinary classes too.
- [x] **Class introspection attrs** — FIXED: instance `__class__`/`__dict__`,
      class `__mro__`/`__bases__`/`__dict__`/`__qualname__` (`object` is the implicit
      MRO/bases tail), and `vars(instance)` (== `__dict__`). User-class repr now
      carries the `__main__.` module qualifier to match CPython (builtins stay bare).
      Still open: `__subclasses__`; the synthetic `__dict__` dunder entries
      (`__module__`/`__weakref__`/…); MRO-inconsistency detection.
- [x] **Iteration protocol** — FIXED: `__iter__`/`__next__` (lazy when `__iter__`
      returns a native iterator, else materialized), `__getitem__(0..)`-fallback
      iteration, `__contains__` (with iterate-and-compare fallback), and
      `__reversed__` (plus `__getitem__`+`__len__` reverse) all work — for `for`,
      comprehensions, `list()/tuple()/set()/sum()/max()/sorted()`, and `in`. The
      shared `host::iter_instance_items` drives the whole protocol.
- [x] **`__call__` dispatched** — FIXED: an instance whose class defines `__call__`
      is callable via `invoke`; `callable(obj)` reflects it (and now also reports
      `True` for partial/lru_cache/namedtuple/static+classmethod callables).
- [x] **Descriptor protocol** — FIXED (see `property` row above): `__get__`/`__set__`/
      `__set_name__` fire; data-vs-non-data precedence honored.
- [ ] **Attribute-hook dunders** — `__getattr__` (fires when normal lookup fails),
      `__getattribute__`, `__setattr__`, and `__delattr__` all FIXED and dispatched.
      Still inert: `__dir__` — `dir(obj)` returns the class/instance dict rather than
      the user hook's list.
- [x] **`__new__`** — FIXED: `instantiate` calls a user `__new__(cls, *a)` (implicit
      staticmethod) to build the instance; `object.__new__(cls)` allocates a bare
      instance; `__init__` runs only when `__new__` returned an instance of the class
      (or subclass), matching `type.__call__`.
- [x] **`__bool__` / `__len__` truthiness** — already dispatched (b_truthy).
- [x] **f-string / `.format` honor `__format__`** — FIXED: `format_field` (shared by
      f-strings, `str.format`, and the `format()` builtin) dispatches `__format__(spec)`;
      `!r`/`!s`/`!a` conversions dispatch `__repr__`/`__str__`. `str.format` now parses
      the `!conv` field syntax too.
- [x] **`NotImplemented` + `__ne__` from `__eq__` + unary dunders** — FIXED:
      `PyObj::NotImplemented` singleton resolves as a name and is honored by the
      comparison/arith dispatch (forward → reflected → identity for `==`/`!=`,
      `TypeError` for ordering/arith). Default `__ne__` derives from `__eq__`.
      `__neg__`/`__pos__`/`__invert__`/`__abs__` dispatched. (`__iadd__`/`__divmod__`
      still open.)
- [ ] **Context managers** — multiple `with` now exit **LIFO**, `__exit__` returning
      `True` **suppresses**, and `__exit__` receives the live `(type, value, None)` on
      the error path: all FIXED. Still open: parenthesized `with (a as x, b as y)` is a
      `SyntaxError: expected ')' but found Name("as")` — the parser does not accept the
      parenthesized with-items form (CPython 3.10+, via its PEG parser).
- [x] **`__slots__` enforced** — FIXED: a fully-slotted instance (every user class
      in its MRO declares `__slots__`) rejects assignment of an undeclared attribute
      (`… object has no attribute 'z' and no __dict__ …`) and has no `__dict__`; a
      non-slotted base restores the dict (no restriction). Still open:
      `a.__class__ = B` reassignment.
- [x] **`__init_subclass__` (PEP 487)** — FIXED: after a class is built and its
      `__set_name__` hooks fire, the parent's `__init_subclass__` (an implicit
      classmethod, resolved along the new class's MRO strictly after itself) is
      called with the new class and the leftover class-header keywords
      (`class C(P, tag="x")`). Only-`object` default + extra keywords raises
      `C.__init_subclass__() takes no keyword arguments`. Class-header keywords now
      flow through `BUILD_CLASS` (arity 4→5, cache schema v11) as a dict.
- [x] **`dataclasses` / `enum`** — FIXED: both arrive over the `stdlib-ffi` bridge and
      build native-backed classes. `@dataclass` on a pythonrs class mirrors it via
      `types.new_class` (`P(1,2)` reprs as `P(x=1, y=2)`); `class C(Enum)` is built by
      the real metaclass (`C.R.name`/`C.R.value` correct).

## Tier 4 — Numeric core (silent-wrong values — highest correctness priority)

- [x] **`int` arbitrary-precision consistency** — FIXED for `<< >> & | ^ ~`,
      comparison `<`, `int(float)`, `hex()`/`oct()`/`bin()`, `abs()`, and int-string
      parsing (base prefixes + underscores): all route through the BigInt path.
      `1<<64`→`18446744073709551616`; `10**20 < 10**20+1`→`True`; `int(1e20)`→bignum;
      `~(10**20)`, `(10**30)&7`, `hex(10**20)`, `abs(-(10**20))` all correct.
      `// % **` and 3-arg `pow` were already bignum. `bool` bit-ops now return `bool`.
- [x] **Floor `//` / modulo `%` follow Python floor, not C truncation** — FIXED and
      byte-verified: `7//-2` → `-4`, `-7%-100` → `-7`, `divmod(7,-2)` → `(-4, -1)`.
- [x] **Float `repr` uses scientific notation and keeps `.0`** — FIXED via the
      shortest-round-trip + exponent-threshold formatter: `1e16` → `1e+16`, `1e-05` →
      `1e-05`, `3.0` → `3.0`, `1.5e300` → `1.5e+300`, and `format(1.234e3, ".3e")` →
      `1.234e+03`. The one residual is the dtoa tie-break noted in BUGS.md.
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
- [x] **`0 ** <negative>`** — FIXED: raises `ZeroDivisionError` (int base: `zero to
      a negative power`; float base: `0.0 cannot be raised to a negative power`)
      instead of returning `inf`. (Was the last mixed-fuzz divergence.)
- [x] **Complex arithmetic** — FIXED: `int op complex`/`complex op complex` for
      `+ - * / **` route through `complex_val`/`c_pow` (CPython `complex_pow`:
      exact `c_powi` repeated-squaring for small integral exponents, polar
      `_Py_c_pow` otherwise); `complex("1+2j")`/`"-2j"`/`"(1+2j)"`/`"j"` parsing
      (CPython last-non-exponent-sign split); `.real`/`.imag`/`.conjugate()`,
      `abs(complex)`, and a negative real base to a fractional power → complex
      root (`(-8)**(1/3)`). Complex `==` (real+zero-imag unifies with the real
      number), `bool`, and hashing (`PKey::Complex`; zero-imag normalizes to the
      real key) all work. `complex(1,2)` repr `(1+2j)` (integral parts drop `.0`).
- [x] **`frozenset` real hashable type** — FIXED: `PyObj::Frozenset` (same storage as
      `set`, immutable) + `PKey::Frozenset` (element keys sorted/deduped → canonical, so
      equal frozensets share one hash). Dict key / set member work; `frozenset(...)` /
      `frozenset()` repr; set algebra (`| & - ^`) returns a `frozenset` when the left
      operand is one; `isinstance` (frozenset ⊄ set, set ⊄ frozenset); `set == frozenset`
      by membership; immutable (mutators raise `AttributeError`).
- [x] **Misc:** all FIXED — `True&False` → `False` (a real `bool`); `to_bytes`/
      `from_bytes`/`bit_count`/`as_integer_ratio`/`.hex`/`fromhex`/`numerator`/
      `denominator`/`__index__` all present; `int("0x1F",16)` → `31`;
      `float("1_000.5")` → `1000.5`; `10//0` raises
      `ZeroDivisionError: division by zero`, matching python3 verbatim.

## Tier 5 — Strings / bytes / formatting

- [x] **`%`-operator formatting** — the mini-language (flags/width/precision/`*`/
      `%(name)s`/all conv chars, incl. `%a` ascii-escaped) works and `str % obj` is
      native `str.__mod__`, authoritative over a right operand's `__rmod__` (CPython
      never returns `NotImplemented` from `str.__mod__`). **FIXED:** `%s`/`%r`/`%a` of
      a *user instance* (and of a container holding instances) now dispatch its
      `__str__`/`__repr__`/`ascii(repr)` — `b_binop` pre-resolves each format arg's
      dispatched `(str, repr, ascii)` *outside* the host borrow into a table keyed by
      heap id and threads it into `str_format_percent`/`format_conv`, so the formatter
      no longer prints `<C object>`. Covers `%` and `%=`. Byte-verified vs CPython
      (instance, container, mixed tuple, mapping, width/precision).
- [x] **`str.format` / f-string advanced spec** — **f-string nested field specs FIXED:**
      `f'{x:{w}.2f}'`/`f'{3.14159:{5}.{2}f}'`/`f'{42:>{width}}'` expand the `{…}` inside
      the spec as their own replacement fields at runtime (spec is compiled as a mini
      joined-string; cache SCHEMA→12), byte-verified vs CPython. **`str.format` nested
      field specs + keyword/index/attribute fields FIXED:** `'{:{}}'`/`'{:.{}f}'`/
      `'{:>{width}.{prec}f}'` now evaluate the `{…}` inside the spec and splice it in
      (a brace-depth-aware field scanner + shared automatic-field counter, mirroring the
      f-string path); keyword `'{name}'.format(name=…)`, positional-index `'{0[1]}'`,
      subscript `'{d[k]}'`, and attribute `'{0.real}'` field access all resolve. Byte-
      verified vs CPython (`format2` fuzz mode, 0 divergences). The `=` debug specifier
      `f'{x=}'`/`f'{x = }'`/`f'{x=:.2f}'`/`f'{y=!r}'` is now supported (`conttail` fuzz
      mode, 0 divergences). (`g`/`c`/`e` type handling and sign-aware `=`/`0` fill are
      correct.)
- [x] **str method args honored** — FIXED: `"a,b,c".split(",",1)` → `["a","b,c"]`,
      `"abcabc".find("b",2)` → `4`, and `splitlines(True)`/`splitlines(keepends=True)`
      keep the line endings.
- [x] **Missing str methods** — FIXED: `partition`/`rpartition`/`rindex`/`isnumeric`/
      `isdecimal`/`istitle`/`isidentifier`/`isprintable`/`expandtabs`/`translate`/
      `format_map` (instance methods) + `str.maketrans` (static method on the `str`
      type object, like `dict.fromkeys`). All byte-verified vs CPython.
- [x] **bytes / bytearray** — real heap types, byte-verified vs CPython (`bytesops`
      fuzz mode, 0 divergences). Construction (`b'…'`, `bytes([65,66])`, `bytes(3)`,
      `bytearray(b'…')`, `bytes.fromhex`/`bytearray.fromhex`), `len`, integer indexing
      (`b[0]`→int), iteration/`list()`, slicing (`b[1:3]`/`b[::-1]`), concat (`b1+b2`,
      result type follows the left operand), repeat (`b*3`), membership (`int in b`,
      bytes-like substring `b'a' in b'abc'`), ordering (`<`/`==` incl. bytes vs
      bytearray). Str-parallel methods returning/taking bytes: `split`/`rsplit`
      (maxsplit + whitespace)/`join`/`replace`/`find`/`rfind`/`index`/`rindex`/`count`
      (start/end)/`startswith`/`endswith` (tuple + start/end)/`strip`/`lstrip`/`rstrip`/
      `upper`/`lower`/`splitlines`/`partition`/`rpartition`/`removeprefix`/`removesuffix`/
      `decode`/`hex`. `bytearray` item assignment (`ba[0]=65`) + slice assignment
      (`ba[1:2]=b'xy'`, `ba[::2]=…`) + `append`/`extend`/`pop`/`clear`. `repr` matches
      CPython quoting (single/double-quote selection; the bytearray `\'`-escape quirk).
      The `bytestail`/`conttail` fuzz modes drove the remaining str-parallel methods to
      0: `swapcase`/`title`/`capitalize`/`center`/`ljust`/`rjust`/`zfill`/`expandtabs`/
      `translate`/`maketrans`/`isX` predicates, `%`-formatting on bytes (incl. `%b`/`%s`
      dispatch of a user `__bytes__`), `del ba[i]`/`del ba[i:j]`, and the `errors=` arg
      on `decode` are all implemented.
- [x] **`str.encode` honors the codec/errors args** — FIXED:
      `"x".encode("utf-16")` → `b"\xff\xfex\x00"` (BOM + LE), matching CPython.
- [x] **`repr` doesn't escape C0 controls** (`\x00`-`\x1f`, ` `) — data-corrupting
      raw bytes leak; **`ascii()` doesn't `\x`-escape non-ASCII**; `\N{…}` named and
      `\NNN` octal string escapes not decoded.
      **FIXED:** `repr` `\xXX`/`\uXXXX`/`\UXXXXXXXX`-escapes
      non-printable chars (printable Unicode kept verbatim); `ascii()` escapes every
      non-ASCII char; lexer decodes `\NNN` octal escapes. `\N{NAME}` now decoded via the
      vendored `unicode_names2` crate — the lexer maps the name to its codepoint in normal
      AND f-strings (round-tripped through the canonical name to reject CPython-invalid loose
      matches like ` SPACE` / `GREEK_SMALL_LETTER_ALPHA`); unknown names raise CPython's exact
      `(unicode error) 'unicodeescape' ... unknown Unicode character name` and malformed
      `\N` / `\N{}` raise the matching `malformed \N character escape`.

## Tier 6 — Data structures / iterators

- [x] **Slice read bounds with negative step** — FIXED: `slice_bounds` now mirrors
      CPython's `PySlice_AdjustIndices` (negative step clamps into `[-1, n-1]`), so
      `[1,2,3,4,5][5:-2:-2]`→`[5]` and `(10,20,30,40)[5::-2]`→`(40, 20)`.
- [x] **Slice assignment & `del` slice** — FIXED: `x[i:j]=it` (contiguous splice, any
      length), `x[::k]=it` (extended, size-checked with the CPython `ValueError`
      message), `x[1:1]=it` (insert), `del x[i:j]`, `del x[::k]` all work on lists. The
      RHS iterable is materialized in `b_setitem` outside the host borrow (so a
      generator RHS is fine and never re-borrow-panics).
- [x] **`zip`/`map`/`filter`/`enumerate`/`reversed` are lazy iterators** — FIXED:
      each is a real lazy iterator object (`PyObj::Zip`/`MapObj`/`FilterObj`/
      `EnumerateObj`; `reversed` → one-shot `Iter`). Sources are held as iterators and
      pulled one item per step by the free `iter_step` (host borrow released, so an
      infinite generator source never materializes — no hang). `next()` works, they
      exhaust once, `repr` is `<zip object at 0x…>`, `type().__name__` is `zip`/`map`/….
      `enumerate(start=)` and `zip(strict=True)` (byte-exact CPython shorter/longer
      `ValueError` messages) honored.
- [x] **dict views** — FIXED: `PyObj::DictView{dict,kind}` is a live view (holds a
      handle to the backing dict, reflects mutations). `type().__name__` =
      `dict_keys`/`dict_values`/`dict_items`; repr `dict_keys([…])`; iteration, `len`,
      `in`. Keys/items views participate in set algebra (`| & - ^`, via `setmap_of`),
      returning a `set`. `dict.fromkeys(iterable[, value])` (reached on the `dict` type
      object), `dict | dict` merge (right wins), `d |= …`, and `d.update(mapping |
      pairs-iterable, **kwargs)` all work.
- [x] **`range`** — FIXED: slicing yields a new `range` (`range(10)[2:8:2]`→
      `range(2, 8, 2)`, never materializes), `.index`/`.count` (O(1) arithmetic), and
      value equality (`range(10)==range(0,10)`→True; two ranges equal iff same length
      and same start/step when non-trivial). O(1) membership was already done.
- [x] **set** — FIXED: subset partial-order comparisons `<= >= < >` (in `compare`,
      before the total-order path, so incomparable sets yield False both ways),
      `isdisjoint`, and `intersection_update`/`difference_update`/
      `symmetric_difference_update` (all accept any iterable via `iter_keys`).
      `issubset`/`issuperset` now also accept any iterable.
- [x] **`type([])`/`type({})`/… print `<class 'list'>`** — FIXED, and the instance
      dunders resolve too: `[].__class__` → `<class 'list'>`, `[].__len__()` → `0`,
      unbound `str.lower("AB")` → `"ab"`.
- [x] **set repr ordering** — FIXED for the deterministic subset: `set`/`frozenset`
      of machine ints now repr and iterate in CPython's open-addressing table order
      (`setobject.c` faithful port — `set_add_entry` perturb+`LINEAR_PROBES`, the
      `fill*5 >= mask*3` grow trigger, `used*4` resize target, and `set_insert_clean`
      reinsertion; `hash(n) == n` bar `hash(-1) == -2`). `{3,1,2}` → `{1, 2, 3}`,
      `set([9,1,17,25,33])` → `{33, 1, 9, 17, 25}`, verified 0-diff vs `python3`
      across 120+ random int sets and every `set(iterable)` form. Boundary (noted,
      not faked): (a) string/other-object sets stay in insertion order — CPython
      SipHash-randomizes those per process, so no fixed order matches byte-for-byte;
      (b) a *constant* set **literal** with 5+ colliding ints (e.g. `{9,1,17,25,33}`)
      can differ, because CPython's compiler folds a constant set display to a
      presized `frozenset` constant, which lays out differently than the incremental
      build pythonrs (and `set(list)`) performs. Tuple/frozenset `hash()` values still
      differ (not observable in repr).

**Corpus-caught composite gaps** (found by `dropin_check.sh`, not the per-expression fuzzer):
- [x] **`sorted`/`.sort(key=…)` is stable on ties** — FIXED:
      `sorted([('alice',30),('carol',25),('bob',25)], key=…)` →
      `[('carol', 25), ('bob', 25), ('alice', 30)]`, byte-identical to python3.
- [x] **`json.dumps(sort_keys=True)`** — FIXED: the real CPython `json` arrives over
      the bridge, so `dumps({"b":1,"a":2}, sort_keys=True)` → `{"a": 2, "b": 1}`.
- [x] **A native `open()` handle passed INTO a CPython stdlib call** — FIXED:
      `json.dump(cfg, f)`, `json.load(f)`, `csv.writer(f)` and `csv.DictReader(f)`
      used to raise `TypeError: cannot pass 'TextIOWrapper' to a CPython stdlib
      call`. The marshaler now wraps a `PyObj::File` as a `PyrsFile` proxy whose
      `read`/`write`/`__next__`/`seek`/`tell`/`name`/`mode`/`closed` route back to
      the native handle, so the stdlib reads and writes the real file.
- [x] **File-object surface**: `f.read(n)` (n *characters*, never splitting a
      multi-byte one), `f.seek`/`f.tell`/`f.truncate`/`f.fileno`/`f.isatty`, and
      the data attributes `f.name`/`f.mode`/`f.closed`/`f.encoding`. `readable()`
      /`writable()` now report the mode the handle was opened with instead of
      always `True`, a binary handle types as `_io.BufferedReader`/`BufferedWriter`
      /`BufferedRandom`, and the `repr` carries the real mode string.

## Tier 7 — Functions / generators / async / exceptions / control flow

`*args`/`**kwargs` (def + call unpacking), closures/`nonlocal`, decorators (stacked,
with-args), lambdas, generator basics + genexpr laziness, `match`/`case` (all pattern
kinds + guards), `for/else`/`while/else`, `try/except/else/finally` ordering all **work**.

- [x] **Generator `.send()` / `.throw()` / `.close()`** — FIXED: `send` feeds the
      value into the `yield` expression (rejects a non-`None` value into a
      just-started generator); `throw` queues an exception raised at the suspended
      `yield` point (`gen_yield` checks `pending_throw`), catchable by the body;
      `close` throws `GeneratorExit`, runs `finally`, and swallows the clean exit.
- [x] **`yield from` delegated value + `StopIteration.value`** — FIXED: the body's
      `return X` is captured into the generator's `ret_value`; `StopIteration.value`
      exposes it, `next()`/`send()`/`__next__` raise `StopIteration(value)` on
      exhaustion, and `yield from` lowers to the new `GENRET` op so the expression
      evaluates to the sub-generator's return value. (Sent-value forwarding through
      `yield from` still not plumbed.)
- [x] **`async`/`await`/`asyncio` on a native fusevm event loop** — FIXED: `async def`
      returns a real coroutine object (body does not run on call), backed by the
      generator/`corosensei` infra with each `await` a suspension point. `await` drives
      coroutines / `asyncio.Future`s / `__await__` objects, suspending up to the driving
      Task until the awaitable settles. Native ready-queue + timer-heap event loop
      (`src/async_rt.rs`, virtual clock) powers `asyncio.run`/`sleep`/`gather`/
      `create_task`/`ensure_future`/`wait_for`/`get_event_loop`/`Future` — byte-verified
      vs CPython (coroutine type, ordered gather, task interleaving, Future set_result +
      await, cross-await exception propagation, sleep timer ordering). `async for`
      (`__aiter__`/`__anext__`, `StopAsyncIteration`, `for…else`), `async with`
      (`await __aenter__`/`__aexit__`), and async comprehensions
      (`[x async for x in ag()]` + set/dict + `if` filters) all lowered and byte-verified.
      `asyncio.wait`/`as_completed`/`Event`/`Lock`/`Queue` are implemented natively too
      (`async with lock`, producer/consumer `Queue`, `Event.wait`/`set`). Async generators
      (`async def` with `yield` → `async_generator` with `__aiter__`/`__anext__`, driving
      awaits to the loop between yields) power `async for`/comprehensions over real async
      generators. **Still pending:** task cancellation injection, bounded-`Queue`
      back-pressure, `wait` timeout/`return_when`, async-gen `asend`/`athrow`/`aclose`.
- [x] **Bare `raise` re-raise** — FIXED: the except handler now keeps the caught
      exception as the "currently handled" one (`h.exc`) while its body runs, so a
      bare `raise` re-raises it (caught by an outer handler); it is cleared when the
      handler finishes without raising. (`b_try`.)
- [x] **Exception chaining** — FIXED: per-exception `__cause__`/`__context__` live in
      a heap-index-keyed side table (`PyHost.exc_links`). `raise X from Y` wires
      `__cause__` (and `__suppress_context__`→True); raising inside a handler sets the
      implicit `__context__` to the exception being handled (`h.exc` captured before
      the new raise overwrites it). Both readable on builtin `Exception` objects and on
      user exception instances (gated by `class_is_exception` so non-exception objects
      still `AttributeError`). Still open: `ExceptionGroup` (though `except*` parses).
- [x] **User exception subclasses inherit `BaseException.__init__`/`__str__`/`.args`**
      — FIXED: a `class E(Exception)` instance now behaves like a builtin exception.
      Construction seeds `self.args = tuple(ctor_args)` (`BaseException.__new__`);
      `super().__init__(*a)` overrides it, and `self.args = …` works too. `str(e)` is
      the message (`''` / `str(arg)` / `repr(tuple)`), `repr(e)` is `E(arg, …)`, and
      `.args` reads the tuple (`host::exc_instance_args`). `BaseException.__str__`
      (the message) wins over a user `__repr__` in `str()`. Uncaught `raise E('x')`
      prints `E: x`. (host `str_of`/`repr_of` exception-instance arms + `py_str`
      precedence + `instantiate_plain`/`super().__init__`/`raise_value` seeding.)
- [x] **Keyword-only default values** — FIXED: `MKFUNC` now carries the evaluated
      keyword-only defaults (a count + values below the func id; cache schema v5);
      `bind_params` applies them for any omitted optional kwonly param. Works for
      `def`, `lambda`, methods.
- [x] **Positional-only enforcement** — FIXED: `FuncDef` carries a `posonly` count
      (cache schema v8); `bind_params` never binds a positional-only param by keyword
      (a same-named keyword falls through to `**kwargs` or raises CPython's
      `got some positional-only arguments passed as keyword arguments: 'a, b'`).
- [x] **Walrus in a comprehension leaks** to the enclosing scope — FIXED: the
      compiler collects every `:=` target in a comprehension's element/value/`if`
      clauses (not its iterables) and injects a `global`/`nonlocal` declaration at
      the top of the hidden comp function, chosen by the enclosing real-scope depth
      (`Compiler.fn_depth`: module → `global`, function → `nonlocal`). The
      comprehension result is unchanged; the `:=` target binds in the enclosing
      scope (`list`/`set`/`dict` comps), and stays unbound if never assigned
      (empty iterable). Cache schema bumped to v9 (comp bytecode changed).

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

## parity-fuzz snapshot — 2026-08-02 (50,000 cases)

Oracle: reference `python3` (3.14.6). Mixed mode, 10 workers.
**50,000 checked → 0 divergences (0 known / 0 new), 21 timeouts, 417.6s at 120/s.**

The four classes that dominated the 2026-07-19 snapshot (1,164 divergences, 2.3%)
are all closed, and each root cause is checked off in the tiers above:

| class | then (~share) | root cause | now |
|---|---|---|---|
| `str.format('{}', float)` / scientific | ~442 | float `repr` had no scientific notation, dropped `.0` | fixed — Tier 4 |
| `'%…' % x` format specs | ~338 | `%`-operator specs unimplemented | fixed — Tier 5 |
| `pow(a,b,m)` | 188 | 3-arg modular pow ignored the modulus | fixed — Tier 4 |
| `//` / `%` sign | ~140 | C-truncation instead of Python floor | fixed — Tier 4 |

Re-measure: `cargo build && ./target/debug/parity-fuzz --count 50000`.
Replay one: `./target/debug/parity-fuzz --once --seed <N>`.
Per-mode: `--<mode>`, one of the 53 real modes (`mixed` rotates all of them) —
arith, bignum, floatfmt, strings, fstring, slice, listcomp, dictcomp, setcomp,
sorting, formatspec, boolint, ranges, strmeth, comparison, builtins, ternary,
augassign, classes, iterproto, generators, exceptions, unpacking, comprehension,
dictset, itertools, complexnum, numedge, exceptions2, exceptions3, exceptions4,
closures, oop2, strfmt2, bytesops, bytestail, format2, strformat, async, async2,
augwith, descriptors, attr, calls, match, conttail, itertail, metatype, seqtail,
display, scoping, codec, subclass.

The generated programs import only `sys` and `asyncio` (both native arms), so no
case ever crosses the CPython bridge — every bridge defect is structurally
invisible to `parity-fuzz` and has to be caught by `scripts/dropin_check.sh`
(which is exactly how the `TextIOWrapper`-into-a-stdlib-call gap surfaced).

**Object-model modes added 2026-07-19** (`classes`, `iterproto`, `exceptions`) —
each generates deterministic-stdout programs exercising the OOP surface and is in
the `mixed` rotation. Trajectory to 0: `classes` 15→0 (fixed `bool()`/`any`/`all`
not dispatching `__bool__`/`__len__`), `iterproto` 0, `exceptions` 0. After the
`0 ** -1` → `ZeroDivisionError` fix, **mixed 8,000 = 0 divergences**; each new mode
at 3,000 = 0.

**Language-core modes added 2026-07-19** (`unpacking`, `comprehension`, `dictset`,
`itertools`, `complexnum`, `exceptions2`) — cover starred/nested/spread unpacking,
list/set/dict/nested comprehensions + genexprs, dict views + set algebra +
frozenset, the lazy iterators driven via `next()`/`list()` (incl. an infinite
generator source), complex arithmetic, and `raise from`/implicit-context chaining.
All outputs are order-deterministic (sets always `sorted`). Each drove to **0**
(unpacking/comprehension/dictset at 1,500; itertools/complexnum/exceptions2 at 800),
and a **mixed 4,000 run = 0 divergences** with the new modes in rotation.

**Object-model / closure / format modes added 2026-07-19** (`exceptions3`,
`closures`, `oop2`, `strfmt2`) — cover user exception subclasses (args/str/repr/
message/inheritance/`isinstance`/`super().__init__`/custom `__str__`), nested
functions + `nonlocal` + late-binding loop captures + decorators-with-args +
`*args`/`**kw` wrappers + multi-level lexical capture, multiple-inheritance MRO +
attribute order + `super()` in a property + `__init_subclass__` class-kwargs +
classmethod alt-constructors, and `!r`/`!s`/`!a` on containers + positional
`.format` reuse + `%`-format tuples + int format specs. All outputs are
deterministic (plain values, no instance-`%` dispatch, no nested-field spec).
Bugs found + fixed while driving to 0: sign-aware zero-pad placed the fill before
the sign (`+05d` of 5 → `000+5`, now `+0005`) and the space sign flag didn't
prefix non-negative values. Each mode drove to **0** (exceptions3/closures at
2,000; oop2/strfmt2 at 1,500).

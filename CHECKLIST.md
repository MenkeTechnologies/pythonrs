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
- [x] **`itertools.islice` is eager → hangs on infinite generators** — RESOLVED via
      the `stdlib-ffi` bridge: `itertools` is now the real CPython module, so
      `islice`/`count`/`cycle` are natively lazy and `islice(count(), 5)` returns.
      (The old hand-rolled eager `itertools` shadow was deleted.)
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

**`subprocess`, `pathlib`, `io` (`StringIO`/`BytesIO`), and the full `os` surface
are CLOSED via the `stdlib-ffi` bridge** (real CPython modules). `open()` + file
objects are native (landed). Items below track the native default-build surface.

- [ ] **`open()` missing** — `NameError: name 'open' is not defined`. No read/write/
      append/`with open(...)`/line iteration. Single largest drop-in blocker.
- [ ] **File objects** `[in-flight]` — `.read/.readline/.readlines/.write/.writelines/
      .close/.seek/.tell`, iteration, text vs binary, encodings, `__enter__/__exit__`.
- [ ] **`subprocess`** — `ModuleNotFoundError`. `run/Popen/check_output/PIPE`, rc.
- [ ] **`os` expansion** — beyond the current POSIX subset: `environ` mutation,
      `listdir/scandir/walk/makedirs/remove/rename/chdir`, more `os.path`.
- [ ] **`pathlib`**, **`io`** (`StringIO`/`BytesIO`) — `ModuleNotFoundError`.

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
len/str/repr/membership route back through the bridge. A pythonrs callable passed
as a stdlib callback (`functools.reduce(f, …)`, `sorted(key=f)`) is wrapped so
CPython calls back into fusevm. Verified byte-identical to `python3` (3.14.6):
`re.findall`, `hashlib.sha256`, `argparse`, `json.dumps/loads`, `textwrap`,
`itertools.chain/combinations/permutations`, `functools.reduce/partial/lru_cache`,
`os.path.*`, `string.*`.

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
- [ ] **Attribute-hook dunders** — `__getattr__` FIXED (fires when normal lookup
      fails). Still inert: `__getattribute__`/`__setattr__`/`__delattr__`/`__dir__`.
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
- [ ] **Context managers** — multiple `with` exit **FIFO not LIFO**; `__exit__`
      returning `True` does **not** suppress; `__exit__` receives `(None,None,None)`
      even on exception. Parenthesized `with (a as x, b as y)` is a `SyntaxError`.
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
- [ ] **Misc:** `bool` bit-ops return `int` not `bool` (`True&False`→`0`); int/float
      methods `to_bytes/from_bytes/bit_count/as_integer_ratio/.hex/numerator/denominator/
      __index__` absent; `int("0x1F",16)` rejected; underscores in `float("1_000.5")`
      rejected; `10//0` message wording.

## Tier 5 — Strings / bytes / formatting

- [x] **`%`-operator formatting** — the mini-language (flags/width/precision/`*`/
      `%(name)s`/all conv chars, incl. `%a` ascii-escaped) works and `str % obj` is
      native `str.__mod__`, authoritative over a right operand's `__rmod__` (CPython
      never returns `NotImplemented` from `str.__mod__`). **Deferral:** `%s`/`%r`/`%a`
      of a *user instance* does NOT dispatch its `__str__`/`__repr__` (the `%`
      formatter runs inside the host borrow and can't call back into user code) — it
      prints `<C object>` where CPython uses the dunder. Exception instances format
      correctly (host-side arms). f-strings and `str.format` DO dispatch these dunders
      (`format_field` runs out of borrow); prefer them. Fixing `%` faithfully requires
      moving the formatter out of the borrow like `format_field`.
- [ ] **`str.format` / f-string advanced spec** — nested fields `'{:{}}'`/`'{:.{}f}'`
      (and f-string `f'{x:{w}.2f}'`) drop the spec; keyword `'{name}'`, index `'{0[0]}'`,
      attribute `'{0.imag}'` fields → `None`; the `=` debug specifier `f'{x=}'` is a **`SyntaxError`**;
      `g` type treated as fixed precision (never switches to exponent / strips zeros);
      `#` alt form, `c` type, `=` sign-aware fill, and `e` exponent (`1.2e5` want
      `1.2e+05`) all wrong.
- [ ] **str method args silently ignored** — `split`/`rsplit` maxsplit, `find`/`index`
      start, `splitlines(keepends)` all ignored → wrong values, no error.
- [x] **Missing str methods** — FIXED: `partition`/`rpartition`/`rindex`/`isnumeric`/
      `isdecimal`/`istitle`/`isidentifier`/`isprintable`/`expandtabs`/`translate`/
      `format_map` (instance methods) + `str.maketrans` (static method on the `str`
      type object, like `dict.fromkeys`). All byte-verified vs CPython.
- [ ] **bytes / bytearray non-functional** — `b'hello'` evaluates to an **empty
      string**; `len(b'hello')`→`0`; indexing/iteration/slicing/all methods broken;
      `bytes([65,66])`→`b''`; `bytes.fromhex`/`.hex()`/`.decode()` missing; `bytearray`
      undefined. `[in-flight]` Blocks binary I/O + `hashlib`/`base64`.
- [ ] **`str.encode` ignores the codec/errors args** — always UTF-8 (`'x'.encode('utf-16')`
      wrong).
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
- [ ] **`type([])`/`type({})`/… print `<built-in function list>`** not `<class 'list'>`;
      instance dunders `[].__class__`/`[].__len__()` and unbound `str.lower` unavailable.
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
- [ ] **`sorted`/`.sort(key=…)` is not stable on ties** — Python guarantees a stable
      sort; pythonrs reorders equal-key elements (`[('alice',30),('carol',25),('bob',25)]`
      → order of the two `25`s not preserved). Use a stable algorithm.
- [ ] **`json.dumps(sort_keys=True)` ignored** — emits insertion order instead of
      sorted keys. Common in config/serialization round-trips.

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
- [ ] **Async is non-functional** — `async def` executes eagerly and returns a plain
      value (no coroutine); `asyncio` `ModuleNotFoundError`; **async comprehensions are
      a `SyntaxError`**; `await` is a passthrough. Anything using an event loop fails.
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
builtins, ternary, augassign, classes, iterproto, exceptions, **unpacking,
comprehension, dictset, itertools, complexnum, exceptions2**).

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

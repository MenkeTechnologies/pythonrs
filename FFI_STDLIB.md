# pythonrs stdlib via CPython FFI — implementation spec (turnkey)

**Decision:** pythonrs does NOT reimplement the stdlib. It imports the real CPython
stdlib — pure `.py` **and** C-accelerator `.so` modules — over an FFI bridge to an
embedded `libpython`. User code runs on fusevm (JIT/rkyv/AOT); `import <stdlib>`
delegates to CPython.

## Validated (isolated spike — proven, do not re-litigate)
- **pyo3 0.24** with feature `abi3-py313` + env `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`
  builds/links against the system CPython **3.14.6** via the stable ABI. (Drop the flag
  when pyo3 ships native 3.14; abi3 keeps one binary compatible across CPython minors.)
- **Import sweep: 61/61 modules** load — pure (`argparse csv textwrap dataclasses enum
  pathlib json logging http email xml…`) and C-accel (`re/_sre hashlib/_hashlib
  datetime/_datetime socket/_socket struct math random pickle/_pickle base64/binascii
  zlib itertools`). C code runs, results marshal back to Rust (bytes/list/tuple/dict/
  int/float/str): `hashlib.sha256(b"abc")`→correct, `Decimal("0.1")+Decimal("0.2")`→`0.3`
  exact, `struct.pack(">I",1000)`→`[0,0,3,232]`, `pickle` roundtrip, `argparse` parse.
- **Stdlib resolution** proven both ways via `PYTHONHOME`/`sys.prefix` (set before init):
  - system: no override → uses installed CPython's `Lib/`.
  - bundled: `PYTHONHOME=<bundle>` → loads `<bundle>/lib/python3.14/` + `lib-dynload/`.

## Implementation (feature-gated so it never breaks default/peer builds)

1. **Cargo** — optional dep + feature (default OFF):
   ```toml
   [dependencies]
   pyo3 = { version = "0.24", features = ["abi3-py313", "auto-initialize"], optional = true }
   [features]
   stdlib-ffi = ["dep:pyo3"]
   ```
   Default `cargo build`/`test`/`clippy` are unaffected (no libpython needed). CI adds one
   job: `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo build --features stdlib-ffi`.

2. **`src/ffi.rs`** (`#[cfg(feature = "stdlib-ffi")]`):
   - `init()` once at startup: resolve the stdlib prefix (order: `PYTHONRS_STDLIB` env →
     bundled `<exe_dir>/../lib/python3.14` → system CPython → error), set `PyConfig.home`
     / `PYTHONHOME` before `Py_Initialize`.
   - `import(name) -> Result<ForeignHandle, String>`: `Python::with_gil(|py| py.import(name))`,
     store the `Py<PyAny>` in a host side-table, return an id.
   - Marshal helpers: pythonrs `Value` ↔ CPython object. By value in *both*
     directions for int/float/bool/None/str/bytes/list/tuple/dict/set, plus (in) a
     bytearray→CPython `bytearray`, range, complex, `collections.deque`, and
     frozenset. By handle (`PyObj::Foreign`) for everything else (compiled regex,
     datetime, socket, file, …). **In-place mutation write-back:** after a call, a
     by-value mutable-container argument (`list`/`bytearray`/`deque`) is re-read from
     its CPython object and the pythonrs heap slot is overwritten in place, so
     in-place stdlib mutators (`heapq.heapify`, `random.shuffle`, `struct.pack_into`)
     reflect back and aliases observe them. Write-back marshals by value only (never
     allocates a `Foreign`), so it does not grow the side-table.
   - **Handle lifetime (known limit):** the side-table is bounded for the
     value-marshaled path but *not* reclaimed for stdlib calls that return a live
     CPython object (`re.match` results, datetime, files) — each takes a permanent
     slot, growing 1:1 with the pythonrs host heap. The host heap is an arena that
     never frees any object and `PyObj::Foreign` carries only a bare id, so the
     bridge has no drop signal and cannot safely reclaim. Real reclamation needs a
     `Foreign`-drop callback / arena GC in `host.rs` (out of the bridge's scope).

3. **`PyObj::Foreign(u32)`** (`#[cfg(feature)]` variant → id into the ffi side-table).
   Route `get_attr`/`call`/`__getitem__`/`__iter__`/`__next__`/`str`/`repr`/`len`/
   `__contains__` on a Foreign through pyo3 (marshal args in, result out). pyo3 owns
   refcounts + the GIL. Add `#[cfg(feature)]` arms to the PyObj matches (type_name,
   str_of, repr_of, truthy, get_attr, dispatch, invoke). **Binary / comparison /
   unary operators** on a Foreign operand (`+ - * / // % ** @ & | ^ << >>`,
   `== != < <= > >=`, unary `- + ~ abs`) route through `ffi::binary_op`/`unary_op`,
   which marshal both operands (a native operand crosses by value) and call
   CPython's `operator.<fn>`; the result marshals back by value or as a fresh
   `Foreign`. Minimal `#[cfg(feature)]` hooks live at the top of `PyHost::arith`
   (`+ - *`, comparisons, unary `-`), `PyHost::binop` (`/ // % ** @ & | ^ << >>`),
   `PyHost::unary` (`~`, unary `+`), and the `abs` builtin. A CPython
   `TypeError`/`NotImplemented` surfaces as a pythonrs error, never a panic.

4. **`host::import_module`** — on the current miss (before `ModuleNotFoundError`), if
   `stdlib-ffi`, try `ffi::import(name)` → wrap as a `Module` whose attrs are Foreign
   proxies (or a Foreign module handle). `from x import y`, submodules (`os.path`),
   `sys.modules` all fall out of CPython's own importer.

5. **Delete the remaining hand-rolled shadows** in the SAME commit that turns the bridge
   on (so no regression window): `src/stdlib/{json,os,random,string,itertools,functools,
   statistics,textwrap}.rs` + their `import_module`/`call_builtin_function`/
   `is_builtin_function` wiring + `mod.rs`. Keep only genuinely-native pieces (fusevm
   runtime `sys.argv`/`sys.exit`, `math` if kept native). (`re/datetime/heapq/bisect`
   already deleted.)

6. **Bundle packaging** (the "install stdlib with it" path): release artifact ships
   `lib/python3.14/` (or zipped `python314.zip`) + `lib/python3.14/lib-dynload/*.so` +
   `libpython3.14.dylib`; AOT standalone binaries bundle all three. Homebrew: either
   `depends_on "python@3.14"` (system) or bottle the bundle.

## Remaining language gaps (loop, gated on session-limit reset)
Exception chaining (`__cause__`/`__context__`, `raise X from Y`); lazy `zip`/`map`/
`filter`/`enumerate` (+ infinite-`islice`); `frozenset` real type; dict-view set-ops +
`range`/`set` methods; slice assignment/`del`; remaining str methods; `repr` control-char
escaping; positional-only enforcement; metaclasses. (Complex arithmetic, `super`/C3,
property/descriptors, iteration protocol, generators send/throw/close, banker's round,
bignum, numeric-key unification — DONE; parity-fuzz at 0 across all modes.)

# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython 3.14.6 on the example corpus). This file is the
honest list of what is **not** yet covered, so nobody mistakes a gap for a bug
fixed. Every line below was re-checked against the **default-build** binary
(`cargo build`, no features) before being written.

## Implemented (previously listed here as gaps)
- **Generators / `yield`.** A `def` whose body contains `yield` builds a real
  lazy generator, backed by a stackful `corosensei` coroutine on the same thread
  (the thread-local `PyHost` is shared across suspend/resume via a swapped
  execution context). Supported: `for x in gen()`, `next(g)`, `list(gen())`,
  `yield`-expression value, and `yield from` — including the delegate's `return`
  value (`r = yield from sub()` binds `sub`'s return). Generator expressions
  `(x for x in xs)` are **lazy** (a hidden generator function), not eager.
- **Call-site unpacking** `f(*args, **kwargs)`, `f(a, *b, c, **d)` — flattened at
  runtime through `BUILD_ARGS`/`BUILD_KWARGS` and the `CALL*_EX` ops.
- **Literal spreads** `[*a, *b]`, `(*a, b)`, `{*a, *b}`, and dict `**`-spread
  `{**a, "k": 1, **b}` (later keys override; `None` stays a valid key).
- **`match`/`case`** (PEP 634): literal, capture, wildcard `_`, sequence
  `[a, *rest]`, mapping `{"k": v, **rest}`, class `Point(x=0)` (via
  `__match_args__` + builtin-type self-match), OR-patterns `a | b`, `as`
  bindings, and `if` guards.
- **`nonlocal`** rebinds the nearest enclosing FUNCTION scope that binds the name
  (distinct from `global`, which targets module scope).
- **User exception subclasses** inherit `BaseException`: `class E(Exception)`
  instances carry `args` (seeded by construction / `super().__init__` / direct
  assignment), stringify to the message (`''`/`str(arg)`/`repr(tuple)`), repr as
  `E(arg, …)`, and expose `.args`; `str()` uses the message even when a user
  `__repr__` exists. An uncaught exception prints CPython's
  `Traceback (most recent call last):` block — header, `  File "<path>", line N,
  in <scope>` + source line per frame (outermost first), then `ErrorType: message`
  (caret `^^^` markers omitted for now). **Exception chaining**
  `raise X from Y` records `__cause__` (verified: `v.__cause__` is the `from`
  operand).
- **Object model**: `complex` (`(1+2j)*(3-1j)`, `.real`/`.imag`, `abs`),
  `frozenset` (immutable, hashable, set algebra), **metaclasses**
  (`class A(metaclass=M)`, `M.__new__`/`__init__`; `type(A) is M`), `property`
  getters/setters, custom **descriptors** (`__get__`/`__set__`), `super()` +
  **C3 MRO** (`C.__mro__` linearization), and **`__init_subclass__` (PEP 487)**
  (parent hook fires with the new class and class-header keywords).
- **Instances are hashable** as dict keys / set members via a user `__hash__`
  (with `__eq__`), so `{K(1): 'a'}[K(1)]` resolves.
- **`NotImplemented`-driven reflected-op negotiation**: a forward dunder that
  returns `NotImplemented` retries the reflected dunder, for both arithmetic
  (`A().__add__` → `B().__radd__`) and comparison (`A().__lt__` → `B().__gt__`);
  when neither resolves, a `TypeError` is raised.
- **`%s`/`%r`/`%a` dispatch a user instance's `__str__`/`__repr__`/`ascii(repr)`**
  (and recurse into containers holding instances), matching f-strings/`.format`;
  the format args' dispatched values are pre-resolved outside the host borrow.
- **Nested format specs (f-string AND `str.format`)** `f'{x:{w}.2f}'` /
  `f'{3.14159:{5}.{2}f}'` / `'{:{}}'.format('hi', 10)` /
  `'{:>{width}.{prec}f}'.format(v, width=10, prec=2)`: the `{…}` inside a spec is
  evaluated as its own replacement field (sharing the automatic-field counter) and
  spliced into the final spec before formatting.
- **`str.format` keyword / index / attribute fields** `'{name}'.format(name=…)`,
  `'{0[1]}'.format(seq)`, `'{d[k]}'.format(d=…)` (unquoted subscript key → str),
  `'{0.real}'.format(x)` (attribute access) — all resolve against the positional
  args, kwargs, and accessor chain.
- **`\N{NAME}`** named-Unicode escapes decode in normal and f-strings.
- **File I/O**: `open()` (text/binary, read/write/append), `.read`/`.readline`/
  `.readlines`/`.write`, line iteration, and `with open(...) as f:` work in the
  default build.
- **`bytes`/`bytearray` are real heap types** with the full sequence + method
  surface (byte-verified vs CPython via the `bytesops` fuzz mode, 0 divergences):
  construction (`b'…'`, `bytes([65,66])`, `bytes(3)`, `bytearray(b'…')`,
  `bytes.fromhex`/`bytearray.fromhex`), `len`, integer indexing (`b[0]`→int),
  iteration/`list()`, slicing (`b[1:3]`, `b[::-1]`), concat (`b1+b2`, result type
  follows the left operand), repeat (`b*3`), membership (`int in b` byte-value,
  bytes-like substring `b'a' in b'abc'`), ordering (`<`/`==`, incl. bytes vs
  bytearray). Str-parallel methods returning/taking bytes:
  `split`/`rsplit`/`join`/`replace`/`find`/`rfind`/`index`/`rindex`/`count`/
  `startswith`/`endswith`/`strip`/`lstrip`/`rstrip`/`upper`/`lower`/`splitlines`/
  `partition`/`rpartition`/`removeprefix`/`removesuffix`/`decode`/`hex`.
  `bytearray` item + slice assignment (`ba[0]=65`, `ba[1:2]=b'xy'`, `ba[::2]=…`),
  plus `append`/`extend`/`pop`/`clear`. `repr` matches CPython quoting (single/
  double-quote selection; the bytearray always-escape-`'` quirk).
- **Comprehension scope**: list/set/dict comprehensions run in their own function
  scope, so the loop variable no longer leaks; enclosing variables are still read
  through the closure (the outermost iterable is evaluated in the enclosing
  scope, matching CPython).

## Implemented — async/await/asyncio (native fusevm event loop)
- **`async def` / `await` / `asyncio`.** `async def f()` returns a real coroutine
  object (`type(f()).__name__ == 'coroutine'`; the body does **not** run on call),
  backed by the same stackful `corosensei` coroutine as generators — each `await`
  is a suspension point. `await` drives an awaitable (a coroutine, an
  `asyncio.Future`/`Task`, or an object with `__await__`), suspending the running
  coroutine (yielding up to its Task) until it settles, then resuming with the
  result (or raising its exception). The event loop (`crate::async_rt`) is a native
  ready-queue + timer-heap with a virtual clock, single-thread and cooperative like
  CPython's. `asyncio.run`/`sleep`/`gather`/`create_task`/`ensure_future`/
  `wait_for`/`get_event_loop`/`get_running_loop`/`Future` all run on it, verified
  byte-for-byte vs CPython (coroutine type, ordered `gather` results, `create_task`
  interleaving, `Future.set_result` + await, exception propagation across `await`,
  and `asyncio.sleep` timer ordering).
- **`async for` / `async with` / async comprehensions.** `async for x in ait`
  drives `__aiter__`/`__anext__` (stopping on `StopAsyncIteration`, with correct
  `for…else` semantics); `async with cm` drives `await __aenter__` / `await
  __aexit__`; async comprehensions `[x async for x in ag()]` (and set/dict forms,
  with `if` filters) run the hidden comprehension body as an awaited coroutine —
  all byte-verified vs CPython.
  **Not yet:** async generators (`async def` containing `yield`);
  `asyncio.wait`/`as_completed`/`Queue`/`Event`/`Lock`; task cancellation
  propagation (`Task.cancel` settles the future but does not inject
  `CancelledError` into a suspended coroutine).

## Not yet implemented (compile/parse-time error, no silent wrong answer)
- **`yield from` sent values.** The delegate's `return` value is now forwarded,
  but a value `send()`-ed into the delegating generator does not reach the
  sub-generator (`x = yield` inside the delegate sees `None`, not the sent value).

## Partial / simplified semantics
- **Operator overloading dunders**: dispatched, with `NotImplemented` reflected
  fallback (see Implemented). Covered: arithmetic/bitwise
  (`__add__`/`__sub__`/`__mul__`/`__truediv__`/`__floordiv__`/`__mod__`/`__pow__`/
  `__matmul__`/`__and__`/`__or__`/`__xor__`/`__lshift__`/`__rshift__`) with their
  reflected `__r*__`, comparisons (`__eq__`/`__ne__`/`__lt__`/`__le__`/`__gt__`/
  `__ge__`), and `__getitem__`/`__setitem__`/`__len__`/`__bool__`/`__str__`/
  `__repr__`/`__iter__`/`__next__`/`__init__`/`__hash__`. Container `repr`/`str`
  recurses so instance elements/keys/values dispatch their own `__repr__`.
  **Not yet:** in-place augmented dunders — `x += y` does **not** call `__iadd__`
  (falls through to `__add__`, or `TypeError` if absent).
- **Chained comparisons** `a < b < c` re-evaluate the interior operand `b`
  (correct for side-effect-free operands; a function call in the middle runs
  twice — verified `1 < f() < 10` calls `f` twice vs CPython's once).
- **`with`** desugars to try/finally calling `__enter__`/`__exit__`; on the
  exceptional exit path the triple passed to `__exit__` is always
  `(None, None, None)`, and a `True` return does **not** suppress the propagating
  exception (verified: the exception still escapes the `with`).
- **`int`** is arbitrary precision (bignum) across `+ - * ** // %` and the bitwise
  ops `& | ^ << >>` — verified byte-identical to CPython on `10**30`-scale values
  (the earlier i64-cap on `//`/`%`/bitwise is gone).
- **`bytes`/`bytearray`** (real types — see Implemented) still lack a few string-
  parallel methods (`swapcase`/`title`/`capitalize`/`center`/`ljust`/`rjust`/
  `zfill`/`expandtabs`/`translate`/`maketrans`/the `isX` predicates),
  `%`-formatting on bytes (`b'%d' % 5`), `del ba[i]`/`del ba[i:j]`, and the
  `errors=` argument on `.decode()` (only the codec is honored).
- **f-string / `str.format` format spec** covers the common mini-language
  (fill/align/sign/width/`,`/`.prec`/type `d f e x o b % s c g`) and nested field
  specs (see Implemented). The `=` debug specifier `f'{x=}'` is still a
  `SyntaxError`.

## Tooling
- **`--dap`** (Debug Adapter Protocol): implemented — breakpoints, step
  in/out/over/continue, stack trace, locals, and program-stdout capture (pipe +
  dup2 → `output` events). Frame names in the stack use the function name (or
  `<module>`), shared with the traceback path. Watch expressions not yet added.
- **`--lsp`**: full corpus — completion (166 builtins/keywords/methods), position-
  aware hover, and diagnostics via the real parser. Go-to-def and signature help
  not yet added.
- **REPL** does not echo bare-expression values (use `print(...)`); multi-line
  blocks close on a blank line.

## Standard library
The **default build** (no features) serves only a native subset; every other
module raises `ModuleNotFoundError`.

- **Native in every build**: `math` (constants + a common function subset), `sys`
  (`argv` from the process args, `exit`/`getrecursionlimit`/`setrecursionlimit`,
  `maxsize`, `version`/`version_info` reporting the emulated CPython `3.14.6`,
  `platform` (`darwin`/`linux`), `path`, `modules`, `executable`,
  `stdout`/`stderr`/`stdin` file objects), `collections` (`deque`, `Counter`,
  `defaultdict`, `OrderedDict`, `namedtuple`), `textwrap`, and `statistics`.
- **The rest of the stdlib is served by the `--features stdlib-ffi` bridge** — an
  embedded libpython over pyo3, so `import re`/`json`/`os`/`random`/`string`/
  `itertools`/`functools`/`datetime`/`hashlib`/… load the **real CPython
  modules** (pure `.py` + the C accelerators), not hand-rolled shadows.
  `functools.partial`/`lru_cache`/`reduce`, `re`, `json`, `os` + `os.path`,
  `random`, `string`, and `itertools` (natively lazy `count`/`cycle`/`islice`)
  all come from CPython there (`collections`/`math`/`sys` stay the native arms,
  which resolve before the FFI fallback). Build with
  `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo build --features stdlib-ffi`.
  **Without the feature, none of these import** — e.g. `import functools`,
  `import re`, `import os` all fail in the default build.

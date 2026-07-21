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
  the `yield`-expression value, the full method protocol
  (`.send()`/`.throw()`/`.close()`/`.__next__()`), a generator `return`
  surfacing as `StopIteration.value`, and **full `yield from` delegation**
  (PEP 380): a value `.send()`-ed into the delegating generator reaches the
  sub-generator's `yield` expression, a `.throw()` is forwarded to the
  sub-generator's `.throw()`, a `.close()` (GeneratorExit) forwards to the
  sub-iterator and runs its try/finally, and the delegate's `return`
  (`r = yield from sub()`) binds `sub`'s return value. Generator expressions
  `(x for x in xs)` are **lazy** (a hidden generator function), not eager.
- **Call-site unpacking** `f(*args, **kwargs)`, `f(a, *b, c, **d)` — flattened at
  runtime through `BUILD_ARGS`/`BUILD_KWARGS` and the `CALL*_EX` ops.
- **Literal spreads** `[*a, *b]`, `(*a, b)`, `{*a, *b}`, and dict `**`-spread
  `{**a, "k": 1, **b}` (later keys override; `None` stays a valid key).
- **`match`/`case`** (PEP 634): literal, capture, wildcard `_`, dotted-value
  `Color.RED`, sequence `[a, *rest]`, mapping `{"k": v, **rest}`, class
  `Point(x=0)` (via `__match_args__` + builtin-type self-match), OR-patterns
  `a | b` (with `as` binding looser than `|`), `as` bindings, `if` guards, and
  arbitrary nesting. Singleton patterns `None`/`True`/`False` match by identity
  (`is`), every other literal by `==`. Compile-time `SyntaxError`s (duplicate
  capture, duplicate mapping key, repeated class-keyword, OR alternatives binding
  different names) and the positional-overflow `TypeError` mirror CPython.
- **Name resolution (LEGB)** follows CPython's compile-time scope analysis. A
  name assigned anywhere in a function body is a **local**; reading it before it
  is bound raises **`UnboundLocalError`** (a `NameError` subclass) rather than
  falling through to an enclosing/global binding — covering read-before-assign,
  `+=` on an unbound name, a conditionally-assigned name, and `del`-then-read. A
  read at module scope stays dynamic (`NameError`). A **class body is not an
  enclosing scope** for its methods/comprehensions: free names there resolve
  against the enclosing/module scope, never the class namespace (reachable only
  via `self`/`ClassName`).
- **`nonlocal`** rebinds the nearest enclosing FUNCTION scope that binds the name
  (distinct from `global`, which targets module scope). Validated at compile
  time: a `nonlocal` with no enclosing binding is `SyntaxError: no binding for
  nonlocal '<x>' found`, and one at module level is `SyntaxError: nonlocal
  declaration not allowed at module level`.
- **Function/class introspection**: `__name__`, `__qualname__` (the dotted
  `co_qualname` path — `outer.<locals>.inner`, `C.m`, `A.B`), `__module__`
  (`__main__`), and `__defaults__` (positional-default tuple, or `None`) on
  functions, bound methods, and classes.
- **Augmented assignment** (`+= -= *= /= //= %= **= @= &= |= ^= <<= >>=`) runs the
  CPython in-place protocol: `x += y` tries `type(x).__i<op>__(x, y)` first, then
  falls back to `x = x <op> y`. A user `__iadd__`/… that mutates and returns
  `self` preserves identity (`id(x)` unchanged), as do the mutable built-ins
  (`list +=`/`*=`, `set |= &= -= ^=`, `dict |=`, `bytearray +=`/`*=`); immutables
  (`int`/`str`/`tuple`/`frozenset`) rebind a new object. A subscript/attribute
  target's receiver and index are evaluated exactly once.
- **Chained comparisons** `a < b < c` evaluate each interior operand exactly once
  and short-circuit (`1 < f() < 10` calls `f` once; a failed earlier link skips
  the later operands entirely).
- **`with` / `async with`** call a real `__exit__(exc_type, exc_value, tb)` with
  the active exception's type and value on the error path (`tb` is `None` —
  pythonrs has no traceback objects); a truthy return **suppresses** the
  exception, a falsy/`None` return re-raises. On the normal path `__exit__` is
  called once with `(None, None, None)`. `with A, B:` nests independently, so an
  inner manager's suppression hides the exception from the outer one. `__enter__`'s
  return value binds to the `as` target.
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
- **f-string `=` debug specifier** `f'{x=}'` / `f'{x = }'` / `f'{x+1=}'`: the
  source text up to and including the top-level `=` (preserving surrounding
  whitespace) is emitted literally, then the value — defaulting to `repr` with
  neither conversion nor format spec, and honoring a trailing `!r`/`!s`/`!a`
  conversion or `:spec` (`f'{x=:.2f}'`, `f'{y=!r}'`). Byte-verified vs CPython
  via the `conttail` fuzz mode.
- **`str.format` keyword / index / attribute fields** `'{name}'.format(name=…)`,
  `'{0[1]}'.format(seq)`, `'{d[k]}'.format(d=…)` (unquoted subscript key → str),
  `'{0.real}'.format(x)` (attribute access) — all resolve against the positional
  args, kwargs, and accessor chain.
- **`\N{NAME}`** named-Unicode escapes decode in normal and f-strings.
- **File I/O**: `open()` (text/binary, read/write/append), `.read`/`.readline`/
  `.readlines`/`.write`, line iteration, and `with open(...) as f:` work in the
  default build.
- **`bytes`/`bytearray` are real heap types** with the full sequence + method
  surface (byte-verified vs CPython via the `bytesops` and `bytestail` fuzz
  modes, 0 divergences): construction (`b'…'`, `bytes([65,66])`, `bytes(3)`,
  `bytearray(b'…')`, `bytes.fromhex`/`bytearray.fromhex`), `len`, integer
  indexing (`b[0]`→int), iteration/`list()`, slicing (`b[1:3]`, `b[::-1]`),
  concat (`b1+b2`, result type follows the left operand), repeat (`b*3`),
  membership (`int in b` byte-value, bytes-like substring `b'a' in b'abc'`),
  ordering (`<`/`==`, incl. bytes vs bytearray), and `bytes` as a hashable
  dict/set key. Str-parallel methods returning/taking bytes:
  `split`/`rsplit`/`join`/`replace`/`find`/`rfind`/`index`/`rindex`/`count`/
  `startswith`/`endswith`/`strip`/`lstrip`/`rstrip`/`upper`/`lower`/`swapcase`/
  `title`/`capitalize`/`zfill`/`expandtabs`/`center`/`ljust`/`rjust`/
  `splitlines`/`partition`/`rpartition`/
  `removeprefix`/`removesuffix`/`translate`/`maketrans`/`decode` (across
  `utf-8`/`ascii`/`latin-1`/`utf-16`/`utf-32` with `errors=`
  `strict`/`ignore`/`replace`/`backslashreplace`; the encode-only
  `namereplace`/`xmlcharrefreplace` raise `TypeError` on decode, matching
  CPython)/`hex` (incl. the `sep`/`bytes_per_sep` grouping form), the ASCII `isX`
  predicates
  (`isalpha`/`isdigit`/`isalnum`/`isspace`/`isupper`/`islower`/`istitle`/
  `isascii`), and PEP 461 `%`-formatting (`b'%d-%s' % (1, b'x')`, `%b`/`%c`/
  `%a`/`%r`, width/precision/flags, `%(name)s` mapping; `%b`/`%s` dispatch a
  user instance's `__bytes__`). `bytearray` item +
  slice assignment (`ba[0]=65`, `ba[1:2]=b'xy'`, `ba[::2]=…`), deletion
  (`del ba[i]`, `del ba[i:j]`, `del ba[::k]`), plus
  `append`/`extend`/`pop`/`clear`. `repr` matches CPython quoting (single/
  double-quote selection; the bytearray always-escape-`'` quirk).
- **Codecs, escapes, and unicode** (byte-verified vs CPython via the `codec`
  fuzz mode, 0 divergences): `str.encode(encoding, errors)` across
  `utf-8`/`ascii`/`latin-1`/`iso-8859-1`/`utf-16`/`utf-32` (bare `utf-16`/`utf-32`
  emit a little-endian BOM; the `-le`/`-be` names don't) with the
  `strict`/`ignore`/`replace`/`backslashreplace`/`xmlcharrefreplace`/`namereplace`
  error handlers; `bytes.decode` for the same codecs with BOM auto-detection and
  the decode-side handler set. `repr`/`ascii` escape exactly the non-printable
  code points CPython does (Unicode 16.0 general categories Cc/Cf/Cs/Co/Cn and
  Zl/Zp/Zs, space excepted), choosing the shortest `\xHH`/`\uHHHH`/`\UHHHHHHHH`
  form. `chr`/`ord` round-trip the full range (lone surrogates rejected — a Rust
  `str` can't hold them; see gaps). `str.isprintable`/`isascii`/`isidentifier`
  (incl. the PEP 3131 `Other_ID_Continue` + ZWNJ/ZWJ chars)/`isspace` (incl.
  U+001C..U+001F) match CPython; `len`/indexing count code points, not bytes.
  Escape literals — `\n \t \r \0`, octal `\NNN`, `\xHH`, `\uHHHH`, `\UHHHHHHHH`,
  `\N{NAME}`, raw `r"…"`, and byte-string escapes — decode in the lexer.
- **Comprehension scope**: list/set/dict comprehensions run in their own function
  scope, so the loop variable no longer leaks; enclosing variables are still read
  through the closure (the outermost iterable is evaluated in the enclosing
  scope, matching CPython).

- **Subclassing builtin types** (`class Stack(list)`, `class D(dict)`,
  `class U(str)`, `class C(int)`, `class F(float)`, `class T(tuple)`,
  `class S(set)`). The instance is a hybrid: it carries the native builtin
  payload (list storage / int value / …) alongside the class + `__dict__`, so it
  inherits ALL builtin behavior — methods (`.append`/`.upper`/`.keys`),
  operators (`+`/`[]`/`len`), iteration, membership, `repr`/`str`, hashing,
  equality — while supporting user methods, instance attributes, and
  `super().__init__(...)` / `super().__new__(cls, …)`. One mechanism routes every
  type (`builtin_base_of` detects the base from the MRO; the payload is unwrapped
  for operators/coercion and delegated to for methods/protocol dunders).
  Construction builds the payload from the constructor args (immutable bases at
  `__new__`, mutable bases via `__init__`/`super().__init__`). A `dict` subclass
  fires `__missing__` on a key miss; `int`/`float` subclass arithmetic returns
  the plain base type (`C(5) + 3` → `int` `8`); `isinstance` and
  `type(x).__name__` reflect the subclass. Fuzzed to zero divergences
  (`parity-fuzz --mode subclass`).

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
  `asyncio.wait`/`as_completed`/`Event`/`Lock`/`Queue` are also implemented
  natively on the same event loop (`Event.wait/set/clear`, `Lock.acquire/release`
  + `async with lock`, `Queue.put/get/qsize`), byte-verified vs CPython.
- **Async generators.** `async def` containing `yield` builds an async generator
  (`type().__name__ == 'async_generator'`) with `__aiter__`/`__anext__`; each
  `__anext__` drives the body to the next `yield` (forwarding intervening `await`
  suspensions to the loop) and raises `StopAsyncIteration` on exhaustion — so
  `async for x in ag()` and `[x async for x in ag()]` over a real async generator
  both work (byte-verified). The `await`-vs-`yield` distinction rides an
  `awaiting` flag on the generator cell.
  **Not yet:** task cancellation propagation (`Task.cancel` settles the future but
  does not inject `CancelledError` into a suspended coroutine); bounded-`Queue`
  put back-pressure (put is always accepted); `wait`'s `timeout`/`return_when`
  variants; async-generator `asend`/`athrow`/`aclose`.

## Partial / simplified semantics
- **Operator overloading dunders**: dispatched, with `NotImplemented` reflected
  fallback (see Implemented). Covered: arithmetic/bitwise
  (`__add__`/`__sub__`/`__mul__`/`__truediv__`/`__floordiv__`/`__mod__`/`__pow__`/
  `__matmul__`/`__and__`/`__or__`/`__xor__`/`__lshift__`/`__rshift__`) with their
  reflected `__r*__`, comparisons (`__eq__`/`__ne__`/`__lt__`/`__le__`/`__gt__`/
  `__ge__`), and `__getitem__`/`__setitem__`/`__len__`/`__bool__`/`__str__`/
  `__repr__`/`__iter__`/`__next__`/`__init__`/`__hash__`. Container `repr`/`str`
  recurses so instance elements/keys/values dispatch their own `__repr__`.
  In-place augmented dunders are dispatched too (see Implemented).
- **Builtin-type subclass instances as dict/set keys** hash by identity, not by
  their base value: `{U('a'): 1}['a']` (where `class U(str)`) raises `KeyError`
  where CPython returns `1`, because `to_key` keys a subclass instance as
  `PKey::Instance` (identity) rather than unwrapping to the payload's value key.
  The subclass value still hashes correctly via `hash(U('a')) == hash('a')`; only
  the dict/set *keying* path does not yet unwrap. (Subclassing itself — methods,
  operators, iteration, `super()` — is fully covered; see Implemented.)
- **Augmented assignment on a mutable builtin subclass** loses the subclass type:
  `s += [2]` where `class L(list)` yields a plain `list`, not an `L` (the value
  is correct, the type is downgraded). Direct mutation (`s.append(2)`) preserves
  the subclass identity.
- **`int`** is arbitrary precision (bignum) across `+ - * ** // %` and the bitwise
  ops `& | ^ << >>` — verified byte-identical to CPython on `10**30`-scale values
  (the earlier i64-cap on `//`/`%`/bitwise is gone).
- **f-string / `str.format` format spec** covers the common mini-language
  (fill/align/sign/width/`,`/`.prec`/type `d f e x o b % s c g`) and nested field
  specs (see Implemented).
- **Lone surrogates in `str`**: `chr(0xD800..0xDFFF)` raises `ValueError` where
  CPython returns a surrogate-bearing `str` (which then fails only on UTF-8
  encode). pythonrs strings are Rust `String` (valid scalar values only), so a
  lone surrogate is unrepresentable without a surrogate-aware string type; the
  out-of-range and surrogate paths share CPython's `chr() arg not in
  range(0x110000)` message. `surrogateescape`/`surrogatepass` handlers are
  likewise not reachable for the same reason.

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

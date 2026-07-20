# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython on the example corpus). This file is the honest
list of what is **not** yet covered, so nobody mistakes a gap for a bug fixed.

## Implemented (previously listed here as gaps)
- **Generators / `yield`.** A `def` whose body contains `yield` builds a real
  lazy generator, backed by a stackful `corosensei` coroutine on the same thread
  (the thread-local `PyHost` is shared across suspend/resume via a swapped
  execution context). Supported: `for x in gen()`, `next(g)`, `list(gen())`,
  `yield`-expression value, and `yield from`. Generator expressions
  `(x for x in xs)` are now **lazy** (a hidden generator function), not eager.
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
  `__repr__` exists; uncaught `raise E('x')` prints `E: x`.
- **`__init_subclass__` (PEP 487)** fires the parent hook with the new class and
  class-header keywords (`class C(P, tag="x")`); a zero-arg `super()` inside a
  `property` getter/setter resolves; f-string/`.format`/`ascii` `!a` ascii-escapes.
- **Comprehension scope**: list/set/dict comprehensions run in their own function
  scope, so the loop variable no longer leaks; enclosing variables are still read
  through the closure (the outermost iterable is evaluated in the enclosing
  scope, matching CPython).

## Not yet implemented (compile/parse-time error, no silent wrong answer)
- **`async`/`await`.** Parsed, but `await` is a no-op passthrough and there is no
  event loop; `async def` runs synchronously.
- **`yield from` delegation value.** Iteration is fully supported, but the value
  of a `yield from` expression (the sub-generator's `return` value) is always
  `None`; sent values are not forwarded to the delegate.

## Partial / simplified semantics
- **Operator overloading dunders**: now dispatched. Arithmetic/bitwise
  (`__add__`/`__sub__`/`__mul__`/`__truediv__`/`__floordiv__`/`__mod__`/`__pow__`/
  `__matmul__`/`__and__`/`__or__`/`__xor__`/`__lshift__`/`__rshift__`) with their
  reflected `__r*__` fallbacks, and comparisons (`__eq__`/`__ne__`/`__lt__`/`__le__`/
  `__gt__`/`__ge__`, reflected for `<`/`>`/`<=`/`>=`), plus the previously-dispatched
  `__getitem__`/`__setitem__`/`__len__`/`__bool__`/`__str__`/`__repr__`/`__iter__`/
  `__next__`/`__init__`. Container `repr`/`str` (`list`/`tuple`/`set`/`dict`) now
  recurses so instance elements/keys/values dispatch their own `__repr__`.
  Not yet: `NotImplemented`-driven reflected-op negotiation (the forward dunder is
  used if present; it is not retried reflected when it returns `NotImplemented`) —
  though `str % obj` is now native-authoritative and never consults the right
  operand's `__rmod__`; `__hash__` for instances as dict keys / set members;
  in-place `__iadd__` etc. Also: `%s`/`%r`/`%a` of a *user instance* does not
  dispatch its `__str__`/`__repr__` (the `%` formatter runs inside the host borrow;
  f-strings and `str.format` do — prefer them).
- **Chained comparisons** `a < b < c` re-evaluate the interior operand `b`
  (correct for side-effect-free operands; a function call in the middle runs twice).
- **`with`** desugars to try/finally calling `__enter__`/`__exit__`; the exception
  triple passed to `__exit__` is always `(None, None, None)`.
- **`int`** promotes to arbitrary precision (bignum) for `+ - * **`; `//`, `%`,
  and bitwise ops on values beyond i64 fall back to i64 range.
- **`bytes`** literals are accepted but represented as an empty/placeholder object;
  bytes operations are unimplemented.
- **f-string format spec** covers the common mini-language (fill/align/sign/width/
  `,`/`.prec`/type `d f e x o b % s`); nested-field specs `{:{w}}` are not expanded.
- **`str.format`** supports `{}`, `{0}`, `{name}` is not bound (no kwargs plumbing
  through `.format`), and `:spec`.

## Tooling
- **`--dap`** (Debug Adapter Protocol): implemented — breakpoints, step
  in/out/over/continue, stack trace, locals, and program-stdout capture (pipe +
  dup2 → `output` events). Frame names in the stack use the function/class owner
  or `<module>` (no per-function name field yet). Watch expressions not yet added.
- **`--lsp`**: full corpus — completion (166 builtins/keywords/methods), position-
  aware hover, and diagnostics via the real parser. Go-to-def and signature help
  not yet added.
- **REPL** does not echo bare-expression values (use `print(...)`); multi-line
  blocks close on a blank line.

## Standard library
Implemented natively (values match CPython except `random`, which is pythonrs's
own deterministic PRNG): `math`, `sys`, `json` (dumps/loads, order-preserving,
bignum-safe), `os` + `os.path` (POSIX), `random`, `string`, `itertools` (eager —
finite forms; unbounded `count`/`cycle` rejected), `functools.reduce`.
Not yet: `re`, `collections` (needs new container types), `datetime`, file I/O,
`functools.partial`/`lru_cache`, and most of the rest of the stdlib.

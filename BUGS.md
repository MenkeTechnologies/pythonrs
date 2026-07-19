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
  used if present; it is not retried reflected when it returns `NotImplemented`);
  `__hash__` for instances as dict keys / set members; in-place `__iadd__` etc.
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
- **`--dap`** (Debug Adapter Protocol): returns "not implemented". `--dap` compile
  mode (per-statement line markers) exists; the stepping server does not.
- **`--lsp`**: minimal — initialize/shutdown handshake, builtin/keyword completion,
  a stub hover. No diagnostics, go-to-def, or signature help yet.
- **REPL** does not echo bare-expression values (use `print(...)`); multi-line
  blocks close on a blank line.
- **`import`**: only `math` and `sys` (minimal) resolve; other modules raise
  `ModuleNotFoundError`.

## Standard library
Effectively none beyond builtins + minimal `math`/`sys`. No `os`, `re`, `json`,
`collections`, `itertools`, `datetime`, file I/O, etc. yet.

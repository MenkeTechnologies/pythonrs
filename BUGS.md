# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython on the example corpus). This file is the honest
list of what is **not** yet covered, so nobody mistakes a gap for a bug fixed.

## Not yet implemented (compile/parse-time error, no silent wrong answer)
- **Generators / `yield`.** `def` bodies containing `yield` are detected and
  rejected at call time (`generator functions are not yet supported`). Generator
  expressions `(x for x in xs)` are evaluated **eagerly** as a list, not lazily.
- **`async`/`await`.** Parsed, but `await` is a no-op passthrough and there is no
  event loop; `async def` runs synchronously.
- **Call-site unpacking** `f(*args, **kwargs)` — rejected at compile time. Def-site
  `*args`/`**kwargs` and keyword args `f(a, b=2)` DO work.
- **`dict` `**spread` inside a literal** `{**a, "k": v}` — rejected at compile time.
  `dict(**a)` and `d.update(a)` work.
- **`match`/`case`** structural pattern matching — not parsed yet.

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
- **`nonlocal`** is approximated by `global` (rebinding an enclosing function's
  local from a nested function writes to module scope instead).
- **Comprehension scoping**: the loop variable leaks into the surrounding scope
  (Python 3 gives comprehensions their own scope). The accumulator does not leak.
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

# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython 3.14.6 on the example corpus). This file is the
honest list of what is **not** yet covered, so nobody mistakes a gap for a bug
fixed. Every line below was re-checked against the **default-build** binary
(`cargo build` — default features, so the `stdlib-ffi` bridge is ON) before being
written.

## Implemented (previously listed here as gaps)
- **`with` checks the context-manager protocol before entering.** The desugar
  called `ctx.__enter__()` directly, so a manager carrying only `__enter__` ran
  it *and the whole body* and only failed on the way out with
  `AttributeError: 'E' object has no attribute '__exit__'`. CPython's
  `SETUP_WITH` looks up `__exit__` FIRST and refuses to enter at all. The entry
  now routes through a dot-prefixed sentinel (unwriteable in Python source, like
  the desugar's own `.ctx` temporaries) so the check runs before the call, in
  CPython's order, with CPython's message: `TypeError: 'E' object does not
  support the context manager protocol (missed __exit__ method)` — and
  `missed __enter__ method` for the other half. `async with` reports the
  `asynchronous context manager protocol` wording against `__aexit__`/
  `__aenter__`. An explicit `obj.__enter__()` written by the user still raises
  the ordinary `AttributeError`, as CPython does.
- **Parenthesized with-items (`with (a as x, b as y):`).** PEP 617 gave CPython
  3.10 a PEG parser that can backtrack over the `(`-ambiguity, so a long `with`
  header can be wrapped in parentheses. pythonrs rejected the whole form with
  `SyntaxError: expected ')' but found Name("as")` — a hard stop on any modern
  script. The parenthesized item list is now tried first and wins whenever the
  group closes immediately before the `:`, so `with (a, b):` is TWO context
  managers (CPython's reading), while `with (a, b)[0]:`, `with (a) as x:`,
  `with (x for x in y):` and `with ():` still parse as one expression.
- **`divmod` dispatches `__divmod__`/`__rdivmod__`.** It was computed as
  `(a // b, a % b)`, so a class defining only `__divmod__` raised
  `TypeError: unsupported operand type(s) for //`, and a class defining all
  three ran the wrong two. `divmod` is a binary operator in its own right, and
  a missing pair now reports CPython's `unsupported operand type(s) for
  divmod(): 'V' and 'int'`.
- **`dir(obj)` honors a user `__dir__`.** The hook was inert: `dir()` always
  listed the class/instance dict. CPython calls `type(obj).__dir__(obj)` and
  only sorts the result — no dedup (`['a', 'a', 'z']` stays three entries), and
  a non-iterable return or unorderable elements raise from that list()+sort().
- **`obj.__class__ = C` retypes the instance.** The assignment stored a
  shadowing `__class__` entry in the instance dict and left `type(obj)`
  untouched — a silent no-op with no error. It now swaps the class (methods,
  `isinstance`, and `__class__` all follow, the instance dict is kept) when the
  layouts match, and otherwise raises CPython's message: `__class__ must be set
  to a class, not 'int' object` for a non-class, `__class__ assignment only
  supported for mutable types or ModuleType subclasses` for a static type on
  either side, `__class__ assignment: 'B' object layout differs from 'A'` when
  the slot layouts disagree. `del obj.__class__` raises
  `TypeError: can't delete __class__ attribute` instead of an `AttributeError`.
- **Attribute stores and deletes carry a line and caret.** `SETATTR`, `DELATTR`
  and `DELITEM` were emitted with line 0, so every traceback out of a rejected
  `obj.attr = v` (a `__slots__` rejection, a setter-less `property`) or a failed
  `del obj.attr` / `del obj[k]` rendered `File "…", line 0, in <module>` with no
  source line and no carets — naming nothing at all. They now carry the
  statement's line and the target's span, the same fix the container displays
  and subscript stores got.
- **Binary operator slots are real bound methods on the builtin containers.**
  `{'a': 1}.__ior__({'b': 2})`, `[1].__add__([2])`, `{1, 2}.__and__({2})`,
  `'a'.__mul__(3)`, `b'a'.__add__(b'b')`, `(1j).__truediv__(2)` — every one of
  them raised `AttributeError: 'dict' object has no attribute '__ior__'`, even
  though the operator SYNTAX (`d |= …`) worked, because an operator slot is
  dispatched natively rather than through a per-type descriptor object. Only
  `int`/`float`/`bool`, which carry an explicit dunder table, ever answered one.
  Each type now exposes exactly the set CPython 3.14 puts on an instance of it
  (`str`/`bytes`/`bytearray`/`list`/`tuple`/`dict`/`set`/`frozenset`/`complex`),
  the in-place halves mutate and return the receiver, and an operand of the
  wrong kind answers `NotImplemented` for the set/dict/complex operators exactly
  as CPython does. The same table drives `dir()`, so dispatch and listing still
  agree in both directions.
- **`x %= args` on a `bytes`/`bytearray`.** The in-place fallback carried the
  `str %` branch but not the PEP 461 one, so `b'%d' % 1` formatted while
  `x %= 1` on the same receiver raised `unsupported operand type(s) for %:
  'bytes' and 'tuple'`.
- **A numeric `AttributeError` names its type.** `(1).__iadd__` reported
  `AttributeError: object has no attribute '__iadd__'` with no type; CPython
  names it (`'int' object has no attribute '__iadd__'`).
- **A value-keyed object NESTED inside a `tuple`/`frozenset` key.** A `tuple`/
  `frozenset` key is hashed element-wise, so an element with a user `__hash__`
  is a key in its own right — but only the TOP-LEVEL object was prepared outside
  the host borrow, so `{(P(1),): 5}` raised `TypeError: unhashable type: 'P'`
  from the borrowed `to_key`, which cannot run user code. The preparation now
  walks into `tuple`/`frozenset` operands, collapse candidates are collected at
  every depth (so a nested element merges onto a value-equal one anywhere in the
  destination), and two equal elements of ONE key collapse onto each other. A
  `frozenset` key's element keys are recomputed at use, since they were resolved
  when the frozenset was built and carry heap ids the destination knows nothing
  about. `hash()` of such a container drops those ids, so
  `hash((P(1),)) == hash((P(1),))` holds as in CPython. Twenty-one distinct
  shapes were wrong — subscript, assignment, `in`, `get`, `pop`, `setdefault`,
  literal dedup, `repr`, whole-container `==`, `set.add`/`update`, the set
  algebra over tuple elements, and `frozenset`-keyed lookups (which failed with
  `KeyError` rather than `TypeError`).
- **Container `==` runs the elements' user `__eq__`.** `list`, `tuple`, `deque`,
  and a `dict`'s values compared element-wise INSIDE the host borrow, where a
  user `__eq__` cannot run, so `P(1) == P(1)` was True while `(P(1),) ==
  (P(1),)`, `[P(1)] == [P(1)]`, `deque([P(1)]) == deque([P(1)])`, and
  `{1: P(1)} == {1: P(1)}` were all silently False. Element comparison now runs
  through the full `==` dispatch, with CPython's `PyObject_RichCompareBool`
  identity shortcut, whenever any element compares through user code; containers
  of plain values keep the borrowed comparison. `tuple.index`/`tuple.count` had
  the same gap while their `list` counterparts did not — `(P(1), P(2)).index(
  P(2))` raised `ValueError: x not in tuple`.
- **Cross-container algebra with value-keyed elements.** A set/dict operation
  between two *independently built* containers whose elements key through user
  code — a user instance with `__hash__`+`__eq__`, or a CPython `Foreign` object
  (enum member, `Decimal`, `Fraction`, `datetime`, …) — now merges value-equal
  elements across the two operands. `{P(1), P(2)} & {P(2)}` is `{P(2)}`, and
  `|`/`-`/`^`, the method spellings (`union`/`intersection`/`difference`/
  `symmetric_difference`), the in-place forms (`|= &= -= ^=`,
  `update`/`intersection_update`/`difference_update`/
  `symmetric_difference_update`), the subset orders (`< <= > >=`,
  `issubset`/`issuperset`/`isdisjoint`), and `==` between two whole sets or dicts
  all agree with CPython. Such a key carries the heap id of the object it
  collapsed onto (`PKey::Instance`/`PKey::Foreign`) and the borrowed ops compare
  keys structurally, so `host::align_operand` re-keys the right operand's
  elements against the left's through `prepare_key` (running `__hash__`/`__eq__`,
  or the bridge's, outside the borrow) before the comparison. Containers with no
  value-keyed element skip the pass entirely. `update` and
  `symmetric_difference_update` additionally raised `TypeError: unhashable type`
  on any user-`__hash__` element, because they hashed inside the borrow.
  **`dict.update` keys against the DESTINATION** for the same reason. It copied
  the source dict's keys verbatim, so a value-equal key opened a SECOND slot —
  `{P(1): 'a'} | {P(1): 'z'}` was right but `d.update({P(1): 'z'})` and `d |=
  {P(1): 'z'}` left a dict holding two `P(1)` entries, which CPython cannot
  produce; its pair-iterable form (`d.update([(P(1), 'z')])`) hashed under the
  borrow and raised `unhashable type`. Two value-equal keys within one `update`
  now collapse the way a dict literal's do.
- **A class may define `__hash__` without `__eq__`.** CPython then inherits
  `object.__eq__` (identity). The key collapse called `__eq__` directly, so the
  first hash collision between two such instances raised `AttributeError: 'P'
  object has no attribute '__eq__'` and made the whole dict/set unusable
  (`{P(5): 1, P(5): 2}` with `__hash__ = v // 2`). The collapse now runs the full
  `==` dispatch, which also routes a builtin-type subclass through its payload,
  so `class S(str)` with its own `__hash__` still merges `S('a')` with `S('a')`.
- **`dict_keys` / `dict_items` views are set-like**, as in CPython: they take
  part in `==` and in the subset order (`d.keys() == {1, 2}`,
  `d.keys() <= {1, 2}`, `d.items() == {(1, 0)}`), not only in `& | - ^`. `==`
  answered False for every view — including all-`int` keys — and the ordering
  operators raised `'<=' not supported between instances of 'dict_keys' and
  'set'`. A `dict_values` view stays non-set-like (two views are never equal).
  Separately, a key view coerced to a key-set by re-hashing its key OBJECTS, and
  a value key cannot be hashed under the host borrow — the error was discarded,
  so `d.keys() & {P(2)}` silently dropped exactly the value-keyed elements and
  came back empty. A key view now contributes its dict's own key map.
- **A set predicate answers for an iterable it cannot hash.** `{1}.issubset(
  [P(1)])` is `False` in CPython, not a `TypeError`; with no candidate key to
  collapse onto, the argument's elements still have to be hashed outside the
  borrow rather than short-circuited into it.
- **`__slots__` validation** (CPython `type_new_slots_impl`): a slot name also
  bound in the class body is `ValueError: 'a' in __slots__ conflicts with class
  variable`; a non-string is `TypeError: __slots__ items must be strings, not
  'int'`; a non-identifier is `TypeError: __slots__ must be identifiers`; a
  repeated `__dict__`/`__weakref__` is `TypeError: <name> slot disallowed: we
  already got one`. `__qualname__` and `__classcell__` — names class creation
  inserts itself — are exempt from the conflict check.
- **`itertools.chain.from_iterable`** is reachable as an attribute of `chain`.
- **`f.__annotate__`** (PEP 649): the callable that yields the annotations for a
  requested format, `None` on an unannotated function. CPython 3.14's
  `functools.singledispatch.register` gates on it, so `@generic.register` on an
  annotated implementation now infers the dispatch type.
- **CPython-side stdout ordering.** pythonrs's `print` writes straight to the fd,
  while the embedded interpreter's `sys.stdout` is block-buffered on a pipe and
  is never `Py_Finalize`d. A pythonrs builtin handed to CPython crosses as the
  genuine CPython builtin, so `functools.partial(print, …)`,
  `ExitStack.callback(print, …)` and friends wrote through that stream — their
  output came out reordered, or was dropped at exit. Both streams are line
  buffered at bridge init (`ffi::line_buffer_std_streams`).
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
  return value binds to the `as` target. A **foreign** context manager
  (`contextlib.suppress`, …) works on the error path too: the pythonrs exception is
  reconstructed as a real CPython exception for its `__exit__`, so `suppress`
  matches it (including by base class). `contextlib.redirect_stdout`/
  `redirect_stderr` and `sys.stdout = io.StringIO()` retarget pythonrs's own
  `print` (a native redirect; a CPython one only touches CPython's stream, which
  print doesn't consult); nesting restores correctly and `sys.__stdout__`/
  `__stderr__`/`__stdin__` keep the native streams.
- **User exception subclasses** inherit `BaseException`: `class E(Exception)`
  instances carry `args` (seeded by construction / `super().__init__` / direct
  assignment), stringify to the message (`''`/`str(arg)`/`repr(tuple)`), repr as
  `E(arg, …)`, and expose `.args` and `.__class__` (the type object); `str()` uses
  the message even when a user `__repr__` exists. An uncaught exception prints
  CPython's `Traceback (most recent call last):` block — header, `  File "<path>",
  line N, in <scope>` + source line + CPython 3.11+ fine-grained caret per frame
  (outermost first), then `ErrorType: message`. Carets follow CPython's anchor
  rules: `~^~` under a binary operator, `~~~^^^` under a subscript/call's
  brackets, a plain `^^^` under a name/attribute, and no caret when the span
  covers the whole line or when an `x = f(...)` / `return f(...)` call raises. A
  fused name/method call whose *callee lookup* fails (`foo()` on an undefined
  name, `obj.missing()`) anchors the call brackets rather than the name — the one
  spot the fused CALL op diverges from CPython's separate LOAD+CALL. **Exception
  chaining** renders in full: `raise X from Y` records `__cause__` and prints the
  cause's own block followed by "The above exception was the direct cause …"; an
  exception raised while handling another chains via `__context__` ("During
  handling of the above exception …"); `raise X from None` sets
  `__suppress_context__`, hiding the implicit context. Each chained exception's
  frames are captured (`__traceback__`) at the point it is caught.
- **`Did you mean: 'x'?` on an uncaught `NameError`/`AttributeError`.** A port of
  `Python/suggestions.c`'s `_Py_CalculateSuggestions` — which is what CPython
  3.13+ actually runs, and which disagrees with `traceback.py`'s pure-Python
  fallback (the fallback seeds its running best with `len(wrong_name)`, so `st`
  suggests nothing there and `set` in the real interpreter). The distance is
  CPython's modified Levenshtein: moves cost 2, a pure case flip costs 1, common
  affixes are trimmed, and a row that cannot beat the budget bails out.
  Candidates are the frame's locals (including the ones held in frame SLOTS,
  which never reach the environment), then its globals, then the builtins for a
  `NameError`; `dir(obj)` with private names hidden — unless the code asked for a
  private one, or the receiver is the running method's own instance — for an
  `AttributeError`. A bare name that is an attribute of the running method's
  instance is reported as `self.<name>`. The hint belongs to the RENDERED
  traceback, never to `str(e)`/`e.args`, as in CPython. Fuzzed to zero
  divergences (`parity-fuzz --mode suggest --stderr`, 8000 cases; the same mode
  finds 207 in 2000 against the previous build).
- **Exception groups and `except*` (PEP 654).** `ExceptionGroup` /
  `BaseExceptionGroup` are real: the constructor validates its arguments and
  narrows (`BaseExceptionGroup` holding only `Exception`s builds an
  `ExceptionGroup`); `.message`/`.exceptions`/`.args` read back; `str` counts
  members (`g (2 sub-exceptions)`); `ExceptionGroup` answers `isinstance` for
  BOTH its bases. `split`/`subgroup`/`derive` are ported from CPython's
  `exceptiongroup_split_recursive`/`exceptiongroup_subset`, so a nested group is
  rebuilt with its own nesting on both sides and each part inherits the group's
  traceback and chaining; the matcher may be a class, a tuple of classes, or a
  predicate. `except*` runs each clause **at most once** against what is left of
  the group, binds it to the matching subgroup, wraps a naked exception in a
  one-element group, and reassembles what the handlers left behind with
  `_PyExc_PrepReraiseStar`'s rules — a bare re-raise merges back into the
  original group's nesting, a freshly raised exception becomes a sibling in a new
  `ExceptionGroup('', …)`. Its three compile-time rules (`except` and `except*`
  may not be mixed, every clause names a type, no `break`/`continue`/`return`
  leaves the handler) are enforced. An uncaught group renders CPython's
  `+-+---------------- n ----------------` tree — a port of `traceback.py`'s
  `_ExceptionPrintContext`, including the `max_group_width` (15) /
  `max_group_depth` (10) elisions and each member's own chained blocks. Fuzzed to
  zero divergences (`parity-fuzz --mode excgroup`, stdout and `--stderr`).
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
- **`memoryview`** over a `bytes`/`bytearray` buffer (faithful 1-D unsigned-byte
  subset, byte-verified vs CPython): `memoryview(b'…')`, `len`, integer indexing
  (incl. negative), contiguous slicing (a sub-view sharing the buffer) and
  strided slicing (a fresh view), iteration, byte-value membership, equality
  against `bytes`/`bytearray`/other views, `bool`, `bytes(mv)`/`list(mv)`
  conversion, and `tobytes`/`hex`/`tolist`. Read-only descriptors `obj`,
  `nbytes`, `format` (`'B'`), `itemsize`, `ndim`, `shape`, `strides`,
  `readonly`, `contiguous`. A view over a `bytearray` reflects later mutations
  to the backing buffer and is writable-flagged (`readonly` False); a `bytes`
  backing is read-only. `<memory at 0x…>` repr. Not covered: `cast` (format
  reinterpretation), multi-dimensional views, item assignment through the view.
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
- **The context-manager protocol check does not reach the natively shadowed
  managers.** `with <not a context manager>:` now raises CPython's
  `TypeError: 'X' object does not support the context manager protocol (missed
  __exit__ method)` for a user instance and for the core scalars/containers, and
  refuses to enter (see the Implemented entry). It is skipped for a native
  `File`/`Lock`/`redirect_stdout` and for any bridged CPython object, because
  those dispatch `__enter__`/`__exit__` inside `call_method_inner` without
  exposing them as attributes — probing them would report a missing method that
  is in fact there. A `with` on such a value that genuinely lacks the protocol
  still reports the old `AttributeError`. A faithful fix needs a
  "does this native type answer `__exit__`" predicate that agrees with
  `call_method_inner`'s own dispatch table.
- **`memoryview` is not a context manager.** `with memoryview(b"ab"):` raises
  `AttributeError: 'memoryview' object has no attribute '__enter__'`; CPython's
  `memoryview` supports `with` (the exit releases the buffer).
- **No private-name mangling.** `self.__x` inside `class C` stays `__x`; CPython
  rewrites it to `_C__x` at compile time. So `C().__dict__` reads
  `{'__x': 1}` where CPython gives `{'_C__x': 1}`, `dir()` lists `__x`, and the
  `__slots__` conflict check compares the name as written — `__slots__ = ('__x',)`
  next to a `_C__x = 1` class variable is accepted where CPython raises
  `ValueError: '_C__x' in __slots__ conflicts with class variable`. A faithful fix
  is a compile-time rewrite of every `__name` (not ending in `__`) inside a class
  body, which has to cover attribute access, plain names, keyword arguments, and
  `global`/`nonlocal` declarations.
- **A mutable container read off a bridged CPython object is a fresh copy.** The
  marshaller converts a CPython `list`/`dict`/`set` to a native pythonrs value by
  value on every read, so the identity is not preserved and an in-place mutation
  is lost: with `@dataclass class P: tags: list = field(default_factory=list)`,
  `p.tags is p.tags` is `False` and `p.tags.append(3)` leaves `p.tags == []`
  (CPython: `[3]`). Arguments passed INTO a stdlib call are written back
  (`writeback_mutated_args`), so `random.shuffle(xs)` works; only the attribute
  read direction copies. A faithful fix keeps the container behind a `Foreign`
  handle and routes the mutators through the bridge.
- **`f.__annotate__` is a `functools.partial`, not a `function`.** It is callable,
  answers the `VALUE`/`FORWARDREF` formats with the def-time annotations dict, and
  raises a bare `NotImplementedError` otherwise — but `type(f.__annotate__)` and
  its `repr` differ from CPython's compiler-generated annotate function. Likewise
  `repr(itertools.chain.from_iterable)` is
  `<built-in function itertools.chain.from_iterable>` where CPython prints
  `<built-in method from_iterable of type object at 0x…>`; calling it agrees.
- **Operator overloading dunders**: dispatched, with `NotImplemented` reflected
  fallback (see Implemented). Covered: arithmetic/bitwise
  (`__add__`/`__sub__`/`__mul__`/`__truediv__`/`__floordiv__`/`__mod__`/`__pow__`/
  `__matmul__`/`__and__`/`__or__`/`__xor__`/`__lshift__`/`__rshift__`) with their
  reflected `__r*__`, comparisons (`__eq__`/`__ne__`/`__lt__`/`__le__`/`__gt__`/
  `__ge__`), and `__getitem__`/`__setitem__`/`__len__`/`__bool__`/`__str__`/
  `__repr__`/`__iter__`/`__next__`/`__init__`/`__hash__`. Container `repr`/`str`
  recurses so instance elements/keys/values dispatch their own `__repr__`.
  The numeric dunders are also exposed as callable bound methods on
  `int`/`bool`/`float` (`(5).__index__()`, `(-3).__abs__()`, `(7).__floordiv__(2)`,
  `(1).__add__(2)`, `(2.0).__round__()`, reflected `__r*__`, comparisons,
  `__int__`/`__float__`/`__trunc__`/`__floor__`/`__ceil__`/`__invert__`/`__bool__`/
  `__hash__`); a binary dunder returns the `NotImplemented` singleton for operand
  types the base type declines (`int` combines only with `int`-likes) — matching
  CPython, byte-verified. `int`-only bitwise/shift/`__index__`/`__invert__` are
  absent on `float`, as in CPython.
  In-place augmented dunders are dispatched too (see Implemented). Subclassing
  builtin types (`class L(list)`, `class C(int)`, …) is fully covered: inherited
  methods/operators/iteration, `super().__init__`, `__new__`, use as dict/set
  keys (a payload-hashing subclass keys identically to its base value),
  `dict(subclass)` conversion, and augmented assignment preserving the subclass
  type for mutable bases.
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
- **`float` `repr` tie-break**: the shortest-round-trip formatter defers to Rust
  `std`'s Ryū, which breaks an exact tie between two equally-short 17-digit
  decimals toward the larger digit, whereas CPython's dtoa rounds half-to-even.
  This surfaces only on the rare value whose two shortest reprs are equidistant
  from the true value (e.g. `2113325745016023.2` prints as `…3.3`); the underlying
  `f64` bits are identical either way (`float.hex` agrees). A faithful fix needs a
  dtoa-style shortest formatter rather than the `std` one.
- **`dir()` on a native builtin type/value is the method table, not CPython's
  full slot listing.** `dir(list)`/`dir("a")` enumerate the names the type really
  responds to (so `'append' in dir(list)` and `'upper' in dir(str)` are right),
  plus `__class__`/`__doc__`/`__init__`/`__new__`/`__sizeof__`. CPython's
  `dir(list)` is ~46 entries because every slot wrapper (`__add__`, `__iadd__`,
  `__class_getitem__`, …) is a real descriptor on the type; pythonrs dispatches
  those natively rather than through per-type descriptor objects, so they are not
  enumerable. `dir()` of a bridged CPython object (`dir(json)`,
  `dir(datetime.date(...))`) IS exact — it delegates to CPython's own `dir()`.
- **`__loader__` / `__builtins__` are not bound in module globals.** `__name__`,
  `__file__`, `__doc__`, `__package__`, `__spec__` and (for a script)
  `__cached__` all match CPython, but the remaining two need real importer and
  module objects: `__loader__` is a `_frozen_importlib` class and `__builtins__`
  is the `builtins` module itself. `sorted(globals())` therefore differs from
  CPython by exactly those two names. Relatedly, `import builtins;
  builtins.len is len` is `False` — the bridged `builtins` module is a distinct
  CPython object from the native builtin dispatch.
- **256+ argument calls / `**`-spread dict literals**: `CallBuiltin` carries a
  `u8` operand count, so an op that must name >255 stack slots at once raises
  `too many arguments (>255) for one call`. Plain collection literals
  (`[...]`/`(...)`/`{...}` and f-strings) no longer hit this — the compiler now
  builds them in ≤255-slot chunks via the `EXTEND_LIST`/`EXTEND_TUPLE`/
  `EXTEND_SET`/`EXTEND_DICT`/`EXTEND_STR` ops (mirrors CPython's
  LIST_EXTEND/DICT_UPDATE/BUILD_STRING). Still overflowing: a call with >255
  positional args, a `{**a, …}` dict literal with >127 entries (the tag-packed
  `MKDICT_EX` site), and the rare >255-slot `MKFUNC`/class-base/`MATCH_CLASS`
  sites. CPython lowers all of these too; the same chunked treatment would extend
  to the call/spread paths.
- **A call with an ATTRIBUTE callee resolves it after its arguments.** CPython
  evaluates the callee first, then the arguments left to right. The bare-name
  callee now does the same (`aa(bb)` blames `aa`), but `compile_call`
  (`src/compiler.rs`) still folds the attribute lookup INTO `CALL_METHOD`, so
  `obj.m(g())` runs `g()` before resolving `m`:

      log = []
      def f(*a): log.append('call'); return 0
      def g(): log.append('arg'); return 1
      class K:
          def __getattr__(s, n): log.append('callee'); return f
      K().m(g())
      print(log)          CPython: ['callee', 'arg', 'call']
                          pythonrs: ['arg', 'callee', 'call']

  Only a callee with a side effect (`__getattr__`, a property, a module
  `__getattr__`) can tell the difference; a method that exists on the type has
  none. Fixing it means resolving the attribute to a value before the arguments,
  which costs a `Builtin` object plus a `BoundMethod` allocation per call —
  measured on a debug build, interleaved min-of-7 with the bytecode cache off,
  `xs.append(i)` **+18.4%**, `d.get("k")` **+12.4%**, a user method **+7.0%**.
  The bare-name change was taken because its measured cost was **+0.9%** for a
  user function and **+6.0%** for a builtin (after interning the builtin type
  objects); the method path was kept for that 12–18%. Closing it without the
  regression needs a resolve-first opcode that leaves `recv`/`name` on the stack
  when the type answers the name natively and only materializes a callable when
  the lookup would run user code.
- **No "did you mean" suggestions on a `SyntaxError`.** The `NameError` and
  `AttributeError` hints are implemented (see below); CPython also suggests a
  keyword for some `SyntaxError`s, which pythonrs does not.
- **`dir()` on a builtin type is not CPython's full slot listing.** Every name it
  reports is one `getattr` resolves (asserted in both directions by
  `builtin_dir_lists_only_dispatchable_names` /
  `builtin_dispatch_is_fully_listed_by_dir`), but the remaining slots
  (`__class_getitem__`, `__reduce_ex__`, `__sizeof__`, `__init_subclass__`, …)
  are dispatched natively rather than through per-type descriptor objects, so
  they are not enumerable. 476 of CPython's 767 names across the 13 builtin
  types. The BINARY OPERATOR slots are now real bound methods (see
  "Implemented"), which is what took the count from 418 to 476.
- **`collections.deque` implements none of its operators.** `deque + deque`,
  `deque * n` and `q += […]` all raise `unsupported operand type(s)`, so its
  `__add__`/`__iadd__`/`__mul__`/`__rmul__`/`__imul__` are deliberately kept out
  of the bound-method table — exposing them would only move the failure.
- **`TypeError` messages for a bad sequence repetition operand.** CPython says
  `can't multiply sequence by non-int of type 'str'` for `[1] * 'a'` and
  `'str' object cannot be interpreted as an integer` for the `__mul__` spelling;
  pythonrs says `unsupported operand type(s) for *: 'list' and 'str'` for both.
- **An unhashable key is not named at the container-op boundary.** CPython 3.12+
  wraps the error as `cannot use 'list' as a dict key (unhashable type: 'list')`
  / `... as a set element (...)`; pythonrs reports the bare
  `unhashable type: 'list'`. Measured across 16 shapes — dict/set displays,
  `d[k] = v`, `d[k]`, `get`, `in`, `set.add`, `dict(pairs)`, `set(iter)`,
  `frozenset(iter)`, `dict.fromkeys`, `setdefault`, and the dict/set
  comprehensions. Bare `hash([1])` correctly keeps the unwrapped message, and
  `{}.pop([1])` raises `KeyError` in CPython where pythonrs raises the
  `TypeError`. The traceback frame, source line and carets around it now match
  (see "Implemented"); only the message text differs.
- **`[nan] == [nan]` with one shared `nan` is False.** CPython's sequence
  comparison shortcuts on element IDENTITY before `==`, so a list holding the
  same `nan` object twice compares equal to itself. pythonrs stores a `float`
  unboxed, so two equal-valued floats are indistinguishable from one object and
  the shortcut cannot be reproduced. The identity shortcut IS applied to heap
  objects, which is what makes `[P(1)] == [P(1)]` and `[x] == [x]` correct for
  everything with a heap identity.

## Tooling
- **`--build`** (AOT to a standalone native executable): implemented for the
  **libpython-free** build (`cargo build --no-default-features`). An uncaught
  exception in the AOT binary renders the same traceback the interpreter does —
  `File`/source line + CPython carets — and exits non-zero (the embedded image
  carries the source, filename, and caret position tables, and the binary
  recomputes each chunk's serde-skipped `op_hash` so caret lookups hit).
  `sys.exit(n)` returns `n`. Two limits: (1) a `stdlib-ffi` build cannot AOT (its
  CPython/pyo3 symbols can't be statically linked into a standalone binary — the
  build fails up front with that instruction); (2) an error from a **native
  fast-path op** (`int + str`, unary `-` on a bad type) is held in fusevm's
  private AOT result rather than on the host, so it exits silently instead of
  printing a traceback — every builtin-dispatched error (index/key/type-via-
  method/name/division/attribute) renders correctly.
- **`--dap`** (Debug Adapter Protocol): implemented — breakpoints, step
  in/out/over/continue, stack trace, locals, and program-stdout capture (pipe +
  dup2 → `output` events). Frame names in the stack use the function name (or
  `<module>`), shared with the traceback path. Watch expressions not yet added.
- **`--lsp`**: full corpus — completion (builtins/keywords/methods), position-
  aware hover, and diagnostics via the real parser. Go-to-def and signature help
  not yet added.
- **REPL** echoes bare-expression values through `sys.displayhook` (CPython
  "single" mode: prints `repr(value)` for non-`None` top-level results and binds
  `_`); multi-line blocks close on a blank line. Passing `--repl` with piped
  (non-TTY) stdin runs the same interactive loop over the piped source, the
  analogue of `python3 -i < file`.

## Standard library

The **default build** ships the `stdlib-ffi` bridge, so a native fast-path subset
plus the entire CPython stdlib are importable out of the box. A
`--no-default-features` build serves only the native subset below; every other
module then raises `ModuleNotFoundError`.

- **Native in every build**: `math` (constants + a common function fast path;
  in a default build any symbol the native arm lacks — `isqrt`, `trunc`, `comb`,
  `hypot`, … — defers to the real CPython `math` over the FFI bridge), `sys`
  (`argv` from the process args, `exit`/`getrecursionlimit`/`setrecursionlimit`,
  `maxsize`, `version`/`version_info` reporting the emulated CPython `3.14.6`,
  `platform` (`darwin`/`linux`), `path`, `modules`, `executable`,
  `stdout`/`stderr`/`stdin` file objects), and `_thread` (the single-threaded
  primitives `threading` is built on — native under the bridge too, since a
  target handed to CPython's `_thread` would run on an OS thread whose pythonrs
  heap, a `thread_local`, is empty). `collections`'s four MUTABLE containers
  (`deque`, `Counter`, `defaultdict`, `OrderedDict`) are native in a default
  build as well: a CPython one would hand its values back through the by-value
  marshaler, so `dd['k'].append(1)` would mutate a throwaway copy. `namedtuple`
  is not shadowed — its instances are immutable, and CPython's builds the real
  `_tuplegetter` field descriptors (writable `__doc__`). `ChainMap`, `UserDict`,
  `UserList`, `UserString` and `collections.abc` defer to CPython. The
  `--no-default-features` build instead runs the full vendored
  `collections/__init__.py` over the native `_collections` accelerators.
  `textwrap` and `statistics` have
  native subsets too, but they cover only positional args, so under the FFI
  bridge (default) they defer to the real CPython modules (full keyword-option
  surface — `textwrap.fill(t, width=…)`); the native subsets serve only
  `--no-default-features`.
- **The rest of the stdlib is served by the `stdlib-ffi` bridge (on by default)**
  — an embedded libpython over pyo3, so `import json`/`os`/`random`/`string`/
  `functools`/`datetime`/`hashlib`/… load the **real CPython
  modules** (pure `.py` + the C accelerators), not hand-rolled shadows.
  `functools.partial`/`lru_cache`/`reduce`, `json`, `os` + `os.path`,
  `random` and `string` all come from CPython there. A bare `cargo build` works
  as-is (`.cargo/config.toml` pins `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1` for
  pyo3's 3.14 forward-compat check). **Only a `--no-default-features` build drops
  the bridge** — there `import functools`/`import os` all raise
  `ModuleNotFoundError`.
- **`re` and `itertools` are NATIVE shadows in BOTH builds — they never reach
  CPython.** This entry previously listed both among the modules the FFI bridge
  serves, which was wrong in a way that matters: a probe that "passes" against a
  bridged module proves CPython works, while a probe against these two proves
  pythonrs's own code works, and only the second kind is evidence about the port.
  `module_ffi_fallback` covers exactly `math`, `collections`, `functools` and
  `contextlib`; `re` is not in that list, so a miss on the native namespace is a
  hard `AttributeError` and never defers. Checking against CPython 3.14.6:
  `hasattr(re, 'Scanner')` and `hasattr(re, 'RegexFlag')` are both `False` here
  and `True` there; `hasattr(itertools, 'batched')` is `False` here and `True`
  there. `re` is the Rust `regex`/`fancy_regex` engines behind
  `src/regexpr.rs`, and its remaining gaps are listed under "Standard library —
  `re`" below.
- **FFI-boundary integration** — crossing the bridge with a pythonrs object.
  Working: `class C(enum.Enum)` (and other Foreign-base classes) are built by the
  real metaclass via CPython `types.new_class`, so members/`.name`/`.value`,
  singleton `is` identity, IntEnum/Flag, and body-defined methods all behave like
  CPython; a pythonrs generator marshals into a CPython call as a lazy iterator
  (`itertools.takewhile(pred, gen())` over an infinite generator); pythonrs
  callables carry a `__dict__` and expose the wrapped function's dunders, so
  `@functools.wraps` succeeds; pythonrs methods stored in a CPython-built class
  bind `self` (the `PyrsCallable` descriptor). A native pythonrs class also
  crosses into a CPython call — `@dataclass` mirrors it over `object` via
  `types.new_class` (methods as `PyrsCallable` descriptors, `__annotations__`/
  class-vars by value), so dataclass installs `__init__`/`__repr__`/`__eq__`/
  ordering and the result rebinds the name. Class bodies capture their simple
  annotations into `__annotations__`, so `Cls.__annotations__`, `@dataclass`, and
  `typing.NamedTuple` all see the fields. Function parameter/return annotations
  are also kept: `def f(a: int) -> str` builds `f.__annotations__` at def time
  (evaluated eagerly, keys in source order with `"return"` last), reachable on a
  bound method too; a bare builtin type in an annotation (`Optional[int]`) crosses
  into CPython as the real `int` type, so `typing` generics build correctly. A
  pythonrs *instance* also crosses into a CPython call as a `PyrsInstance` proxy
  (attribute/item access, comparison, hashing, repr route back to the fusevm
  object), so `operator.attrgetter("x")(obj)` / `sorted(objs, key=itemgetter(0))`
  work. `functools.total_ordering` and `functools.cached_property` run natively
  (the class stays a native pythonrs class): `total_ordering` derives the missing
  rich-comparison ops from the one defined ordering method plus `__eq__`, and
  `cached_property` is a non-data descriptor that computes on first access and
  caches into the instance dict (later reads hit the dict; a `__slots__` instance
  with no dict raises CPython's `TypeError`). Every other `functools` member
  (`reduce`, `partial`, `lru_cache`, `wraps`, `cmp_to_key`) defers to the real
  CPython module. `int(x)` of a foreign value converts via CPython's `int()` (an
  `IntEnum` member, `Fraction`, …); `isinstance(v, foreign_cls)` against a CPython
  ABC (`collections.abc.Sequence`, …) marshals `v` and lets CPython's structural
  `__instancecheck__` decide, and the mirror direction works too — a CPython
  object behind a handle tested against a NATIVE builtin type (`isinstance(
  namedtuple_instance, tuple)`) resolves the type name out of CPython's
  `builtins` and asks CPython, since the handle reports only its own class name
  and the native structural check has no base chain to walk; and a CPython
  exception raised over the bridge (e.g.
  `dataclasses.FrozenInstanceError`) is caught by `except Exception`. A foreign
  exception also matches a **specific base**: its `__mro__` base names are captured
  at raise time, so `except ValueError` catches a `json.JSONDecodeError` and
  `except ArithmeticError` catches `decimal.InvalidOperation`; the exact foreign
  type (`except json.JSONDecodeError`) matches by its CPython `__name__`. A
  `@dataclass` instance also matches a `match` class pattern (positional via
  `__match_args__`/keyword), routed through CPython `isinstance` + bridge attribute
  reads.
  Remaining gaps:
  - **`collections.namedtuple` field *types*** cross as `PyrsCallable` wrappers,
    not the CPython type objects, so `dataclasses.fields(x)[i].type` on a mirrored
    class is a proxy — the generated `__init__`/`__repr__`/`__eq__` (which use only
    field names) are unaffected.
  - **A class with a foreign base cascades to a foreign class**, so a zero-arg
    `super()` in one of its methods raises `super(): no arguments` — the pythonrs
    method runs on the CPython mirror without a native `__class__`/`self` frame.
    This bites `class C(abc.ABC)` hierarchies (`abc.ABC` is foreign): native
    `abc.ABC`/`@abstractmethod` are not yet recognized, so use a plain base class
    (a method raising `NotImplementedError`) for now. A native-base hierarchy's
    `super()`/MRO is unaffected.

### Standard library — `re`

`re` is native (see the entry above): the Rust `regex` engine, with
`fancy_regex` taking the patterns that need look-around or backreferences.

**Implemented (previously a silent wrong answer): every reported position is a
CODEPOINT index, not a byte offset.** Both engines index a `&str` by byte, and
every span, `pos` and slice inside `src/builtins.rs`'s `re` implementation is a
byte offset — which is what the slicing needs. CPython's `re` indexes a `str` by
codepoint. The two agree on ASCII and only on ASCII, so on any other subject
every position was wrong with no error raised:
`re.search('b', 'éb').span()` was `(2, 3)` against CPython's `(1, 2)`, and
`[m.span() for m in re.finditer(r'.', 'aéb')]` was
`[(0, 1), (1, 3), (3, 4)]` against `[(0, 1), (1, 2), (2, 3)]` — so
`s[m.start():m.end()]` did not even reproduce `m.group()`. The conversion now
happens at each of the five places a position crosses to or from Python and
nowhere else, so the stored spans stay byte offsets and the slicing that reads
them stays correct:

  - `Match.start()`/`.end()`/`.span()` (`re_match_method`), for every group and
    for a named group;
  - `Match.pos`/`.endpos`, which were additionally hard-coded to `0` and to the
    BYTE length — a match now records the window it was found in;
  - `repr(Match)`, which renders the group-0 span;
  - the `pos`/`endpos` ARGUMENTS of `Pattern.match`/`.search`/`.fullmatch`,
    which arrive as codepoint indices (Python computed them with `len()`).
    Consumed as bytes, `re.compile(r'.').search('aéb', 1)` sliced into the
    interior of `'é'` and reported NO MATCH at all;
  - the `fullmatch` end-of-window comparison, which stays on the byte basis
    because both sides of it are internal.

The pair `regexpr::char_index_of`/`byte_index_of` is the single definition of
that boundary. Regression test: `re_positions_count_codepoints_not_bytes` in
`tests/stdlib.rs`, and the `regex` mode of `parity-fuzz`, whose subjects mix
1-, 2-, 3- and 4-byte characters in one string so that neither `byte == char`
nor `byte == k*char` can carry a wrong implementation.

Still open:
- **A `bytes` subject is rejected.** `re.search(rb'b', b'ab')` raises
  `TypeError: expected string`; CPython matches and reports BYTE offsets there
  (`span() == (1, 2)` on `'aéb'.encode()` is `(3, 4)`). A bytes PATTERN compiles
  (each byte is decoded as latin-1, which `json` relies on), but the subject must
  be a `str`. Supporting it means carrying a bytes/str flag on the match so the
  position conversion above is skipped.
- **`Match.regs`, `Match.re`, `Match.lastgroup` and `Match.expand()` are
  missing** — all four raise `AttributeError`. `m.regs` is the span tuple
  (`((1, 3), (1, 2), (2, 3))`), so it is a position API and would convert the
  same way. `m[i]` (`Match.__getitem__`) raises `TypeError`; `m.group(i)` works.
- **`finditer` is eager.** It builds every match and returns a list iterator, so
  `type(...).__name__` is `iterator` where CPython says `callable_iterator`, and
  a scan over a huge subject materializes all of it.
- **`re.Scanner` and `re.RegexFlag` are absent** (the flag constants exist as
  plain ints; `re.A`/`re.I`/`re.M`/`re.S`/`re.X` all resolve).

### `hash()` values, and the containers that follow from them

**`hash(x)` does not return CPython's number for any type.** Every non-instance
key is hashed with Rust's `DefaultHasher` (`builtins::hash_key`), so the value is
internally consistent but bears no relation to CPython's. Measured against
CPython 3.14.6:

| expression | pythonrs | CPython |
| --- | --- | --- |
| `hash(0)`, `hash(1)`, `hash(-1)` | `110868633497842534`, `-2132787436984217633`, `-191408919806194774` | `0`, `1`, `-2` |
| `hash(2**61-1)`, `hash(2**61)`, `hash(2**62)` | arbitrary | `0`, `1`, `2` |
| `hash(0.5)`, `hash(-0.5)` | arbitrary | `1152921504606846976`, `-1152921504606846976` |
| `hash(float('inf'))`, `hash(float('-inf'))` | arbitrary | `314159`, `-314159` |
| `hash(True)`, `hash(False)`, `hash(None)` | arbitrary | `1`, `0`, `4238894112` |
| `hash(1+2j)` | arbitrary | `2000007` |
| `hash(())`, `hash((1,2))`, `hash(frozenset([1]))` | arbitrary | `5740354900026072187`, `-3550055125485641917`, `-558064481276695278` |

This is not an implementation detail CPython leaves open. The numeric hash is
specified — `hash(n) == n % (2**61 - 1)` with `-1` mapping to `-2`, and the same
modular scheme extended to `float`, `complex` and `Fraction` so that any two
equal numbers hash equally. `str`/`bytes` hashing IS randomized (SipHash under
`PYTHONHASHSEED`) and is the documented boundary; the numeric side is not.

**The consequence is a container bug, not just a cosmetic number.** A bridged
CPython object carries CPython's own hash (`PKey::Foreign { hash, .. }` from
`ffi::foreign_hash`), while a native `int`/`float` carries the `DefaultHasher`
one — so a value-equal pair straddling the bridge lands in two different slots
and never collapses:

```
len({1, Decimal(1)})            # pythonrs 2, CPython 1
len({0.5, Fraction(1, 2)})      # pythonrs 2, CPython 1
d = {1: 'int'}; d[Decimal(1)] = 'dec'
d                               # pythonrs {1: 'int', Decimal('1'): 'dec'}
                                # CPython  {1: 'dec'}
```

Both parts are one fix: give `hash_key` CPython's algorithm for `int`
(`long_hash`, already ported as `host::bigint_pyhash` but applied only to a
user `__hash__` RESULT — `host::cpython_int_hash` is the machine-int arm and is
wrong above `2**61-1`, where it returns `n` instead of reducing), `float`
(`_Py_HashDouble`), `complex` (`hashreal + 1000003 * hashimag`), `None`
(`0xFCA86420`), `tuple` (the xxPRIME accumulator) and `frozenset`
(`_shuffle_bits`); then widen `prepare_key_walk`'s collapse loop, which today
only considers a candidate that is ALSO a `PKey::Foreign`, so a native key is
never a collapse target for a foreign one or the reverse.

The cross-type collapse WITHIN native numbers is already correct and must stay
so: `len({1, 1.0, True})` is 1, `hash(1) == hash(1.0) == hash(True)`,
`hash(0.0) == hash(-0.0)`, and `{1: 'a', 1.0: 'b', True: 'c'}` is `{1: 'c'}`.

Not reachable from `parity-fuzz`: the six `hash(` sites in the generator all
compare `hash(x) == hash(y)`, which any self-consistent hash satisfies. Closing
this needs a mode that prints the raw value.

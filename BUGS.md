# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython 3.14.6 on the example corpus). This file is the
honest list of what is **not** yet covered, so nobody mistakes a gap for a bug
fixed. Every line below was re-checked against the **default-build** binary
(`cargo build` — default features, so the `stdlib-ffi` bridge is ON) before being
written.

## Implemented (previously listed here as gaps)
- **Source too deeply nested no longer aborts the process.** Five shapes killed
  the interpreter thread outright — `fatal runtime error: stack overflow`,
  SIGABRT, exit 134, no traceback and nothing for `except` to see:
  `exec('('*10000)`, `'-'*100000+'1'`, `'a'+'.b'*100000`, `'1'+'+1'*200000` and
  `'not '*20000+'1'`. CPython answers all five with an ordinary catchable
  exception. The tokenizer now carries CPython's `MAXLEVEL` — 200 open brackets,
  one counter shared by `(`, `[` and `{`, so `'([{'*67` trips it too — and
  refuses the 201st with `SyntaxError: too many nested parentheses` (measured:
  `compile('('*200+'1'+')'*200, …)` compiles on 3.14.6 and `'('*201` does not,
  for all three bracket kinds). Bracket-free operator chains nest just as deeply,
  so the parser also carries a tree-depth cap (`parser::MAX_TREE_DEPTH`, 20 000)
  reported as CPython's own `MemoryError: Parser stack overflowed - Python source
  too complex to parse`. The cap sits above every depth CPython accepts in those
  shapes (`'1'+'+1'*20000` and `'a'+'.b'*20000` parse there, `*100000` does not)
  and below where the 512 MB interpreter stack in `src/main.rs` runs out
  (measured: those shapes survive 25 000 levels and abort by 30 000 on a debug
  build). See "Partial / simplified semantics" for the two limits that remain.
- **A format spec with too many width or precision digits raises instead of
  panicking.** `parse_internal_render_format_spec` accumulated digits with a
  plain `*`/`+` on a `usize`, so `format(1, '1' + '0'*20 + 'd')` — and
  `'{:{}d}'.format(1, 10**20)`, which splices its argument in as spec text —
  aborted with "attempt to multiply with overflow", which no `except` can catch.
  CPython's `get_integer` raises `ValueError: Too many decimal digits in format
  string`. The accumulator is checked against `Py_ssize_t`, not `usize`, because
  `'9'*19` fits one and not the other and CPython rejects it; a precision past
  `INT_MAX` keeps its own `ValueError: precision too big`; and a width the
  allocator refuses is `MemoryError` (via `try_reserve`) rather than an abort,
  matching `format(1, '9'*18 + 'd')`.
- **A repetition too large to allocate is `MemoryError`, not an abort.**
  `Vec::with_capacity` and `str::repeat` abort on a failed allocation, so
  `'a' * (2**48)` printed `memory allocation of 281474976710656 bytes failed` and
  exited 134. The result length is reserved fallibly now: `[1]*(2**48)`,
  `'a'*(2**62)` and `(1,)*(2**62)` raise `MemoryError` as CPython does, and the
  bytes path raises CPython's own `OverflowError: repeated bytes are too long`.
- **An `int` too large for `Py_ssize_t`, used as an index, a count or a
  length.** `PyHost::as_int` answers `None` for a bignum exactly as it does for
  a string, so all three failure modes collapsed into one and every site
  reported the wrong thing — or, worse, read the `None` as "argument omitted"
  and silently produced a different answer. `PyHost::index_fit` keeps
  "fits" / "too large" / "not an int" apart, and each site reports what CPython
  reports:
  - a subscript is `IndexError: cannot fit 'int' into an index-sized integer`
    (`[1][10**30]`, `'a'[10**20]`, `b'a'[10**20]`, `memoryview(b'ab')[10**30]`,
    `l[10**30] = 2`, `del l[10**30]`) — it was
    `TypeError: list indices must be integers or slices, not int`. `range` is
    the exception: it computes in arbitrary precision, so `range(10)[10**30]` is
    `IndexError: range object index out of range`;
  - a repetition or a length is `OverflowError: cannot fit 'int' into an
    index-sized integer` (`[1]*(10**20)`, `b'a'*(10**20)`, `bytes(10**20)`,
    `bytearray(10**20)`); the `Py_ssize_t` conversion runs before the sign check,
    so `bytes(-10**30)` is that too rather than `ValueError: negative count`;
  - an Argument Clinic `Py_ssize_t` parameter is `OverflowError: Python int too
    large to convert to C ssize_t` (`ljust`/`rjust`/`center`/`zfill`,
    `split`/`rsplit`'s maxsplit, `replace`'s count, `int.to_bytes`'s length,
    `'%*d'`'s width). Each of these previously reverted to its DEFAULT and
    answered silently: `'abc'.ljust(10**20)` was `'abc'`,
    `'abc'.replace('b','x',10**20)` replaced everywhere;
  - the two C-`int` parameters name that width instead —
    `'a\tb'.expandtabs(10**20)` and `'%.*f' % (10**20, 1.5)`.

  A range longer than `Py_ssize_t` refuses to materialize instead of looping
  forever: `list(range(10**25))` built a vector nothing could hold, with no
  panic and no error to interrupt it, where CPython's `PyObject_LengthHint` asks
  `range.__len__` first and raises `OverflowError: Python int too large to
  convert to C ssize_t`. `len(range(10**25))` raised already but named the wrong
  C type. A bignum range that is SHORT (`range(10**30, 10**30+5)`) still
  materializes.

  A slice bound SATURATES rather than raising, because `_PyEval_SliceIndex`
  passes a NULL exception type to `PyNumber_AsSsize_t`. Read as "omitted", every
  one of these returned the whole sequence; they now match CPython:
  `'abc'[10**30:]` is `''`, `'abc'[::10**30]` is `'a'`, `'abc'[::-10**30]` is
  `'c'`, `[1,2,3][10**30:]` is `[]`, `range(10)[10**30:]` is `range(10, 10)`.
  And `chr` read the bignum as `None` and then as `0`, so `chr(10**30)` printed a
  NUL where CPython raises `ValueError: chr() arg not in range(0x110000)`; a
  non-integer argument is now the `__index__` `TypeError` CPython gives rather
  than that `ValueError`.
- **Binary-mode file reads answer `bytes`.** Every read path decoded UTF-8
  unconditionally, so `type(open(p, 'rb').read())` was `str` and a file holding a
  byte that is not valid UTF-8 died with `OSError: stream did not contain valid
  UTF-8` — CPython returns the bytes. `read`/`read(n)`/`readline`/`readlines`/
  iteration all answer `bytes` on a `'b'` handle now, `write` rejects the wrong
  operand type in both directions (`TypeError: a bytes-like object is required,
  not 'str'` on a binary handle, `TypeError: write() argument must be str, not
  bytes` on a text one), and `open()` on a DIRECTORY raises
  `IsADirectoryError: [Errno 21] Is a directory` rather than handing back a
  handle that only fails at read time. Text mode is unchanged (a multi-byte
  character still counts as one `read(n)` character).
- **`OSError` carries `errno`, `strerror`, `filename`, `filename2`.** It was a
  one-string exception: the whole rendered line sat in `args[0]` and none of the
  four attributes existed, so `if e.errno == errno.ENOENT:` — the ordinary way to
  discriminate an `OSError` — raised `AttributeError` from inside the handler.
  `synth_exc` now splits `[Errno N] strerror: 'filename'` the way CPython's
  `oserror_init` splits its arguments, so `open('/no/such/file')` gives
  `args == (2, 'No such file or directory')`, `errno == 2`,
  `filename == '/no/such/file'`, `filename2 is None`. Any `open` failure other
  than the three that were hard-coded keeps the OS's own errno and maps it to
  CPython's subclass.
- **`NameError.name` and `AttributeError.name`.** Both attributes were absent, so `except NameError as e: e.name` raised from inside the
  handler. `AttributeError.obj` is still absent — see below.
- **A regex group NUMBER out of range raises.** `Match.group` accepted any
  integer and read its span vector out of bounds, answering `None` — which is
  the value CPython reserves for a group that EXISTS and did not participate in
  the match, so a caller distinguishing the two saw the wrong one.
  `re.match('(a)','a').group(5)`, `.group(-1)` and `.group(0, 9)` are
  `IndexError: no such group`, and a group that really did not match is still
  `None`.
- **`sys.setrecursionlimit` validates its argument.** The whole call was
  `Ok(Value::Undef)`, so `sys.setrecursionlimit(0)` — which CPython refuses —
  was accepted silently. It reports
  `ValueError: recursion limit must be greater or equal than 1` below 1,
  `OverflowError: Python int too large to convert to C int` past a C `int`, and
  the `__index__` `TypeError` for a non-integer. The limit itself is still not
  enforced; see below.
- **The `n` presentation type.** `n` reached no arm of the renderer at all and
  fell through to the no-type one, so `format(1234567.891, 'n')` printed the
  `repr` (`1234567.891`) where CPython gives `1.23457e+06`, and
  `format(True, 'n')` printed `True` where CPython gives `1`. It now renders as
  `d` for an int-like value and `g` for a float — `format_float_internal`
  literally does `if (type == 'n') type = 'g'` — and takes its separator, group
  WIDTHS and decimal point from `localeconv()`, so `format(1234567, 'n')` under
  `de_DE` is `1.234.567` and under `hi_IN` is `12,34,567` (grouping `[3, 2, 0]`,
  not a fixed three). `_PyUnicode_InsertThousandsGrouping` and its
  `GroupGenerator` are ported for the variable widths and the `0`-flag
  interleave. `,n` and `_n` are both rejected (`n` brings its own separator) and
  a precision on an int `n` is rejected as it is for `d`.
- **The `#` alternate form on a float conversion.** `Py_DTSF_ALT` keeps a
  decimal point even when the precision rounded every fraction digit away:
  `format(1.0, '#.0f')` is `1.`, `'%#.0e' % 1.0` is `1.e+00`,
  `format(1.0, '#.0%')` is `100.%`. All of these dropped the point. Relatedly
  `fmt_g` short-circuited any zero to the string `"0"`, which lost both the sign
  of `-0.0` (`format(-0.0, 'g')` is `-0`) and the flag (`format(0.0, '#g')` is
  `0.00000`).
- **A bignum through a float presentation type.** `as_f` stops at `i64` and the
  fallback was `.unwrap_or(0.0)`, so `format(10**20, 'f')` printed
  `0.000000` instead of `100000000000000000000.000000`. It now converts as
  `PyNumber_Float` does and raises `OverflowError: int too large to convert to
  float` past `f64`. `'%d' % 1e30` likewise went through an `i64` cast that
  truncated; it is exact now, and `'%d' % float('inf')` raises
  `OverflowError: cannot convert float infinity to integer` rather than printing
  `9223372036854775807`.
- **Grouping stops at the digits.** `parse_number` counts the leading run of
  ASCII digits and everything after it is remainder, so a separator can no
  longer land inside an exponent or a suffix: `format(1, '_.0%')` is `100%` (was
  `1_00%`), `format(1.5, ',.0')` is `2e+00` (was `2e,+00`), and a non-finite has
  ZERO digits so `format(float('inf'), '012,f')` is `000000000inf` (was
  `0,000,000,inf`).
- **The `0` flag keys off the FILL, not the alignment.**
  `parse_internal_render_format_spec` takes `0` as the fill whenever no fill char
  was named — naming an alignment is not enough. `format(1, '<08d')` is
  `10000000`; it used to be `1` padded with spaces because the explicit `<`
  suppressed the flag.
- **`c` rejects a sign and the alternate form.** `format(65, '+c')` /
  `format(65, '-c')` / `format(65, ' c')` are
  `ValueError: Sign not allowed with integer format specifier 'c'` and
  `format(65, '#c')` is the matching `Alternate form (#) …`; all four used to
  succeed. `'%c' % (10**20)` is `OverflowError: %c arg not in range(0x110000)`
  rather than a `TypeError` — an int too large is a RANGE error, not a type one.
- **`PYTHONHASHSEED` is honoured for every seed, not just `0`.** See the
  `hash()` section below.
- **`functools.wraps` copies `__doc__` and `__module__` across the bridge.**
  Every pyclass answers those two names from its own type (`None` and
  `"builtins"`), so normal attribute lookup succeeded and the proxy's
  `__getattr__` never fired for them — `functools.wraps(f)` copied `None` over
  the wrapped function's docstring and `"builtins"` over its module. Both are
  getset pairs now, delegating to the wrapped callable until something assigns.
- **`iter()`/`next()` honor the user iterator protocol.** `iter(x)` called
  `PyHost::make_iter` directly, which cannot run Python, so a class defining
  `__iter__`/`__next__` was `TypeError: 'Count' object is not iterable` and
  `next()` on one was `TypeError: not an iterator` — even though `for x in
  Count()` worked, because the loop took a different path. `iter(x)` now runs
  `type(x).__iter__` and hands back its result UNCHANGED (so an object whose
  `__iter__` returns `self` keeps its identity and an unbounded iterator is
  never drained), falls back to the `__getitem__` sequence protocol when there
  is no `__iter__`, and rejects a non-iterator result with CPython's
  `iter() returned non-iterator of type 'int'`. `next()` steps a user `__next__`
  outside the host borrow, treating `StopIteration` as exhaustion, and names a
  non-iterator as `'N' object is not an iterator`.
- **Every builtin iterator reports its own CPython type name.** All of them
  answered `iterator` — the name CPython reserves for the `__getitem__` sequence
  iterator alone. The snapshot cursor now carries an `IterKind` tag, so
  `type(iter(x)).__name__` is `list_iterator` / `tuple_iterator` /
  `str_ascii_iterator` / `str_iterator` / `bytes_iterator` /
  `bytearray_iterator` / `set_iterator` / `memory_iterator` /
  `_deque_iterator` / `dict_keyiterator` / `dict_valueiterator` /
  `dict_itemiterator` / `range_iterator` / `longrange_iterator`, and `reversed`
  splits into `list_reverseiterator`, the three `dict_reverse*iterator`s, and
  the generic `reversed`.
- **`co_flags` matches the 3.14 compiler.** `CO_NOFREE` (0x40) was set whenever
  a function had no free variables, so `def f(): pass` reported 67; 3.14's
  compiler never sets that bit and reports 3. `dis.COMPILER_FLAG_NAMES` still
  NAMES the bit, which is what made the stale value look right. The three flags
  that do apply are now derived from `__qualname__` and the docstring:
  `CO_NESTED` (0x10) for any function inside another function's scope,
  `CO_METHOD` (0x8000000, new in 3.14) for one defined directly in a class body,
  and `CO_HAS_DOCSTRING` (0x4000000) when the body opens with a string.
- **`Cls[T]` requires `__class_getitem__`.** Every user class was treated as
  parameterizable, so `class Box: pass` silently accepted `Box[int]` as a
  `types.GenericAlias` where CPython raises. A class now parameterizes only when
  `__class_getitem__` is in its MRO; a metaclass `__getitem__` is dispatched as
  ordinary indexing (it outranks the alias reading); and the rejection names the
  class itself — `type 'Box' is not subscriptable`, not `'type' object is not
  subscriptable`. `tuple[int, ...]` also prints the ellipsis in its literal
  spelling rather than as `Ellipsis`.
- **`types.UnionType` is `typing.Union`.** 3.14 merged the PEP 604 type into
  `typing`, so `type(int | str)` reports `__name__ == 'Union'`,
  `__module__ == 'typing'`, `repr` `<class 'typing.Union'>`, and messages such
  as `'typing.Union' object is not callable`. pythonrs still answered with the
  pre-3.14 `builtins.UnionType` spelling.
- **`import typing` works.** `typing._SpecialForm` declares
  `__slots__ = ('_name', '__doc__', '_getitem')` with no docstring. pythonrs
  seeded `__doc__` into every class namespace unconditionally, so the slot check
  saw a class variable that CPython's compiler never emits and the import died
  with `ValueError: '__doc__' in __slots__ conflicts with class variable`,
  taking the whole module with it. The default is now skipped exactly when the
  body slots `__doc__` and has no docstring.
- **`__debug__` is bound.** It is a builtin constant, so `if __debug__:` — the
  ordinary spelling of a debug-only block — was a `NameError` in every scope. It
  now resolves everywhere and is False exactly when the interpreter is
  optimized; `-O`/`-OO` are folded into `PYTHONOPTIMIZE` so both spellings share
  one source of truth, with CPython's lax parse (empty is 0, an integer is that
  integer, any other non-empty value is 1). Assert stripping under `-O` is still
  not implemented — see "Partial / simplified semantics".
- **`function.__isabstractmethod__` raises `AttributeError`.** The slot belongs
  to `staticmethod`/`classmethod`/`property`, not to `function`; answering
  `False` on a plain function hid the real shape (`abc` reads it with a
  `getattr(…, False)` default precisely because the attribute is absent).
  `property` gained the slot it was missing.
- **A mutable container reached through a bridged CPython object keeps its
  identity.** The marshaller converted an exact CPython `list`/`dict`/`set` to a
  native value on every read, which is right for a call RESULT (a fresh object
  the caller owns; arguments passed IN are already written back by
  `writeback_mutated_args`) and wrong for a reference into a live object. With
  `@dataclass class P: tags: list = field(default_factory=list)`,
  `p.tags is p.tags` was `False` and `p.tags.append(3)` mutated a copy that was
  then discarded. An attribute or item read that yields a mutable container now
  keeps it behind the `Foreign` handle (`ffi::reference_to_value`), so identity
  holds and the mutation lands on the real object; `__setitem__`/`__delitem__`
  are routed too, and a slice crosses as a real `slice` (built through the
  `slice` builtin, so an omitted bound is `None` rather than a sentinel int).
  The rule applies at every depth — `d.m['k'].append(2)` reaches the inner list
  by ITEM access on an already-bridged dict. Immutable containers (`tuple`,
  `frozenset`, `bytes`, `str`, scalars) still cross by value: nothing can
  observe the difference and operations on them stay native.
- **Private-name mangling.** Every `__name` written inside a class body now
  compiles as `_Class__name` (CPython `_Py_Mangle`), so `C().__dict__` reads
  `{'_C__x': 1}`, the `AttributeError` names `_F__missing`, and two classes in
  one hierarchy can each keep a private `__x` without aliasing. The rewrite
  (`src/mangle.rs`) runs in the compiler on the parsed AST, not in the parser —
  `ast.parse` must keep showing the name as written, and it reaches the same
  `parser::parse` without passing through `compile`. It covers attribute access,
  plain names, `def`/`class` names, parameters, `global`/`nonlocal`, `import`
  and `except ... as` bindings, and `match` captures; a CALL keyword
  (`f(__k=1)`) is not an identifier reference and is left alone, as are `__x__`
  and `_z`. Leading underscores are stripped from the class name (`_K` -> `_K__v`,
  `__L` -> `_L__v`) and the innermost enclosing class wins. Slot names mangle for
  the descriptor they install while `__slots__` keeps the tuple as written, so
  `__slots__ = ('__x',)` beside a `_C__x = 1` class variable now raises
  `ValueError: '_C__x' in __slots__ conflicts with class variable`. This changes
  emitted bytecode, so `cache::SCHEMA` went to 49.
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
  inserts itself — are exempt from the conflict check, and so is `__doc__` in a
  body that has no docstring (CPython's compiler emits that store only for a
  real one, so there is nothing for the slot descriptor to collide with).
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
- **File I/O**: `open()` (text and binary, read/write/append), `.read`/`.readline`/
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

- **The `itertools`/`collections`/`math` container surface that no probe
  exercised.** Found by diffing the names `src/builtins.rs` dispatches against
  the identifiers the fuzz corpus actually writes: a keyword-only argument, a
  function nobody called, or a method absent from the note-taker's list is
  invisible to a curated corpus no matter how many cases run. All of the below
  are now covered by `parity-fuzz --mode containertail` (4 000 cases, zero
  divergences):
  - `itertools.accumulate(initial=)` was **ignored**. The seed is yielded before
    the source is touched, so the result is one longer than the input and
    `accumulate([], initial=5)` is `[5]` — pythonrs answered `[]`, and
    `accumulate([1,2,3], operator.mul, initial=10)` answered `[1, 2, 6]` instead
    of `[10, 10, 20, 60]`.
  - `itertools.batched` did not exist (`AttributeError: module 'itertools' has no
    attribute 'batched'`), including its `strict=` form and its two ValueErrors
    (`batched(): incomplete batch`, `n must be at least one`). `pickle` batches
    its APPENDS/SETITEMS through it.
  - `itertools.count(start, step)` coerced both through `as_int`, so
    `count(1.5, 0.5)` counted `0, 1, 2` — a silently wrong answer, not an error.
    Start and step are added with the numeric `+` now, so floats count in floats
    and a bignum start stays exact.
  - `repr` of `count`/`repeat` printed the generic
    `<itertools.count object at 0x…>`; CPython gives both a constructor-style
    repr reporting LIVE state (`count(3)` after two pulls, `repeat('x', 2)`
    after one).
  - `collections.deque.insert` did not exist. It clamps like `list.insert`,
    accepts a negative index, and — unlike `append` — REFUSES on a full bounded
    deque with `IndexError: deque already at its maximum size` rather than
    evicting from the far end.
  - `Counter` held **only ints**: every count went through `as_int`, so
    `Counter(a=1.5)` stored `0` and `Counter(a=10**30)` stored `0`. The
    constructor, `update`, `subtract`, `total`, `most_common`, `elements`, the
    multiset operators and the unary forms all carry counts as values now, added
    with the numeric `+`/`-`. `Counter.update`/`subtract` also **dropped their
    keyword counts entirely** — `c.update(a=2)` was a silent no-op.
  - `Counter.__repr__` used insertion order; CPython's is
    `f'Counter({dict(self.most_common())!r})'` — descending by count, stable, so
    ties keep insertion order. `Counter(a=3, b=-1, c=0, d=0)` reprs as
    `Counter({'a': 3, 'c': 0, 'd': 0, 'b': -1})`.
  - Unary `+c` / `-c` on a Counter were a `TypeError: bad operand type for unary
    +: 'Counter'`. CPython defines them as `c - Counter()` and `Counter() - c`,
    so both drop non-positive counts — the pair that splits a signed tally into
    its gains and its losses.
  - `defaultdict.default_factory` did not exist in either direction. It reads
    back the factory (or `None`) and is writable — assigning `None` turns the
    defaultdict back into a KeyError-raising dict.
  - `OrderedDict.popitem(last=)` raised `TypeError: dict.popitem() takes no
    arguments (1 given)`, so the ordered form could only pop LIFO. `last=False`
    is how an OrderedDict is used as a FIFO queue. Its empty-dict `KeyError` also
    carries `'dictionary is empty'`, not `dict`'s
    `'popitem(): dictionary is empty'`.
  - `math.prod(start=)` was ignored — `prod([2,3], start=4)` answered `6`. The
    start also fixes the RESULT TYPE of an empty iterable: `prod([], start=2.5)`
    is `2.5`, not `1`.

- **`sys.stdlib_module_names`, and the NameError hint built on it.** The
  attribute did not exist, and with it missing `print(functools)` reported a bare
  `NameError: name 'functools' is not defined` where CPython adds
  `. Did you forget to import 'functools'?` (and, when a near miss also matches,
  the stacked `. Did you mean: 'funtools'? Or did you forget to import
  'functools'?`). CPython ships the name table as a generated static list;
  pythonrs COMPUTES it from the three places a stdlib module can actually come
  from — `sys.builtin_module_names`, the native-only arms of
  `import_module_inner`, and the bundled `pylib/` tree — so the set can never
  advertise a module the interpreter would fail to import. CPython's own
  exclusions are ported (`Tools/build/generate_stdlib_module_names.py`'s `IGNORE`
  set, plus the install-only `_sysconfigdata_*` / `sitecustomize` /
  `usercustomize` names its generator never sees). Measured: 217 names, every one
  of them present in CPython 3.14.7's 297 — zero false positives.

- **A pythonrs value that crossed into CPython and back came home as a NEW
  object.** `py_to_value` had no case for the four proxy pyclasses this crate
  hands out (`PyrsCallable`, `PyrsIterator`, `PyrsInstance`, `PyrsFile`), so a
  round trip through any stdlib API that merely stores a value and returns it
  minted a fresh `Foreign` handle and `is` went False. The proxy is unwrapped on
  the way back now. `functools.wraps` needed a second half: it does
  `setattr(wrapper, '__name__' / '__doc__' / '__wrapped__', …)` and then RETURNS
  the wrapper, and every one of those assignments landed in the proxy's own
  `__dict__` and died with it — the decorated function kept its original
  `__name__` and had no `__wrapped__` at all. `PyrsCallable.__setattr__` writes
  through to the wrapped pythonrs callable, which is what CPython's in-place
  semantics mean.

- **`int` → `float` conversion saturated instead of raising.** CPython reads an
  `int` operand of a mixed arithmetic expression through `PyLong_AsDouble`, which
  RAISES past the `f64` range. `num_val` saturated to `inf`, so a wrong NUMBER
  travelled where an error was due: `(2**2000) * 1.0` was `inf` and
  `(2**2000) // 1.0` was `nan`. Arithmetic now reads operands through
  `num_val_arith`. Comparison deliberately keeps saturating — CPython never
  converts there, and `(2**2000) > 1.0` must stay `True`. `float(2**2000)` raises
  too.
- **`int / int` divided in the FLOAT domain.** Both sides were read as `f64` and
  divided, so past the `f64` range the answer was not merely imprecise but
  absent: `2**2000 / 2**1999` came out `inf / inf` = `nan` instead of `2.0`, and
  a representable quotient was reported as overflow because an OPERAND alone did
  not fit. `bigint_true_divide` now runs CPython's `long_true_divide` — the
  quotient is formed in the integer domain and rounded once, scaled to 55
  significant bits with the low bit forced odd so the two-step rounding is exact
  (one spare bit is not enough: an odd quotient is then itself the tie, which
  cost an ulp on `(10**20) / 3`). 4000 randomized bignum divisions agree with
  CPython bit-for-bit, compared as `float.hex`.
- **`2.0 ** 10000` returned `inf`.** CPython's `float_pow` reports the C
  library's ERANGE as `OverflowError: (34, 'Result too large')`. Only a FINITE
  pair can overflow into one, so `float('inf') ** 2` stays `inf`. Relatedly
  `(-1.0) ** float('inf')` answered `(nan+nanj)`: `fract()` of an infinity is
  NaN, which compares unequal to `0.0` and sent every infinite exponent down the
  "negative base to a non-integer power is complex" path. C99 gives
  `pow(-1.0, inf) == 1.0`.
- **`range()` named itself instead of the offending type.** `range(1.5)` said
  `'range' requires integer arguments`; CPython uses the vocabulary every
  index-taking builtin shares — `'float' object cannot be interpreted as an
  integer`.
- **Container dunders were granted to every value.** `__len__`, `__getitem__`,
  `__setitem__`, `__delitem__`, `__iter__`, `__contains__` and `__bool__` were
  exposed as bound methods on any builtin, which is observable: `hasattr(5,
  '__len__')` was True and `(1, 2).__setitem__` handed back a bound method for a
  method a tuple does not have — 38 wrong answers across the builtin types.
  `is_object_dunder_method` now takes the receiver's type name and gates each on
  the types CPython gives it to. A container's truth comes from `__len__`, so
  containers get no `__bool__` either; only `__str__`/`__repr__` stay universal.
  `dict_values` loses `__contains__` to match, and `v in d.values()` still works
  by iterating the view — which is exactly why CPython omits the method.
  CALLING an absent dunder now raises the same `AttributeError` that reading it
  does, instead of letting the native operation answer with its own complaint.
- **The "perhaps you missed a comma?" `SyntaxWarning` covered one of its twelve
  shapes.** Only a literal sequence subscripted by a FLOAT warned. Every non-int
  compile-time index warns now (`[1, 2]['a']`, `[None]`, `[b'x']`, `[1j]`,
  `[...]`, and the list/tuple/dict displays), while an `int`/`bool` index, a
  slice, a `dict`, and a bare NAME stay silent as in CPython. CALLING a literal
  — `None()`, `1(2)`, `[1, 2](3)` — did not warn at all and now does. `eval` and
  `exec` compiled their source and DROPPED the warnings entirely; they print them
  to stderr attributed to `<string>`, as CPython does.
- **The bytecode cache did not invalidate on a REBUILD.** The key hashed the
  source, a hand-bumped `SCHEMA`, and `CARGO_PKG_VERSION` — no term that a
  rebuild moves. Any build between two releases that changed lowering silently
  replayed the PREVIOUS build's bytecode out of `~/.pythonrs/scripts.rkyv`: no
  error, no wrong answer to chase, just "my fix did not take". Found when a
  compiler change emitting a new `SyntaxWarning` appeared to do nothing for every
  already-cached script on a binary rebuilt seconds earlier; the v49 `SCHEMA`
  note records the same class of bug biting once before. The key now also hashes
  the running executable's size and mtime.

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
- **`dir()` on a builtin type omits most of the inherited dunders.** Every
  builtin is missing the `object`-level names (`__delattr__`, `__dir__`,
  `__format__`, `__getattribute__`, `__getstate__`, `__init_subclass__`,
  `__reduce__`, `__reduce_ex__`, `__setattr__`, `__subclasshook__`) plus the
  comparison set and the container dunders it does implement — `dir('a')` is
  short by 24 names, `dir([1])` by 26, `dir(5)` by 15 (which also lacks
  `denominator`/`numerator`/`imag`/`real`/`from_bytes`). Attribute ACCESS is
  unaffected: the names that matter resolve, and `hasattr` agrees with CPython
  across the container dunders. What this costs is `dir()` itself and the
  "Did you mean" hint computed from it, so `'a'.__setitem__` reports the right
  `AttributeError` without CPython's `Did you mean: '__getitem__'?` clause.

- **The depth guards are calibrated for the interpreter's 512 MB stack, not for
  an embedder's.** `src/main.rs` runs the interpreter on a thread with
  `stack_size(512 * 1024 * 1024)`, and `parser::MAX_TREE_DEPTH` is chosen against
  that. pythonrs descends roughly fifteen parser frames per nesting level, so
  `pythonrs::eval_str` called from an ordinary 2 MB thread overflows well below
  the cap — libtest's worker cannot hold even the 200 bracket levels CPython
  accepts, which is why `deeply_nested_source_raises_instead_of_overflowing_the_stack`
  spawns a matching thread. Lowering the cap to fit 2 MB would reject source
  CPython accepts; making the levels cheaper is the real fix.
- **The stage that runs out of parser stack is not reproduced.** CPython reports
  `MemoryError: Parser stack overflowed …` when its PEG parser is what
  overflows and `RecursionError: Stack overflow (used N kB) during compilation`
  when the parse succeeded and the compiler is what overflows —
  `'-'*100000+'1'` is the first, `'a'+'.b'*100000` and `'1'+'+1'*200000` are the
  second. pythonrs's cap lives entirely in the parser, so all of them report the
  `MemoryError` form. Both are catchable, which is the property that was missing;
  the class split is not. Relatedly, pythonrs is MORE permissive than CPython on
  two shapes it accepts up to the cap: `'lambda: '*5000+'1'` and
  `'not '*20000+'1'` parse here and are `MemoryError` there.
- **`AttributeError.obj` is absent.** `.name` is bound (see above), but the
  object the failed lookup ran against is not recoverable from the rendered
  message that `synth_exc` reconstructs the exception from, and fabricating one
  would be worse than its absence. CPython answers `1` for `(1).nope`.
- **`UnicodeDecodeError`/`UnicodeEncodeError` carry the rendered message, not the
  five-tuple.** CPython's `args` is `(encoding, object, start, end, reason)` —
  `('utf-8', b'\xff', 0, 1, 'invalid start byte')` — with `.encoding`,
  `.object`, `.start`, `.end` and `.reason` reading back from it; pythonrs has
  `args == (<the whole message>,)` and none of the five attributes. Unlike the
  `OSError` case, this one cannot be fixed by parsing the message: the `object`
  is the offending `bytes`/`str` itself and the message only shows one byte of
  it. Closing it means carrying the structured arguments from the codec raise
  sites in `src/stdlib/codecs.rs` through to the exception object, and teaching
  `exc_message` to render CPython's text back from them.
- **A bridged exception's type has a two-element MRO.** `struct.error` and
  `binascii.Error` report `__module__ == 'builtins'` and
  `type(e).__mro__ == (error, object)`, where CPython says `struct.error` /
  `binascii.Error` and `(error, Exception, BaseException, object)` /
  `(Error, ValueError, Exception, BaseException)`. Catching is unaffected —
  `except struct.error`, `except binascii.Error` and `except ValueError` all
  match, because handler matching walks the base names captured at raise time
  rather than the type object — but the traceback's final line reads `error:` /
  `Error:` rather than `struct.error:` / `binascii.Error:`, and code that reads
  `__mro__` or `__module__` off a caught exception sees the wrong thing.
  Separately, `re.error` is not a class at all here (it answers a
  `builtin_function_or_method`, so `re.error.__mro__` raises), where CPython
  3.14.6 answers a class named `PatternError` in module `re`.
- **`int(str)` has no digit limit and `sys.set_int_max_str_digits` is absent.**
  CPython 3.14.6 caps a decimal `int()` conversion at 4300 digits
  (`int('9'*100000)` is `ValueError: Exceeds the limit (4300 digits) for integer
  string conversion: value has 100000 digits; use sys.set_int_max_str_digits() to
  increase the limit`) and exposes
  `sys.set_int_max_str_digits`/`get_int_max_str_digits` to change it;
  `int('9'*100000)` succeeds here. pythonrs is more permissive, so nothing that
  works under CPython breaks — but a program relying on the guard does not get
  it.
- **A binary operator's caret anchor stops at the operator.** When the right
  operand is PARENTHESIZED, CPython's anchor runs from the left operand's end to
  the right operand's own `col_offset`, which is INSIDE the parens: `1+("a")`
  underlines `~^^~~~~` and pythonrs underlines `~^~~~~~`. Unparenthesized
  operands agree. Only the caret row differs; the message, the line and the span
  are the same.
- **`stdout` is never block-buffered, so a merged stream interleaves
  differently.** CPython line-buffers `stdout` on a TTY and BLOCK-buffers it on
  a pipe or file, while `stderr` stays unbuffered; pythonrs flushes `stdout` on
  every write. Redirected output therefore comes out in a different order:

      import sys
      print("out1"); sys.stderr.write("err1\n")
      print("out2"); sys.stderr.write("err2\n")

      $ python3 prog.py > log 2>&1   ->  err1 err2 out1 out2
      $ python  prog.py > log 2>&1   ->  out1 err1 out2 err2

  The same difference makes pythonrs KEEP output CPython drops:
  `print("kept", end=""); os._exit(0)` prints `kept` here and nothing under
  CPython, because `os._exit` skips the flush of a buffer pythonrs does not
  have. `-u` cannot be observed on the pythonrs side for the same reason — the
  streams it would unbuffer are already unbuffered. Neither differential harness
  can see any of this: `dropin_check.sh` discards stderr and `parity-fuzz` reads
  the two streams through separate pipes, so the interleaving is never compared.
  Closing it means owning a real `BufWriter` for `stdout` with a TTY check, a
  flush at normal exit and before `input()`, and matching buffering on the
  embedded interpreter's side (`ffi.rs::line_buffer_std_streams` currently
  line-buffers CPython's streams specifically to match the unbuffered behaviour
  described here).
- **A user exception raised out of a wrapped generator loses its identity, not
  its class.** `PyrsIterator` now implements the generator protocol
  (`send`/`throw`/`close` beside `__iter__`/`__next__`), so
  `@contextlib.contextmanager` drives a pythonrs generator: `__exit__` throws the
  body's exception in, a `try/except` around the `yield` sees it, and
  `StopIteration` comes back out as "handled". An exception the generator body
  re-raises unchanged, however, crosses as a NEW CPython object of the same class
  and args rather than the very object `__exit__` threw in — `contextlib`
  branches on `exc is not value`, so it re-raises instead of returning False. The
  message, class and args a caller sees are identical; the traceback is one frame
  shorter. Closing it means carrying the CPython object's identity through the
  pythonrs exception value rather than rebuilding from class + args. Reachable
  from `parity-fuzz --mode ctxmgr` and `--mode stdlibexc`.
- **`-O` reaches `__debug__` and nothing else.** The flag sets `__debug__` to
  False, but the compiler still emits every `assert` (so an optimized run keeps
  checking them) and `sys.flags.optimize` still reports 0. Skipping asserts at
  compile time means threading the level into the bytecode CACHE KEY as well —
  otherwise a chunk compiled under `-O` would be reused without it — so it is
  deliberately not bolted on to the flag alone.
- **`__slots__` installs no member descriptors.** The slot RESTRICTION is
  enforced (a non-slot attribute is the CPython `AttributeError`), but the
  names are absent from the class: `class A: __slots__ = ('x',)` then `A.x` is
  `AttributeError: type object 'A' has no attribute 'x'` where CPython answers
  `<member 'x' of 'A' objects>`. Reaching a slotted `__doc__` through the class
  reports `None` for the same reason.
- **`types.UnionType is type(int | str)` is False on the ffi build.** In the
  self-contained build `types.py` binds `type(int | str)` and the identity
  holds. Under `stdlib-ffi`, `types.UnionType` and `typing.Union` are both
  CPython's own object (identical to each other) while `type(int | str)` stays
  native, so the union type has two representations that never compare `is`,
  even though the name, module, repr and messages all match. This is the general
  cross-bridge type-identity boundary, not specific to `Union`.
- **PEP 649 forward-ref annotations do not raise.** `def g(x) -> NotYet: ...`
  then `g.__annotations__` yields `{}`; CPython 3.14 evaluates the annotation
  lazily on that read and raises `NameError: name 'NotYet' is not defined`.
  Class bodies drop the unresolvable entry the same way.
- **Vendored `ast` omits optional-field defaults from `repr`.** In the
  self-contained build `repr(ast.Constant(1))` is `Constant(value=1)`; CPython
  says `Constant(value=1, kind=None)`, because every OPTIONAL ASDL field carries
  a class-level `None` default that `repr` then reads. `_fields` already lists
  `kind`; what is missing is the optional/required split from `Python.asdl`.
- **`compile()` is absent** — `NameError: name 'compile' is not defined`. The
  `-c`/file paths compile internally, but the builtin that exposes it (and so
  `code`-object construction from source, `exec(compile(...))`, and
  `dis`-over-source) is not wired up.
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
- **f-string / `str.format` format spec** is complete for the builtin types.
  Every presentation type (`b c d e E f F g G n o s x X %` and the omitted one),
  every flag (fill/align/sign/`#`/`0`/width/`,`/`_`/`.prec`), and nested field
  specs are covered — measured by sweeping 4 800 generated specs against all of
  `int`/`bignum`/`bool`/`float`/`-0.0`/`inf`/`nan`/`str` (91 206 pairs) under
  `LC_ALL` in `C`, `en_US`, `de_DE`, `hi_IN` and `fr_FR`, byte-identical to
  CPython 3.14.6 in every one.
- **Lone surrogates in `str`**: `chr(0xD800..0xDFFF)` raises `ValueError` where
  CPython returns a surrogate-bearing `str` (which then fails only on UTF-8
  encode). pythonrs strings are Rust `String` (valid scalar values only), so a
  lone surrogate is unrepresentable without a surrogate-aware string type; the
  out-of-range and surrogate paths share CPython's `chr() arg not in
  range(0x110000)` message. `surrogateescape`/`surrogatepass` handlers are
  likewise not reachable for the same reason.
- **`math.lgamma`/`erf`/`erfc` differ from CPython in the last ULP.** Measured on
  3.14.7: `math.lgamma(5)` is `3.1780538303479444` there and
  `3.1780538303479458` here, `math.erf(1)` is `0.8427007929497148` there and
  `0.8427007929497149` here, `math.erfc(1)` is `0.15729920705028516` there and
  `0.15729920705028513` here. pythonrs calls the platform libm; CPython does
  NOT — `Modules/mathmodule.c` carries its own `m_lgamma` (a Lanczos-series
  implementation with its own coefficient table) and its own `m_erf`
  /`m_erfc` (`m_erf_series` below 1.5, `m_erfc_contfrac` above), precisely so the
  answer does not vary with the host's libm. Closing this means porting those
  three routines from `mathmodule.c` rather than adjusting a rounding mode; every
  other `math` function measured (`gamma`, `hypot`, `dist`, `fsum`, `sumprod`,
  `comb`, `perm`, `isqrt`, `lcm`, `ldexp`, `frexp`, `modf`, `nextafter`, `ulp`,
  `remainder`, `cbrt`, `exp2`, `expm1`, `log1p`, `factorial`, `isclose`) already
  matches bit-for-bit.
- **A traceback stops at the pythonrs frame; frames INSIDE a bridged stdlib
  module are not listed.** `textwrap.shorten('a b c', 4)` raises the right
  exception with the right message and the right caret line, but CPython's
  rendering also names the four `textwrap.py` frames between the call site and
  the `raise` (`shorten` → `fill` → `wrap` → `_wrap_chunks`) and pythonrs's does
  not. The exception object and its type are correct; only the intermediate
  frames of the CPython-side call stack are missing, because the bridge returns
  the error without walking the foreign traceback. This is the same boundary the
  `During handling of the above exception…` chained section sits behind.
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
  `dir(list)` is 48 entries because every slot wrapper (`__add__`, `__iadd__`,
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
  they are not enumerable. pythonrs reports 459 of CPython's 781 names across
  the 13 builtin types (`int float bool str bytes bytearray list tuple dict set
  frozenset complex type`), measured by intersecting `dir(t)` per type against
  CPython 3.14.6. The BINARY OPERATOR slots are now real bound methods (see
  "Implemented"), which is what moved the count up.
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

### What the parity harnesses cannot report

Every number this project quotes comes out of one of four measuring tools, and
each is blind to a definite class of divergence. A gap in this table is not a
gap that has been ruled out — it is one no amount of running the tool can
surface, so it has to be found by reading code or by writing a new probe.

| Harness | Compares | Structurally cannot report |
| --- | --- | --- |
| `scripts/dropin_check.sh` | stdout bytes + exit code of whole scripts | stderr (discarded); any script the reference exits non-zero on (SKIPped, so the whole nonzero-exit surface); stdin-reading scripts (none supplied); argv shapes other than the one fixed triple; files the script wrote; stdout/stderr INTERLEAVING (separate pipes); timing |
| `src/bin/parity.rs` | stdout of the `examples/` corpus | stderr; the corpus scripts' own exit codes (not compared at all); everything the corpus does not happen to do; no frozen replay, so a machine without `python3` measures nothing — but it now says so and exits 2 rather than reporting success (see below) |
| `src/bin/parity_fuzz.rs` | stdout bytes + zero/non-zero exit of `-c` one-liners | the exact exit CODE (only success-ness); stderr unless `--stderr`, and then only a normalized last line; anything a generator does not emit — no filesystem, no subprocess, no threads, no stdin, no argv, no multi-file import, no `__main__` semantics; a case whose oracle output is nondeterministic is reported as a permanent gap rather than rejected |
| in-process `g()` (`tests/*.rs`) | one global's `repr` after `eval_str` | stdout entirely (`print` is invisible); stderr; the exit code; ordering between statements; **and it is not differential at all** — it compares against a value a human transcribed from CPython, so it catches a REGRESSION and can never catch a divergence that was wrong from the first commit |

A harness that reports success having measured nothing is worse than no harness,
and `src/bin/parity.rs` had four ways to do it: no `examples/` directory (it
printed a note and returned), an `examples/` with no `.py` files (the loop ran
zero times), no `python3` on PATH (every file printed `skip` and the summary read
`0 passed, 0 failed`), and — the sharpest — an actual divergence, since `fail >
0` still fell off the end of `main`. All four exited 0, so a caller reading the
status could not tell a clean sweep from a total mismatch. It now exits 1 on a
divergence, 2 on a run it cannot measure, and prints how many scripts it actually
compared. `scripts/dropin_check.sh` already refused an empty corpus and a missing
reference; the two agree now.

Two axes were pinned to a constant across every one of them, which hid the axis
rather than controlling it:

* **`PYTHONHASHSEED`** was frozen at `0` by both subprocess harnesses, and
  pythonrs ignored the variable entirely — so the fuzzer could not have detected
  that any other seed returned the seed-0 value. Closed: the seed is honoured
  (see the `hash()` section) and `parity-fuzz` now sweeps it, pinning the same
  value on both children per case.
* **`LC_ALL`** was pinned nowhere, which is the opposite failure — every run
  measured whatever locale the operator's shell had. That is what let
  `format(n, 'n')` ship with no locale grouping at all: on a `C`-locale machine
  it is indistinguishable from `d`. Closed: `dropin_check.sh` pins `LC_ALL=C` so
  a run is reproducible, and the locale-VARYING surface is measured by sweeping
  `LC_ALL` over the format-spec corpus against `python3`.

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
  foreign exception also keeps what its rendered `Class: message` line cannot
  carry: its real `args` and any instance attributes outside them are recorded at
  raise time (`host::ForeignExc`) and restored on the pythonrs side, so
  `os.environ['missing'].args` is the KEY rather than the key's repr (`KeyError.
  __str__` is `repr(args[0])`, so re-parsing the rendering doubled the quotes)
  and `except json.JSONDecodeError as e: e.lineno` reaches the real position.
  A `@dataclass` instance also matches a `match` class pattern (positional via
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
  `type(...).__name__` is `list_iterator` where CPython says `callable_iterator`,
  and a scan over a huge subject materializes all of it.
- **`re.Scanner` and `re.RegexFlag` are absent** (the flag constants exist as
  plain ints; `re.A`/`re.I`/`re.M`/`re.S`/`re.X` all resolve).

### `hash()` values: what is reproduced, and what cannot be

`hash(x)` now returns CPython's own number. The algorithms are ported from the
CPython 3.14.6 C sources in `src/pyhash.rs` (`long_hash`, `_Py_HashDouble`,
`complex_hash`, `Py_HashBuffer`/`siphash13`, `tuple_hash`, `frozenset_hash`),
and the cross-bridge container collapse that follows from them works in both
directions:

```
len({1, Decimal(1)})            # 1        len({0.5, Fraction(1, 2)})   # 1
{1: 'int'}  | d[Decimal(1)]='dec'  ->  {1: 'dec'}
{Decimal(1): 'dec'} | e[1]='int'   ->  {Decimal('1'): 'int'}
```

`PYTHONHASHSEED` is honoured, not ignored. `_Py_HashRandomization_Init`
(`Python/bootstrap_hash.c`) is ported: seed `0` zeroes the 24-byte secret, any
other pinned seed expands through `lcg_urandom`, and an unset variable — or
`random` — draws per-process entropy exactly as CPython does. `hash('abc')` is
therefore byte-identical to `PYTHONHASHSEED=N python3` for every `N` in
`[0, 4294967295]`, where before this only `N == 0` agreed and every other seed
silently returned the seed-0 value. A seed CPython refuses (`0x10`, `-1`,
`4294967296`, a trailing space) is refused here with CPython's own
`Fatal Python error: config_init_hash_seed: …` text and exit code 1.

One residue remains, and it is a boundary rather than a gap:

- **Address-derived hashes are not reproducible by anyone.**
  `hash(float('nan'))`, `hash(...)`, `hash(NotImplemented)` and an instance's
  default identity hash come from `PyObject_GenericHash`, i.e. the object's
  address. Measured across CPython runs they differ every time *even under
  `PYTHONHASHSEED=0`*, so there is no value to match. pythonrs returns a stable
  internally-consistent number instead.

An UNSET seed is likewise unmatchable in principle — both interpreters draw
their own entropy — which is a property of asking for unpredictability, not a
divergence. `parity-fuzz` pins the same seed on both children and sweeps it
across cases rather than freezing it at `0`, so the whole seed axis is measured;
it was previously frozen, which made a hash-seed divergence structurally
unreportable.

A `__hash__` RESULT is not reduced modulo `2**61-1`. CPython's `slot_tp_hash`
tries `PyLong_AsSsize_t` first and uses any value that already fits a
`Py_hash_t` verbatim — `__hash__` returning `2**62` hashes to `2**62`, not `2` —
falling back to `long.__hash__` only on overflow. Reducing unconditionally would
rewrite every large in-range hash a user returns.

Reachable from `parity-fuzz --mode hashval`, which prints RAW hash values. The
six older `hash(` sites only compare `hash(x) == hash(y)`, a shape any
self-consistent hash satisfies — which is why a hash that matched CPython for no
type at all went unnoticed.

### `set` iteration order diverges for a set DISPLAY

`setobject.c`'s open-addressing table is ported (`host.rs`, `SetTable`) and
reproduces the order for `set(iterable)`, `.add()` in a loop, and `frozenset`.
A set **literal** still diverges:

```
{1, 2, 3, 10, 20}           # CPython {1, 2, 3, 20, 10}, pythonrs {1, 2, 3, 10, 20}
{100,200,300,400,500,600}   # CPython [400, 100, 500, 200, 600, 300]
                            # pythonrs [100, 200, 300, 400, 500, 600]
```

The cause is a table SIZE difference, not a hash difference. A literal compiles
to `BUILD_SET 0` + `LOAD_CONST frozenset({...})` + `SET_UPDATE`, and
`set_update_internal`'s set-to-set path presizes with
`set_table_resize(so, (so->used + other->used)*2)` — for five elements that is
`minused = 10`, giving a 16-slot table (`mask` 15). Inserting the same five
elements one at a time never presizes: the table starts at 8 and grows to 32 on
the fifth insert (`mask` 31). A 16-slot table reproduces both diverging cases
above exactly, including the `LINEAR_PROBES` runs that place `500` and `600`.
Closing this needs the literal's presize modelled, not a change to hashing.

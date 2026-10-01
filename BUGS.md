# pythonrs — known gaps and unimplemented behavior

pythonrs is Python lowered to fusevm (bytecode VM + Cranelift JIT), with a PyHost
object heap. It runs a large, real subset of Python 3 correctly (verified
byte-for-byte against CPython 3.14.6 on the example corpus). This file is the
honest list of what is **not** yet covered, so nobody mistakes a gap for a bug
fixed. Every line below was re-checked against the **default-build** binary
(`cargo build` — default features, so the `stdlib-ffi` bridge is ON) before being
written.

## Implemented (previously listed here as gaps)
- **A `--build` binary runs, and reports an error the VM raised itself.** The
  runner deserialized fusevm's embedded chunk without stripping its format tag
  (`AOT_CHUNK_MAGIC`), so every built binary failed with `corrupt embedded
  chunk` or hung allocating; and an error a native fast-path op raised (`int +
  str`) sat in fusevm's run result rather than on the host, so the binary
  exited 0 with no traceback. It now strips the tag (refusing a binary built by
  another fusevm, as fusevm's own runner does), takes the result
  (`VM::take_aot_result`), records the failing op's line and caret as the
  interpreter does, and prints the traceback with exit status 1.
- **A failing unary operator names its line.** `-x`, `+x` and `~x` were
  emitted with line 0, so `z = -"s"` reported `File "<string>", line 0, in
  <module>` with no source line or caret; they now carry their statement's
  line as binary operators do.
- **Deep source is measured as CPython measures it.** pythonrs bounded every
  shape with one tree-depth cap (`MAX_TREE_DEPTH`, 20 000) calibrated for the
  512 MB interpreter thread, reported everything as the parser's `MemoryError`,
  accepted `'lambda: '*5000+'1'` and `'not '*20000+'1'` that CPython refuses,
  refused `'a'+'.b'*20000` that CPython compiles, and aborted an embedder's
  2 MB thread on 50 brackets. Now the parser counts pegen's rule levels
  (`MAXSTACK`, 6000) as `Parser/parser.c` does — one per `-`, `not` and
  `else`, two per `lambda` and `**`, and the levels each context adds — so
  `'-'*5968+'1'` parses and `'-'*5969+'1'` raises `MemoryError: Parser stack
  overflowed`, across 33 statement contexts and 47 shapes measured against
  CPython 3.14.8. A left-recursive chain grows in a loop, as pegen's does, and
  fails in the compiler instead: every walk checks the native stack the way
  `_Py_EnterRecursiveCall(" during compilation")` does (`src/stack.rs`, after
  `Python/ceval.c`'s `hardware_stack_limits`/`tstate_set_stack`) and raises
  `RecursionError: Stack overflow (used N kB) during compilation`, the class
  CPython raises for `'a'+'.b'*100000`; the AST drops iteratively so the failed
  tree cannot overflow on the way out. The parser and the compiler run on a
  512 MB stack of their own when the caller's has less than 64 MB to spare,
  so `eval_str` on libtest's 2 MB worker runs the 200 brackets CPython
  allows, `compile` on a 256 KB thread accepts and refuses what CPython does,
  and neither aborts; a generator's coroutine stack is measured as its own.
- **A starred display item is a `bitwise_or`.** `[*a if b else c]`, `x = *a or
  b,`, `{*a or b}` and `[*not a]` compiled and ran; `star_named_expression`
  and `star_expression` take `'*' bitwise_or`, so CPython refuses them with
  `invalid syntax` at the operator, and so does pythonrs now. A call argument
  still takes `'*' expression`.
- **Annotation scopes in a class body see the class namespace.** A class
  annotation (`class K: T = int; x: T`), an annotated method's
  `__annotate__` (`def m(self, a: T) -> T`) and a `type` alias value were
  ordinary nested functions, which skip the class scope, so each raised
  `NameError` (the class annotation was silently dropped). They are now
  PEP 649/695 annotation scopes: built with `FuncDef::sees_class_scope`, they
  capture the class body's own environment — CPython's `__classdict__` — and
  read it before the enclosing function and module scopes; a lazily evaluated
  one sees the namespace as the body left it. An ordinary method still does not
  see class names.
- **A `__slots__` member keeps its value beside the instance dict.** Slot
  values were stored in the instance `__dict__`, so a slotted base under an
  unslotted subclass leaked them into `vars(c)` (`['a', 'z']` where CPython has
  `['z']`), and a dict entry of the same name shadowed the slot. Each
  instance's slot values now live in their own storage, reached when the name
  resolves to the class's `member_descriptor` (the first class along the MRO
  that declares the slot, unless an earlier one binds the name): the member
  answers before the instance dict as a data descriptor does, an empty slot
  raises on read and `del` raises `member_set`'s bare `AttributeError(name)`.
  `__getstate__` and `__reduce_ex__` follow `object_getstate_default` —
  `(dict_or_None, {slot: value})` in `copyreg._slotnames` order, with
  `__slotnames__` cached on the class, and the state taken from a class's own
  `__getstate__` when it has one — protocol 0/1 refuses a slotted class without
  `__getstate__` as `copyreg._reduce_ex` does, and `copy.copy` / `deepcopy`
  carry the slots. The per-class slot layout is memoized, which also takes the
  `__slots__` restriction check off the per-write MRO walk: an attribute
  read/write loop retires 5% fewer instructions than before.
- **An exception keeps its identity, type and CPython frames across the
  bridge.** `host::ExcBridge` pairs every exception that crosses with its
  CPython counterpart, both ways. A pythonrs exception sent to CPython is the
  same CPython object every time — a builtin class's own, or for a user or
  native-module class an instance of a mirror class (subclass of its nearest
  builtin exception ancestor, its own name/module, `str`/`repr`/attributes
  answered by the pythonrs exception) — and comes back as the pythonrs
  original. So `@contextlib.contextmanager` sees `exc is value` and declines
  (the body's exception propagated as a new object, and a user exception did
  not cross at all: `exceptions must derive from BaseException`), and an
  exception a callback raises inside CPython code is caught as itself
  (`json.dumps(default=f)` turned `MyErr` into `RuntimeError: MyErr: bad`).
  An exception CPython raised has CPython's class as `type(e)`/`e.__class__`
  (`json.JSONDecodeError`, not a `<built-in function>` with a two-element
  `__mro__`), its uncaught last line names it as CPython does
  (`struct.error:`, not `error:`), and its traceback continues with the
  frames it passed through inside CPython (`textwrap.shorten` → `fill` →
  `wrap` → `_wrap_chunks`, rendered by CPython's `traceback.format_tb`).
  `raise` of a CPython exception or exception class works. Two native bugs
  this exposed are fixed with it: a bare `raise` re-raised a caught user
  exception as `StopIteration` (`b_reraise` rendered every non-builtin value
  that way), and handing an error to CPython cleared the exception the
  calling handler was still handling. `re.PatternError` is a real class
  (`__mro__`, `isinstance`) whose constructor is `re/_constants.py`'s
  (`msg`/`pattern`/`pos`/`lineno`/`colno`), and an exception reprs by its
  type's short name.
- **A pythonrs object pickles.** `pickle` is CPython's C pickler, and a pythonrs
  instance reached it as an opaque `PyrsInstance` (`TypeError: cannot pickle
  'builtins.PyrsInstance' object`), a class or function as a fresh wrapper on
  every crossing (so `__main__.P` could never be found again as the same
  object), and CPython's `__main__` was the embedded interpreter's empty module.
  Five pieces close it. A native class crosses as ONE cached mirror per class,
  and a callable or instance as one cached proxy, so identity holds across
  crossings; a mirror's metaclass constructs the native class when called and
  detaches it from the native class the moment CPython code changes it (as
  `@dataclass` does, whose result stays the CPython class it was). An instance
  proxy's `__class__` is the mirror and its `__reduce_ex__`/`__reduce__` are the
  native object's. CPython's `__main__` answers a missing name (PEP 562
  `__getattr__`) from the program's own namespace. And a mirror, or an
  instance CPython made of one with `object.__new__` (`copyreg.__newobj__`,
  `copyreg._reconstructor`), crosses back as the native class / a native
  instance carrying its attributes — one object for the life of the process, so
  a class's `__setstate__` running mid-load and the finished object agree. The
  pickle bytes are CPython's at every protocol, `pickle.loads` gives `type(p) is
  P`, a class or function round-trips as the very object, and a bound method
  pickles as `getattr(obj, name)`. `object.__reduce_ex__` on a user instance is
  now CPython's: a class's own `__reduce__`, `__getstate__`, `__getnewargs__` and
  `__getnewargs_ex__` (`copyreg.__newobj_ex__`) are honoured, a builtin
  subclass reduces to its own class (below protocol 2 with the builtin base and
  its value), a `list`/`dict` subclass carries its items and a `set` subclass
  uses `set_reduce`; it used to ignore all of these. Slot values loaded back
  land in the native class's slot storage.
  A `bytearray` reduces with `bytes` from protocol 3 (not 5) and with no
  arguments when empty, as `bytearray.__reduce_ex__` does.
- **A native class named in an annotation is that class again.** A class
  crossed as a fresh mirror every time and came back as a `Foreign` handle, so
  `dataclasses.fields(F)[0].type is E` and `typing.get_type_hints(N)['e'] is E`
  were `False`; with the class crossing as one cached mirror that crosses back
  as the native class, both hold, inside a generic alias too.
- **Self-referential and shared containers cross the bridge as a graph.** Every
  conversion was a tree walk, so a list containing itself recursed until the
  native stack was gone and the process ABORTED (`json.dumps(l)`,
  `pickle.dumps(l)`, even `operator.is_(l, l)` through the argument write-back),
  and a list reached twice crossed as two objects (`pickle.loads(pickle.dumps([x,
  x]))` lost the sharing). One marshal now memoizes the containers it has
  converted, registering a list or dict before its elements, in both
  directions and across all the arguments of one call.
- **`import __main__` is the program's own module.** It resolved over the
  bridge to the embedded interpreter's empty `__main__`, so
  `__import__('__main__').X` raised `AttributeError`; it is now module slot 0,
  present in `sys.modules` from the start, and a module without a spec reprs as
  `importlib._bootstrap._module_repr` does (`<module '__main__' from
  'x.py'>`). A module imported from a relative `sys.path` entry (`''` under
  `-c`) carries an absolute `__file__`, as `FileFinder` makes it.
- **A user exception class inherits `add_note` and `with_traceback`.**
  `BaseException`'s methods resolved only on the builtin exception types, so
  `class E(Exception)` raised `AttributeError: 'E' object has no attribute
  'add_note'` — including inside `pickle.py`, whose `save` notes every
  failure. Both now resolve on any exception instance, as a call and as a bound
  method, and `add_note` is `BaseException_add_note_impl`: it appends to
  `__notes__` in place (the list keeps its identity, where it used to be
  replaced), rejects a non-`str` note with `add_note() argument must be str,
  not int`, and refuses a `__notes__` that is not a list.
- **A bound `object` slot read off an instance is callable.** `obj.__reduce_ex__`
  / `obj.__eq__` / `obj.__setattr__` and the other inherited `object` slots
  read as an attribute and called later — `r = getattr(obj,
  '__reduce_ex__'); r(4)`, which is how `copy` and `pickle` reach them —
  raised `TypeError: 'method-wrapper' object is not callable`. The bound
  wrapper now runs the slot on the instance it was read from.
- **`operator.index()` sees a native `__index__`.** The `operator` module is
  the bridged C accelerator, and a pythonrs instance crossed as a
  `PyrsInstance` proxy with no `nb_index` slot, so `operator.index(obj)` and
  `operator.getitem(seq, obj)` raised `TypeError: 'builtins.PyrsInstance'
  object cannot be interpreted as an integer`. An instance whose class defines
  `__index__` now crosses as `PyrsIndexInstance`, a proxy subclass that fills
  the slot, runs the method on the fusevm side and checks its result as
  `PyNumber_Index` does. Every other instance keeps the slotless proxy, since
  CPython probes that slot (`PyIndex_Check`) to choose a path — `bytes(x)`
  takes a length from an index-able `x`.
- **"Did you mean" for a misspelled keyword on a `SyntaxError`.** 3.14's
  `traceback` (`TracebackException._find_keyword_typos`) turns a bare
  `invalid syntax` (or `Perhaps you forgot a comma`) into `invalid syntax.
  Did you mean 'while'?`, carets on the misspelled name, when replacing one
  of the first ten non-keyword names before the error with a keyword it
  resembles makes the source compile or merely incomplete; pythonrs never
  did. `suggest::keyword_typo` ports it, with `difflib.get_close_matches`
  (`SequenceMatcher` with autojunk), `textwrap.dedent`, the `_suggestions`
  distance already used for `NameError`, and `codeop`'s acceptance test —
  a full compile, or `parser::is_incomplete_input`, pegen's
  `_IncompleteInputError` (the parse fails having reached the end of the
  source). It runs for a program that does not compile, for an uncaught
  `SyntaxError` object, and — because the parser now records `_metadata`
  as `(0, 0, source)` (exec input newline-translated and newline-terminated,
  eval input as given), it crosses the bridge, and `SyntaxError(msg,
  details)` keeps a seventh details item as `_metadata` instead of dropping
  it — for CPython's own `traceback.format_exception_only`.
- **A line continuation at the end of input, or followed by more of its
  line.** `x = 1 + \` with nothing after it was `invalid syntax` at the `\`;
  CPython's tokenizer reports `unexpected EOF while parsing` just past it
  (end offset -1), or the open bracket's `was never closed` inside one. A
  `\` followed by anything but the line break is the tokenizer's `unexpected
  character after line continuation character` (pythonrs: `invalid
  syntax`).
- **A bracket left open at the end of an indented block is `never closed`.**
  `def f():\n  f(1,` reported `invalid syntax` at the last token, because
  the lexer's closing DEDENTs hid that the parser had reached the end of the
  input; the end is now the whole closing run of NEWLINE/DEDENT/EOF tokens.
- **A memoryview's hash is cached.** `memory_hash` stores the number on the
  view and reads it before the released check, so a view hashed while live keeps
  hashing — and keeps finding itself as a dict key — after `release()`.
  pythonrs recomputed it from the bytes each time and raised `ValueError:
  operation forbidden on released memoryview object` for it. The view now
  remembers that it was hashed; a view first hashed after release still raises.
- **The compiler's pattern-matching `SyntaxError`s are positioned.**
  `name capture 'a' makes remaining patterns unreachable`, `wildcard makes
  remaining patterns unreachable`, `multiple assignments to name 'a' in
  pattern`, `mapping pattern checks duplicate key`, `attribute name repeated
  in class pattern` and `alternative patterns bind different names` had
  `lineno`/`offset`/`end_lineno`/`end_offset` of `None`, so a program that
  raised one printed no `File` line position or caret, and one raised inside
  `exec`/`eval` lost its inner `File "<string>", line N` block. Every
  `Pattern` now carries CPython's AST `Loc` (UTF-8 byte columns), and each
  error is raised at the node `codegen_pattern_*` hands `_PyCompile_Error`:
  the irrefutable capture or wildcard, the re-binding node (the whole mapping
  for `**rest`), the whole mapping for a duplicate key, the repeated keyword's
  sub-pattern, the whole or-pattern. `args` is `(msg, (filename, lineno,
  offset, None, end_lineno, end_offset))`, as `_PyErr_RaiseSyntaxError`
  builds it, and the offsets are bytes plus one (`case ('éé', a, a)` is 19).
- **Zero-arg `super()` works in a class CPython built.** A class with a foreign
  base (`class C(abc.ABC)`, an `enum.Enum` subclass) is built by CPython's
  metaclass and has no native `ClassDef`, and its methods are called through
  CPython, so `super()` in one raised `RuntimeError: super(): no arguments` —
  every `abc.ABC` hierarchy that chains `__init__` broke on it. `type.__new__`
  fills the body's `__class__` cell with the class it built; pythonrs now does
  the same by tagging each function defined in the body (and the function
  inside a `classmethod`/`staticmethod`) with the built class
  (`PyHost::foreign_class_cells`), and `super()` there is CPython's own
  `super(cls, inst)`, as is an explicit `super(Cls, obj)` against such a class.
  Its instance is the frame's first argument, which is CPython's rule
  (`super_init_without_args` reads `localsplus[0]`), so a native method called
  with its receiver as a plain argument — `Kid.g(obj)`, `map(Kid.g, objs)` —
  resolves too. A `classmethod`/`staticmethod` stored in a CPython-built class
  binds as `classmethod.__get__`/`staticmethod.__get__` do (the owner, or
  nothing), where both used to bind the instance like a plain function.
- **A method call resolves its attribute callee before its arguments.**
  `obj.m(g())` ran `g()` before looking `m` up, so a `__getattr__`, a property
  or a failed lookup acted after the arguments (`['arg', 'callee', 'call']`
  where CPython logs `['callee', 'arg', 'call']`), and the miss carets the call
  (`~~~~~~~~~^^`) where CPython carets the attribute (`^^^^^^^^^`). The new
  `LOAD_METHOD` op is CPython's `LOAD_ATTR` method form: it resolves the
  callee and leaves `[self_or_none, name, callee_or_none]` for `CALL_LOADED` /
  `CALL_LOADED_KW` / `CALL_LOADED_EX`. A function found on an instance's class
  travels as `[recv, name, func]` (no bound-method allocation), so an argument
  that rebinds the attribute does not change what is called; a method the
  receiver's type answers natively is left for the call to dispatch by name;
  anything that runs user code goes through the full attribute protocol. A
  call whose arguments are all inert — literals and frame-slot locals every
  path has assigned — cannot observe the order and keeps the fused
  `CALL_METHOD`, which now resolves instance, class and `super` receivers the
  same way, so it too calls what a `__getattribute__` override, a property or a
  user descriptor produces rather than the descriptor object (`P().m()` with a
  property `m` raised `'property' object is not callable`). On a debug build,
  instructions retired against the previous binary: a module-level
  `xs.append(i)` (a global argument, so loaded) +11.9%, a user method call
  there +2.6%, `d.get("k")` +0.8%, and inside a function `xs.append(i)` +0.7%
  and `c.m(i)` −0.1%.
- **`--lsp` go-to-definition and signature help.** The server answered only
  completion, hover and diagnostics. `textDocument/definition` now resolves the
  name under the cursor the way the compiler does — innermost function out,
  class bodies invisible from their methods, `global` names in the module — to
  the nearest binding above the cursor (`def`, `class`, parameter, assignment,
  `for`/`with`/`except` target, import, `match` capture), or the first one when
  all come later. `textDocument/signatureHelp` shows the parameters of the call
  being typed exactly as its `def` header writes them (a class shows `__init__`
  without `self`), with the active one picked by position, by `name=`, or as the
  `*args` collector; commas inside strings and nested calls are not counted, and
  a document left unparsable by the half-typed line is re-parsed without it.
- **PEP 695 `type X = ...` builds a `typing.TypeAliasType`.** The statement
  did not parse (`SyntaxError`). It is now the `type` soft keyword's
  `type_alias` rule: `TYPE_ALIAS` creates the alias with one `TypeVar`/
  `TypeVarTuple`/`ParamSpec` per type parameter, and the value — compiled as a
  function of those parameters — runs on the first `__value__` read and is
  cached, so an alias may be recursive or name something defined later (a
  missing name raises `NameError` at that read). `__name__`, `__module__`,
  `__type_params__`, `repr`, `X | Y`, `G[int]` (a `GenericAlias`; a
  non-generic alias raises `Only generic type aliases are subscriptable`),
  hashing and `not callable` follow CPython, in module, class and function
  bodies alike. A `TypeVar` now also joins a `|` union.
- **`for`/`with`/comprehension targets, `*` after `**`, undecodable literals
  and late positional class sub-patterns are positioned `SyntaxError`s.** An
  invalid `for` or comprehension target was the compiler's unpositioned
  `cannot assign to this expression`; the parser now ports
  `invalid_for_target` (re-reading the clause as `star_expressions`, so
  `for a + b in x` names the `expression`), `invalid_for_if_clause` (`'in'
  expected after for-loop variables`) and `invalid_with_item`, naming the part
  that cannot be assigned at its position, inside the parentheses of a group;
  a `for` with no `in` is `invalid syntax` at the next token rather than a
  pythonrs sentence. `f(**x, *y)` runs from the comma to the tokenizer's
  cursor as `RAISE_SYNTAX_ERROR_STARTING_FROM` does; a string or bytes literal
  that fails to decode spans the literal; `C(x=1, 2)` is `positional patterns
  follow keyword patterns` where it used to run; and a span that reaches a later
  line leaves `text` without its newline, as pegen does.
- **`dir()` of a builtin type is CPython's full listing.** `dir(int)`,
  `dir(str)`, `dir(list)`, `dir(dict)` and the rest of the 13 builtin types (and
  their values: `dir(5) == dir(int)`) name every slot wrapper, classmethod, data
  attribute and the inherited `object` surface — every name
  CPython 3.14 lists except `dir(type)`'s five C-layout numbers (see the open
  entry). `builtin_type_dir_is_cpythons_full_listing` pins four exact
  listings and eight counts.
- **A builtin type's classmethod reprs as a method bound to the type.**
  `repr(dict.fromkeys)`, `repr({}.fromkeys)`, `int.from_bytes`,
  `float.fromhex`, `str.maketrans` and `itertools.chain.from_iterable` are
  `<built-in method fromkeys of type object at 0x…>` with the type's own `id`,
  where they printed `<method 'fromkeys' of 'dict' objects>` (and
  `<built-in function itertools.chain.from_iterable>`). A slot reached through
  the type is a `slot wrapper` (`<slot wrapper '__add__' of 'int' objects>`),
  and `dict.__dict__['fromkeys']` reprs as CPython's `<method 'fromkeys' of
  'dict' objects>` rather than `<classmethod_descriptor …>`.
- **A module-level native function reprs by its bare name.**
  `repr(math.sqrt)` is `<built-in function sqrt>`, not
  `<built-in function math.sqrt>` — CPython's `meth_repr` prints `__name__`
  alone when `__self__` is a module.
- **`sys.getsizeof`.** A port of `sys_getsizeof`/`_PySys_GetSizeOf`:
  `type(o).__sizeof__(o)` (a class through its metaclass) plus
  `_PyType_PreHeaderSize` — 16 for a GC type, 32 for an instance with a managed
  `__dict__`, 0 for a scalar or a static type object — with CPython's
  `an integer is required`/`__sizeof__() should return >= 0` errors and the
  `default` returned only for a `TypeError`. A bridged CPython object is sized
  by CPython. The `__sizeof__` of a native builtin value is pythonrs's own
  footprint, so those totals are not CPython's; a user `__sizeof__` and the
  pre-header around it are exact.
- **`type()` of a C-level descriptor or iterator is a type object.**
  `type(C.x)` for a slot, `type(object.__init__)`, `type(str.upper)`
  (`method_descriptor`, was `builtin_function_or_method`),
  `type(dict.__dict__['fromkeys'])`, `type((1).__add__)` and every container
  iterator type (`list_iterator`, `dict_keyiterator`, …) repr as
  `<class '…'>`, are instances of `type`, and raise CPython's
  `cannot create '…' instances` when called. `isinstance(C.x, Exception)` was
  `True`. The same change made `type(zip)`, `type(property)` and the other
  builtin type objects' own type `type`.
- **`f.__annotate__` is the compiled annotate function.** It was a
  `functools.partial` around the def-time dict. The compiler now emits CPython's
  `def __annotate__(format, /)` — qualname `f.__annotate__`, `if format > 2:
  raise NotImplementedError`, a fresh dict per call — and `MKFUNC` keeps it as
  the function's attribute, so `type(f.__annotate__)` is `function`, it is the
  same object on every read, and format 3 (`FORWARDREF`) now raises as CPython's
  does instead of answering. The parameter is CPython's `.format`, so
  `def f(x: format)` still annotates with the builtin.
- **PEP 649 function annotations are evaluated lazily.** `MKFUNC` ran the
  annotations function at `def` time and swallowed a `NameError`, so
  `def g(x) -> NotYet` then `g.__annotations__` was `{}`. The compiled
  `__annotate__` is now called with `VALUE` on the first `__annotations__` read
  (`host::function_annotations`, outside the host borrow, also for a bound
  method and for CPython reading a bridged function): an unresolvable name
  raises its `NameError` on each read until it resolves, side effects happen at
  that read, and the result is cached. `__annotate__(1)` evaluates afresh, and
  assigning `__annotations__` makes `__annotate__` `None`, as in CPython.
- **`--dap` evaluates watch expressions.** `evaluate` only looked a bare name up
  in the paused frame and answered `<cannot evaluate …>` for anything else. It
  now runs the expression as `eval` on the stopped line would — the frame's
  locals over the module globals — and answers its `repr` (a user `__repr__`
  included), or a failed response carrying the exception line. Markers the
  evaluation runs through do not stop it or move the step bookkeeping, and a
  raise leaves no error, exception or traceback behind for the resumed program.
  A `--dap` compile also keeps every function local in the environment, so
  `variables` shows locals assigned after entry: only the parameters were
  visible, because the rest lived in VM frame slots.
- **`UnicodeDecodeError`/`UnicodeEncodeError`/`UnicodeTranslateError` carry
  their five-tuple.** `args` was the rendered message alone and none of
  `.encoding`/`.object`/`.start`/`.end`/`.reason` existed. `src/excunicode.rs`
  ports the three `_init`/`_str` pairs of `Objects/exceptions.c`: the
  constructor checks its arguments as `PyArg_ParseTuple` does (a decoder's
  bytes-like object is kept as `bytes`), the attributes read the tuple back, and
  `__str__` is rendered from the attributes, so reassigning one changes it. A
  native codec raise site (`str.encode`, `bytes.decode`, a text file's encoder,
  `_codecs`) records the tuple beside the error line for `synth_exc`, as
  `ForeignExc` does for the bridge, and one raised by CPython's codecs has the
  attributes bound from its recorded `args`. The utf-16 and utf-32 decoders now
  report CPython's positions and reasons (`illegal UTF-16 surrogate`,
  `unexpected end of data`, `code point not in range(0x110000)`, the surrogate
  range), and a text file's ascii/latin-1 write merges a run as `str.encode`
  does.
- **More than 255 operands anywhere.** `CallBuiltin` carries a `u8` operand
  count, so a call with >255 arguments, a `{**a, …}` display with >85 entries,
  a `def`/`lambda` with >252 defaults, >127 class-header keywords, and a class
  or mapping pattern with >252 keys raised `too many arguments (>255) for one
  call`. Each now gathers its operands the way CPython's `LIST_EXTEND`/
  `DICT_MERGE` do: an oversized call goes through the `*args`/`**kwargs` path,
  whose `BUILD_ARGS`/`BUILD_KWARGS` (and `MKDICT_EX`) take one chunk-built list
  in place of inline slots, and `MKFUNC`/`MATCH_CLASS` take their defaults and
  keyword names as lists. Plain collection literals and f-strings were already
  chunked.
- **`range` and a read-only `memoryview` are hashable.** Both raised
  `TypeError: unhashable type: 'object'`, so neither could be a dict key or set
  member. A range now keys as `range_hash`'s tuple — `(len, None, None)` when
  empty, `(len, start, None)` for one element, else `(len, start, step)` — so
  equal ranges (`range(0)`, `range(5, 5)`) are one key and hash to CPython's
  number, while staying a different key from that tuple. A read-only
  memoryview hashes and keys as the bytes it shows (`{b'ab',
  memoryview(b'ab')}` has one element), and a writable one raises
  `ValueError: cannot hash writable memoryview object`.
- **`set` iteration order for displays and set-to-set merges.**
  `{1, 2, 3, 10, 20}` printed `{1, 2, 3, 10, 20}` where CPython prints
  `{1, 2, 3, 20, 10}`: a constant display is `BUILD_SET 0; LOAD_CONST
  frozenset; SET_UPDATE`, and `set_update_internal` presizes the table in one
  step instead of growing it insert by insert. Each set now records the table
  size its order replays from, and the merges are ported — constant and
  starred displays (but not a display that is only iterated, which CPython
  loads as the folded frozenset), `set(s)`, `frozenset(s)`, `set(dict)`,
  `.copy()`, `|`, `|=`, `.update()`, and `&`/`&=`/`-`/`^`'s construction order.
  Floats, complex numbers and tuples of numbers join ints as reproducibly
  ordered, and `set.pop()` takes the first element in iteration order rather
  than the last inserted. A 400-case differential run over displays and merges
  matches CPython 3.14 on all 1,600 lines (1,298 differed before). Removal is
  what remains (see "`set` iteration order after an element is removed").
- **`float` `repr` breaks a shortest-digit tie to even.** Rust `std`'s shortest
  formatter rounds an exact tie between two equally short round-tripping
  decimals up, so `2113325745016023.2` (the double `…023.25`) printed as
  `…023.3`; CPython's dtoa takes the even last digit. `fmt_float` now checks,
  in exact integer arithmetic, whether the value lies exactly halfway to the
  even neighbour of Rust's odd last digit and takes it when it round-trips.
  Agrees with CPython 3.14 on 99,991 doubles: random bit patterns, 20,000
  constructed ties, and the scientific-notation boundaries.
- **`unexpected unindent`.** A block whose body is only a decorator (`try:` +
  `    @dataclass`, then a dedent or the end of input) reported an unpositioned
  `SyntaxError: invalid syntax`; pegen's generic failure on a DEDENT token is
  `IndentationError: unexpected unindent`, positioned at the next statement's
  indentation or just past the end of the last line. A decorator followed
  by an indented line is `unexpected indent` the same way. A trailing run of blank
  lines still moves CPython's position onto the last blank line, which this
  does not follow.
- **`pow()` dispatches the power dunders.** The builtin went straight to the
  native power, so `pow(P(), 2)` and `pow(2, P())` raised where `P() ** 2`
  worked, and any three-argument call with a non-integer said
  `pow() 3rd argument not allowed unless all arguments are integers`. Two
  arguments are now exactly `a ** b`; three follow CPython 3.14's `ternary_op`
  (`a.__pow__(b, m)`, then `b.__rpow__(a, m)`, then the three-type
  `unsupported operand type(s) for ** or pow(): 'A', 'B', 'M'`; `float` and
  `complex` refuse a modulus). The native `**` failure now says
  `** or pow()` as CPython's does.
- **`__bool__`, `__format__` and `__bytes__` results are checked.**
  `__bool__` returning `1` was accepted (CPython: `__bool__ should return bool,
  returned int`), `__format__` returning a non-`str` was stringified (CPython:
  `__format__ must return a str, not int`), and `bytes(x)` never called
  `x.__bytes__()` at all.
- **`AttributeError.obj` and the `name=`/`obj=` constructor keywords.** An
  `AttributeError` escaping an attribute read now carries the receiver as
  `obj` and the attribute as `name`, which is CPython's
  `set_attribute_error_context`: the lookup that missed records the receiver
  beside the rendered line (`PyHost::note_attr_miss`, the record the "Did you
  mean" hint already used) and `synth_exc` reads it back on an exact line match;
  a user `__getattr__` or property raising its own `AttributeError` has the live
  object augmented, unless it already carries a name or object. A property
  whose body misses on another object keeps that inner context, as in CPython.
  `AttributeError(…, name=, obj=)` and `NameError(…, name=)` take their
  keyword-only arguments with `getargs.c`'s refusals, and the slots read `None`
  when unset, where they raised `AttributeError`.
- **A dedent matching no outer level wins over the parse error after it.**
  `try:` / `if 1:` with a body indented 8 and a following line at 4 reported
  `SyntaxError: expected 'except' or 'finally' block` (or no caret): the
  tokenizer stops at the bad dedent, and the parser failing at that truncated
  end is CPython's tokenizer raising `IndentationError: unindent does not match
  any outer indentation level` when the parser asks for the next token. It is
  now raised there, positioned just past the end of the line as CPython does.
- **A binary operator's caret anchor reaches into a parenthesized right operand.**
  CPython's `ast.BinOp` anchor (`traceback._extract_caret_anchors_from_line_segment`)
  is one character wider than a one-character operator when the next character
  lies before the right operand's AST position, and a parenthesized group has no
  node of its own, so its position is inside the parentheses: `1+("a")`
  underlines `~^^~~~~`, where pythonrs stopped at the operator (`~^~~~~~`). The
  parser records each group's inner position and `Parser::binop_tail` applies the
  rule for every binary operator; a tuple display or a group followed by more of
  the operand keeps the bare operator, as in CPython.
- **Identifiers are NFKC-normalized (PEP 3131).** The lexer folds every
  non-ASCII identifier to NFKC, as CPython's `_PyPegen_new_identifier` does, so
  `ﬁ = 3` binds `fi` and `def ｆ()` defines `f`. Keywords are still recognized
  by the raw spelling first: `ｉｆ = 1` binds the name `if`, `ｍatch x:` is not a
  match statement, and `Ｎｏｎｅ` raises CPython's `ValueError: identifier field
  can't represent 'None' constant`.
- **A starred class base spreads at run time.** `class C(*bases)` raised
  `SyntaxError: invalid syntax`; the class header now takes `*iterable` with the
  call's ordering rules, and the base list is built as a list display is, so a
  starred base spreads and an oversized base list is chunked.
- **`collections.deque` operators.** `deque + deque`, `deque * n`, `n * deque`,
  `q += iterable`, `q *= n`, `<`/`<=`/`>`/`>=`, and the bound
  `__add__`/`__iadd__`/`__mul__`/`__rmul__`/`__imul__` all raised
  `unsupported operand type(s)`. They are ports of `_collectionsmodule.c`'s
  `deque_concat`/`deque_repeat`/`deque_inplace_concat`/`deque_inplace_repeat`:
  the result keeps the left deque's `maxlen` (so a bounded one keeps its last
  `maxlen` items), `+=` takes any iterable, `*=` mutates the receiver, and a
  size-times-count past `Py_ssize_t` is `MemoryError` before the bound is
  consulted, as in CPython.
- **A sequence repetition dunder reads its count as an index.**
  `[1].__mul__('a')` and the other repetition slots called by name said
  `can't multiply sequence by non-int of type 'str'`; CPython's
  `wrap_indexargfunc` says `'str' object cannot be interpreted as an integer`
  (and `OverflowError` past `Py_ssize_t`). `x *= Idx()` on a list, bytearray or
  deque also rebound `x` to a new object instead of mutating it.
- **Operator errors name `collections` types by `tp_name`.**
  `[1] + deque()`, `1 / OrderedDict()`, `-defaultdict()`, `deque() < 1` and the
  rest printed `'deque'` where CPython prints `'collections.deque'` (the C
  containers' `tp_name` is module-qualified; `Counter`, being pure Python, is
  not).
- **`{}.pop(unhashable)` is a `KeyError`.** `PyDict_Pop` reports an EMPTY dict
  as "not found" before hashing, so `{}.pop([1])` raises `KeyError: [1]` (or
  returns the default) where pythonrs raised the unhashable-key `TypeError`;
  `OrderedDict.pop` hashes first and keeps the bare `unhashable type` message.
  This was the last shape of the unhashable-key entry: the other sixteen
  already named the container role.
- **Dict views carry their own set operators; `set` declines a list.**
  `{1} | [1]`, `[1] & {1}`, `frozenset() ^ (1,)` and `{1} - [1]` returned sets
  where CPython raises `unsupported operand type(s)` (`set_or` and friends
  return `NotImplemented` unless both operands are sets), while
  `d.keys() | 'ab'`, `range(3) - d.keys()` and any view op with a generator,
  string or range raised it. The view operators are now ports of
  `dictviews_sub`/`dictviews_or`/`dictviews_xor`/`_PyDictView_Intersect`/
  `dictitems_xor`: any iterable on either side, always a plain `set`
  (`frozenset({1}) | d.keys()` included), `d.keys() | 1` is `'int' object is
  not iterable`. Membership in a keys or items view is a lookup in the dict
  (`dictkeys_contains`/`dictitems_contains`), so a user-hashed key is found and
  an unhashable one is named as a dict key.
- **`set.symmetric_difference_update` makes its argument a set first.**
  `{1}.symmetric_difference_update([2, 2])` toggled `2` twice and left `{1}`;
  `s.symmetric_difference_update(s)` now clears, as
  `set_symmetric_difference_update_impl` does.
- **Bitwise operator errors name the operator.** `[1] | [2]` said
  `unsupported operand type(s) for bitop`, and `<<`/`>>` said `for shift`.
- **An unhashable `collections` object is named by its type.** `hash(deque())`
  said `unhashable type: 'object'` and an `OrderedDict`/`defaultdict`/`Counter`
  said `'dict'`; a list subclass said `'list'`. The inner name is the failing
  type's `tp_name` and the outer `cannot use 'X' as …` name is `%T`, module
  qualified (`'collections.Counter'`, `'mm.Q'`).
- **`deque` item assignment and deletion.** `q[i] = v` and `del q[i]` raised
  `does not support item assignment`/`doesn't support item deletion`. Both now
  follow `PyObject_SetItem`/`DelItem`'s sequence branch into
  `deque_ass_item`/`deque_del_item`, and a slice or other non-index key on any
  of get/set/del is `sequence index must be integer, not 'slice'`.
- **`collections` dict types keep their type through `|` and `.copy()`, and
  compare as CPython does.** `defaultdict(int) | {}` and `OrderedDict() | {}`
  (either side) and `.copy()` of both came back a plain `dict`; they now take
  the type `defdict_or`/`odict_or`/`defdict_copy` build, a defaultdict keeping
  its `default_factory`. Two OrderedDicts with the keys in a different order
  compared equal; `Counter(a=1) == Counter(a=1, b=0)` was False and
  `Counter <= Counter` a TypeError -- the six `Counter` comparisons now read a
  missing count as zero. The operator slots of `defaultdict`/`OrderedDict` and
  of the keys/items views also answer as bound methods (`dd.__or__(d)`,
  `d.keys().__rsub__(it)`).
- **`Counter`'s in-place operators, `deque.copy`/`reverse`, `__missing__`.**
  `c += d` rebound `c` to a new Counter, `c += {'a': 1}` and `c -= …` raised
  TypeError, `c &= …` was unsupported and `c |= …` kept non-positive counts;
  they are now ports of `Counter.__iadd__`/`__isub__`/`__ior__`/`__iand__`
  (any mapping's `items()`, `_keep_positive`, the receiver returned). The
  Counter operator and comparison dunders answer by name. `deque.copy()`,
  `deque.__copy__()` and `deque.reverse()` did not exist, nor
  `defaultdict.__missing__`/`__copy__`, `Counter.__missing__` or a dict view's
  `mapping`.
- **`index()` bounds on tuple and deque; deque argument checks.**
  `tuple.index(x, start, stop)` ignored its bounds, a non-integer bound to any
  of the three was silently treated as absent, and `deque.index` said
  `5 is not in deque` and ignored its bounds; all three now share one port of
  `list_index_impl` (`slice_index` bounds, each type's own `x not in …`
  message). `deque.rotate('a')` rotated by one, and `deque(maxlen=-1)` /
  `deque(it, 'x')` built a deque; they raise as `deque_rotate`/`deque_init` do.
- **A `collections` type object is the type of its instances.**
  `type(deque()) is deque` was False and `type(q)(q)` raised
  `NameError: name 'deque' is not defined`: `type()` handed back a bare
  `deque` builtin while the module exported `collections.deque`. Both are now
  the one type object, which reprs as `<class 'collections.deque'>` (it printed
  `<built-in function deque>`), answers `deque.append`/`deque.__setitem__`
  unbound, lists `dict` in the MRO of `Counter`/`OrderedDict`/`defaultdict`
  (so `issubclass(Counter, dict)` holds), and `fromkeys` through any of the
  three mappings builds that mapping (`Counter.fromkeys` raises
  `NotImplementedError` as CPython's does).
- **Subclasses of the `collections` containers.** `class Q(deque)`,
  `class O(OrderedDict)`, `class C(Counter)` and `class D(defaultdict)`
  instances carried no native payload, so `len(Q([1]))` raised, `O(a=1)` reprd
  as `<__main__.O object …>` and `isinstance(O(), OrderedDict)` was False.
  They now hold their base's payload like a `list`/`dict` subclass: item
  access (with the base's `__missing__`), iteration and reversal, the base
  repr under the subclass's own name, `dict` in the mapping subclasses' MRO,
  and `super().__init__(it, maxlen)`. The results the C code builds through
  `type(self)(…)` are the subclass too -- `Q + Q`, `q * n`, `q.copy()`,
  `o.copy()`, `dd.copy()`, `o | d`, `d | o`, `dd | d` -- with the subclass
  `__init__` run for each. `defaultdict(x)` with a non-callable `x` is now
  `first argument must be callable or None`, and `OrderedDict`/`defaultdict`
  take any mapping (by `keys()`) as their initial data.
- **`stdout`/`stderr` are buffered as CPython buffers them.** pythonrs flushed
  every write, so `python prog.py > log 2>&1` came out in program order where
  CPython's comes out in flush order (`err1 err2 out1 out2` for two
  print/stderr pairs), `print("kept", end=""); os._exit(0)` kept output CPython
  loses, and `-u` had nothing to change. `src/stdio.rs` now ports both layers
  of `create_stdio`'s stack: `TextIOWrapper.write`'s 8192-byte pending chunk
  (flushed on a newline when line-buffered, every write when write-through)
  over `BufferedWriter.write`'s buffer, sized by `io.open`'s
  `max(min(st_blksize, 8 MiB), DEFAULT_BUFFER_SIZE)`. `stdout` is
  line-buffered on a TTY and block-buffered otherwise, `stderr` is
  line-buffered, and `-u`/`PYTHONUNBUFFERED` drops the buffer and writes
  through. The flush points are CPython's: a script file or stdin flushes
  before its traceback/`SystemExit` message (`_PyRun_SimpleFile`'s
  `flush_io`) while `-c` prints the traceback first; `atexit` callbacks run
  after the report and shutdown flushes stdout then stderr; `input()` flushes
  stderr, writes the prompt to `sys.stdout` and flushes it; each REPL statement
  ends with `flush_io`. `print` makes one `write` per separator / argument /
  `end` (`builtin_print_impl`), honours `flush=`, and prints nothing when
  `sys.stdout` is `None`. The embedded interpreter's `sys.stdout`/`sys.stderr`
  are write-through `TextIOWrapper`s over the same streams, so CPython-side
  `print` shares the one buffer. The DAP adapter switches the streams to
  write-through so `output` events are not held to the end of the run. This is
  also what `examples/argparse_demo.py`'s usage block was missing: it now
  precedes the program's stdout in a merged log, as under CPython (the
  `choose from 'fast', 'safe', 'auto'` quoting already matched).
- **`-O`/`-OO` optimize.** `-O` only reached `__debug__`. The compiler now
  carries the level: at 1 an `assert` compiles to nothing, so neither its test
  nor its message runs (`codegen_assert`), and at 2 module, class and function
  docstrings are dropped, so `__doc__` is `None`. `sys.flags.optimize` reports
  the level, and the level is part of the bytecode-cache key, as it is part of a
  `.pyc`'s name, so a chunk compiled at one level is never served to another.
- **`m.lastindex` / `m.lastgroup` name the group that closed last.** They
  named the highest-numbered group that took part, so nested groups
  (`re.match('((a)b)', 'ab')`) answered 2 where CPython's `_sre.c`, which
  records a group as it closes, answers 1 — the outer group closes after the
  inner one. `PyRegex` now records where each group's `)` sits and picks the
  group that ends last, ties going to the later `)`.
- **`re` matches bytes.** A bytes subject raised `TypeError: expected string`.
  A bytes pattern now runs over `bytes`, `bytearray` and `memoryview` (decoded
  latin-1, so codepoint positions are byte offsets: `span() == (3, 4)` on
  `'aéb'.encode()`), groups/`findall`/`split`/`sub` hand back `bytes`,
  `m.string` is the subject object, and the pattern is ASCII as CPython's
  bytes patterns are — `\w \W \d \D \s \S \b \B` and IGNORECASE stop at
  0x7F (they matched and folded `b'\xe9'`/`b'\xa0'`), which also gives
  `re.ASCII` its meaning on str patterns (the flag was ignored). Mixing kinds
  raises CPython's `cannot use a string pattern on a bytes-like object` and
  its counterparts, `re.compile(b'…', re.U)` is the `ValueError`, and a `sub`
  replacement or `expand` group of the wrong kind fails with the join's
  `sequence item N: …` message (a non-str callable result used to be dropped
  silently). Pattern and Match reprs are CPython's `%R` (truncated to 200 and
  50 characters) instead of a hand-quoted string.
- **`finditer` is lazy, and `Pattern.scanner` exists.** `finditer` built every
  match up front and returned a `list_iterator`; it is now `_sre.c`'s
  `iter(pattern.scanner(s, pos, endpos).search, None)` — a `callable_iterator`
  over a native `SRE_Scanner` that finds one match per step (and now honors
  `pos`/`endpos`).
- **`re.Scanner` exists.** It is CPython's Python class (`src/stdlib/pyre.rs`),
  run on pythonrs over `Pattern.scanner` and `lastindex`; the combined pattern
  is built as text, so the phrase is found by its wrapper group's number
  rather than `lastindex - 1`.
- **`Match.regs`, `Match.re`, `Match.lastgroup`, `Match.expand()` and `m[g]`**
  were listed as missing under `re`; all five already answered as CPython's do
  (re-measured), and the entry was stale.
- **`itertools.groupby` is lazy.** It drained its input and built every group
  as a list up front, so it never returned on an infinite iterator
  (`groupby(count(), key=…)`), each group was a `list` instead of an
  `itertools._grouper`, and advancing the groupby did not empty the group
  handed out before. It is now a port of `itertoolsmodule.c`'s
  `groupby`/`_grouper`: the groupers share the input, a grouper is exhausted
  once its groupby moves on, and keys compare through a user `__eq__`.
- **A metaclass `__repr__`/`__str__` renders the class.** `repr(cls)`,
  `str(cls)`, `print(cls)`, f-strings, `%`-formatting and containers printed
  `<class '__main__.C'>` whatever the metaclass defined.
- **Builtin argument checks CPython makes.** `print(…, end=None)` printed a
  literal `None` (and `sep=`/`end=` accepted any type); `bytes('x')` and
  `bytearray('x')` encoded as UTF-8 instead of raising `string argument without
  an encoding`, and `bytes('x', errors=…)` took the handler for the encoding;
  `isinstance(1, 2)`, `issubclass(int, 2)` and `issubclass(1, int)` answered
  `False`; `reversed()` of a set, a generator or a number drained or misnamed
  it; `map(f)` with no iterable built an empty map; `'x'.split(None, 'a')`,
  `'x'.center('a')`, `.zfill(None)` and a non-int `replace` count fell back to
  a default; and the `%c`, `in <string>` and `split(1)` messages differed. All
  raise CPython's `TypeError` now.
- **`isinstance` against `collections` and bridged builtin types.**
  `isinstance(od, collections.OrderedDict)` (and `defaultdict`, `deque`,
  `Counter`) was `False`; a native function or generator was not an instance
  of `types.FunctionType` / `types.GeneratorType` (served by CPython, where the
  native value is a proxy); and `issubclass` of a bridged class such as a
  `collections.namedtuple` raised `arg 1 must be a class`.
- **A `yield` anywhere in a function makes it a generator.** Only a
  statement-level `yield` (`yield x`, `y = yield x`, `return (yield)`) was
  seen, so a function whose `yield` sat inside an expression —
  `print((yield 1))`, `y = (yield) + 1`, `if (yield):`, `for x in (yield):`,
  `{'k': (yield)}`, `raise E((yield))` — ran as a plain function and died with
  `TypeError: 'yield' outside a generator`. The test now walks every
  expression the function's own scope evaluates, as CPython's symbol table
  does; a `yield` in a nested `lambda`/`def` body still belongs to that body.
- **Tracebacks carry generator frames and drop comprehension frames.** An
  exception escaping a generator body left the generator's own frame out of
  the traceback (and named it after the defining class when it had one);
  it is now listed as `in g` / `in <genexpr>` between the frames it called
  and the resumer. PEP 479's `RuntimeError` starts at the resumer while the
  chained `StopIteration` keeps the generator's frame, as in CPython. A
  list/set/dict comprehension, which runs as a hidden function here, showed
  an extra `in <comp>` frame under a `line 0` caller; CPython 3.12+ inlines
  comprehensions (PEP 709), so that frame is folded into its caller, which
  takes its line and caret.
- **Unpacking, `assert`, `del`, `for` and `.throw()` raise positioned.** A
  wrong-length unpack (`a, b = [1]`, `for a, b in …`, `with … as (a, b)`), a
  failed `assert`, a `del` of an unbound name, a `for` loop whose iterable
  raised on `iter()`/`next()`, and an exception thrown into a generator at
  its `yield` all rendered `line 0` with no source line. Each op now carries
  its line and CPython's caret: the target (`^^^^` under `a, b`), the asserted
  test, the deleted name, the loop's iterable, the `yield`.
- **Walking a `str` by index is linear, not quadratic.** `s[i]` collected the
  whole string into a `Vec<char>` and `len(s)` counted every character, on each
  call, so `while j < len(s): c = s[j]` cost O(n²): 40 000 characters took 90 s
  where CPython takes milliseconds, and brace-matching port-report generators
  never finished. Both now read a per-string character index (`StrIndex` in
  `host.rs`: none for ASCII, byte offsets otherwise), built once for the string
  being walked.
- **A native `list[int]` or `int | str` subscripts a CPython `typing` generic.**
  `def f() -> Optional[tuple[int, int]]` died at definition time with
  `TypeError: cannot pass 'GenericAlias' to a CPython stdlib call`: the
  stdlib-bridge conversion had no arm for the native PEP 585 alias or PEP 604
  union. Each now crosses as CPython's own object — `types.GenericAlias(origin,
  args)` and `typing.Union[args]` over the converted members — so
  `Optional[dict[str, list[str]]]` and `List[tuple[int, int | None]]` build and
  repr as CPython prints them. `type(list[int])` reprs `<class
  'types.GenericAlias'>` instead of `<built-in function GenericAlias>`.
- **Syntax errors are positioned and reported the way CPython reports them.**
  A program that did not compile printed one bare line —
  `SyntaxError: invalid syntax` for `x = (`, `expected ':' but found Newline
  (line 2)` for a header without its colon, `expected a name, found Op(":")`
  for `def f(:` — where CPython prints the `File "…", line N` header, the
  source line and a caret run, then its own message. The tokenizer and parser
  now attach CPython's position to every error they raise, the exception built
  from one gets `lineno`/`offset`/`end_lineno`/`end_offset`/`text`/`filename`
  and CPython's `args` shape, and the script, `-c`, stdin, REPL, `exec`,
  `eval` and import paths all render the block. The messages followed:
  `'(' was never closed`, `unmatched ')'`, `closing parenthesis ']' does not
  match opening parenthesis '('`, `unterminated [triple-quoted] [f-|t-]string
  literal (detected at line N)`, `expected ':'` (only where CPython's grammar
  says so), `expected 'else' after 'if' expression`, `Missing parentheses in
  call to 'print'`, `invalid syntax. Perhaps you forgot a comma?`, `Generator
  expression must be parenthesized`, `':' expected after dictionary key`,
  `Expected one or more names after 'import'`, the parameter-list rules
  (`named arguments must follow bare *`, `parameter without a default follows
  parameter with a default`, `* argument may appear only once`, `arguments
  cannot follow var-keyword argument`, `duplicate argument`), `leading zeros
  in decimal integer literals are not permitted`, `invalid digit '2' in binary
  literal`, and `unexpected indent` / `expected an indented block` with
  CPython's positions. Four programs the parser used to ACCEPT are now refused
  as CPython refuses them: two expressions side by side (`a b` ran `a`),
  `print 1`, `a := 1` as a statement, and `lambda: yield`; `0777`, `def f(*)`
  and duplicate parameters ran too. `'return' outside function`, `'break'
  outside loop`, `'continue' not properly in loop` and a module-level
  `nonlocal` are raised once the file has parsed, so a syntax error anywhere
  in the file wins over them, as it does in CPython.
- **PEP 758 `except A, B:`** (3.14) catches any of the listed types without
  parentheses, for `except*` too; with `as` the parentheses are still required,
  and leaving them out is CPython's `multiple exception types must be
  parenthesized when using 'as'`. `site.py` in the vendored stdlib uses the
  form, and did not compile.
- **A `SyntaxError` has its attributes.** `SyntaxError('m', ('f.py', 3, 4,
  'txt'))` bound none of `msg`/`filename`/`lineno`/`offset`/`text`/
  `end_lineno`/`end_offset`/`print_file_and_line`, so `e.lineno` inside a
  handler raised `AttributeError`, and `str(e)` printed the argument tuple
  where CPython prints `m (f.py, line 3)`. `SyntaxError_init` is ported: every
  attribute exists (`None` by default), a 4- to 7-item details iterable fills
  them (fewer or more is CPython's `TypeError`), and `str()` renders from the
  attributes — the BASENAME of a `str` filename and an exact-`int` lineno —
  so assigning `e.lineno` changes it. `IndentationError` and `TabError` share
  it. A SyntaxError the compiler raises gets the same attributes, with
  `lineno` taken from the ` (line N)` its message carries; its `offset`/`text`
  stay `None` and its wording is still pythonrs's own (see below).
- **Misplaced `_` in a numeric literal is a `SyntaxError`.** The lexer skipped
  every underscore, so `1_`, `1__0`, `1_.5`, `1.5_`, `1e_1`, `1_e1`, `1_j`,
  `0xf_`, `0b1__1` and `0o7_` all evaluated. A separator is now accepted only
  between two digits of the literal's base, or directly after a
  `0x`/`0o`/`0b` prefix, and anything else is `invalid decimal literal` /
  `invalid hexadecimal literal` / `invalid octal literal` /
  `invalid binary literal`, as in CPython.
- **`re` iterates, substitutes and reports the way `_sre` does.** All four
  match walks (`findall`, `finditer`, `sub`, `split`) took the Rust engines'
  own iterator, which refuses an empty match where a non-empty one just ended;
  CPython (3.7+) allows it, so `re.sub('x*', '-', 'abxd')` was `-a-b-d-` for
  `-a-b--d-`. A `sub` template was translated into the `regex` crate's `$`
  syntax, which dropped the backslash of `\.`, accepted `\q`/`\x41`, ignored
  octal escapes and out-of-range group references; it is now parsed by a port of
  `re._parser.parse_template`, with its errors and positions, and `m.expand()`
  uses the same parser. `p.search(s, pos)` searched the SLICE `s[pos:]`, so `^`,
  `\b` and look-behind saw a fresh string start at `pos`. `m[g]`, `m.re`,
  `m.lastgroup`, `m.regs` and `p.groupindex` existed only as `AttributeError`s,
  `m.start(9)` answered group 0's position instead of `IndexError`, and
  `p.flags` left out the implied `re.UNICODE` and the pattern's own `(?i)`.
  `re.match(src, s)` compiled — and kept — a new regex on every call; patterns
  are now cached by `(source, flags)` like `re._compile`.
- **`**` and `dict()` take any mapping.** `{**x}` and `f(**x)` spread only a
  plain `dict`: a `dict` subclass, a `mappingproxy`, a `ChainMap` or a class
  with `keys()`/`__getitem__` spread as nothing, and a non-mapping (`{**[1]}`,
  `f(**None)`) was silently empty. They now merge through the same mapping test
  as `dict.update` (a `dict` subclass from its storage unless it overrides
  `__iter__`), and the errors are CPython's — `'list' object is not a mapping`,
  `__main__.f() argument after ** must be a mapping, not list`, `keywords must
  be strings`. `dict(seq)` skipped a pair of the wrong length instead of raising
  `dictionary update sequence element #N has length L; 2 is required`. The
  callee in every `**`-merge error is now named as `_PyObject_FunctionStr`
  names it (`__main__.C.m`, `__main__.outer.<locals>.inner`, `list.append`),
  where a method was its bare name.
- **PEP 3131 identifiers.** The tokenizer started a name only on an ASCII
  letter, so `π = 3` or `def función(año)` was `SyntaxError: invalid syntax`.
  A non-ASCII character that cannot be part of a name is now CPython's
  positioned `invalid character '€' (U+20AC)`, and a stray ASCII one (`$`,
  `?`) is positioned too.
- **Traceback carets follow `_should_show_carets`.** A `return f(...)` /
  `x = f(...)` call hid its carets even when it was not the line's first
  statement (`def g(): return f(x)`), and even with non-ASCII text in it, where
  CPython's byte-vs-character offset comparison keeps them. A call through
  `*`/`**` unpacking had no position at all, so its carets were missing.
  `str()` of a `mappingproxy` is its mapping's, and a bare `*` no longer shows
  up in `co_varnames` as an empty name or sets `CO_VARARGS`.
- **`int` conversions are bounded by `sys.get_int_max_str_digits()`.** pythonrs
  had no limit and no `sys.get_int_max_str_digits`/`set_int_max_str_digits`, so
  `int('9'*100000)` succeeded where CPython 3.14.7 raises `ValueError: Exceeds the
  limit (4300 digits) for integer string conversion: value has 100000 digits; …`.
  The limit (default 4300, seeded from `PYTHONINTMAXSTRDIGITS`, which is
  validated at startup with CPython's fatal message) now bounds every BASE-TEN
  conversion: `int(str)`/`int(bytes)` in any base that is not a power of two
  (digits counted without sign or underscores, checked only after the literal
  is known valid, naming the count), `str`/`repr`/`ascii`, container reprs,
  `%d`/`%s`/`%r`, `format`/f-strings with a decimal presentation, `int.__str__`,
  an `int` subclass with no `__str__` of its own, and an over-long decimal
  LITERAL, which is a `SyntaxError`. Hex, octal and binary are unbounded both
  ways, as in CPython. `set_int_max_str_digits` takes `maxdigits` by position
  or keyword, rejects anything but `0` or `>= 640`, and is reflected in
  `sys.flags.int_max_str_digits`. Two fixes came out of it: a big-int literal
  now compiles to `int(<hex>, 16)`, so a literal the lexer accepted cannot fail
  at run time, and the `stdlib-ffi` bridge moves ints past `i64` in HEX in both
  directions — through decimal, the embedded interpreter's own limit made
  `int(Decimal('1e5000'))` raise instead of answering.
- **`int()`'s invalid-literal message cuts the argument's repr at 200
  characters.** CPython formats it with `%.200R`; pythonrs printed the whole
  repr, so a long bad literal produced a message thousands of characters long
  where CPython's stops after 200 characters, mid-string, without its closing
  quote.
- **A function scope's bindings no longer go through a hash table.** Every
  bind and every bare-name read hashed the name into an `IndexMap`, for scopes
  that hold a handful of names. A `sample` profile of a 400k-call benchmark put
  all 69 samples under `bind_params`' dominant node inside `IndexMap::insert` →
  `hashbrown::find_or_find_insert_index` — the cost is HASHING AND PROBING, not
  the key allocation. `EnvVars` (`src/host.rs`) keeps the bindings in a Vec in
  insertion order until a scope outgrows 12 names and then spills to the same
  map; `locals()` order, in-place rebinding and `del` are unchanged either side
  of the spill. -11.32% instructions retired on the call benchmark against a
  -0.019% A/A control, and the absolute saving doubles exactly when the call
  count doubles (per-call, not a fixed cost). Module globals are a separate map
  and are untouched.

- **Every repr that names a live object now names its TYPE and address.** Nine
  of them printed a single shared placeholder, so no two objects of a kind could
  be told apart: `repr(iter(x))` was `<iterator>` for ALL of
  `list_iterator`/`tuple_iterator`/`str_ascii_iterator`/`str_iterator`/
  `set_iterator`/`dict_keyiterator`/`range_iterator`/`longrange_iterator`/
  `list_reverseiterator`/`collections._deque_iterator`, even though
  `type(it).__name__` already knew which one it was; `[1].sort` was
  `<bound method>` where CPython prints `<built-in method sort of list object at
  0x…>`; `C().f` was `<bound method>` where CPython prints `<bound method C.f of
  <__main__.C object at 0x…>>`; `C.f` was `<function f>` (the bare `__name__`)
  where CPython prints `<function C.f at 0x…>` and a nested function
  `<function g.<locals>.h at 0x…>`; `C().g()` was `<generator object C at 0x…>`
  (the OWNER class) where CPython prints the qualname `<generator object C.g at
  0x…>` — and the never-awaited warning had the same bug, reporting
  `coroutine 'm'` for `C.m`; `property()` and `functools.cached_property` printed
  no address; `object()` was `<__main__.object object at 0x…>` and
  `object().__class__` `<class '__main__.object'>`, qualifying a BUILTIN into the
  running module; and `C().__init__` was `<method-wrapper '__init__' of object>`
  rather than of the instance's own class.
  `type()` was wrong alongside the repr: `type([].sort).__name__` answered
  `method` (a bound Python method) for what CPython calls
  `builtin_function_or_method`, and a slot dunder is a third thing again —
  `type([].__len__).__name__` is `method-wrapper`. Which dunders are slot
  wrappers is per type and irregular (`[].__getitem__` is an ordinary built-in
  method, `''.__getitem__` is a wrapper), so `SLOT_WRAPPERS_EVERY_TYPE` /
  `slot_wrappers_of` in `src/host.rs` are a table MEASURED off CPython 3.14.7,
  with the regeneration command in their doc comment. `PyObj::Descriptor` gained
  a `recv` so a bound method-wrapper can name its instance.
- **An argument after a keyword one is now a `SyntaxError`, not a silent
  reorder.** `f(a=1, 2)` is a compile-time error in CPython; pythonrs RAN the
  program and called `f` with `(2,) {'a': 1}`, exiting 0 where CPython exits 1.
  The AST keeps positionals and keywords in two separate lists, so the source
  order was already gone by the time anything could check it — the parser is the
  only place that still sees it. `ArgOrder` in `src/parser.rs` now tracks the two
  facts that matter (a `name=value` was seen, a `**mapping` was seen) in both the
  call and class-definition argument loops.
  Three messages, measured against CPython 3.14.7, with `**` unpacking winning
  the wording whenever it applies (`f(a=1, **k, 2)` reports the unpacking form
  even though a plain keyword came first):
  `positional argument follows keyword argument`,
  `positional argument follows keyword argument unpacking`, and
  `iterable argument unpacking follows keyword argument unpacking`.
  The legal orderings are unchanged and were pinned alongside the rejections:
  `f(*b, 2)`, `f(a=1, *b)`, `f(**k, b=1)`, `f(a=1, **k)` and `f(*b, a=1)` all
  still run, since `*` unpacking after a plain keyword is valid Python and only
  `**` forbids it.
- **A keyword a builtin does not take is now a `TypeError`, not a dropped name.**
  This is the general form of the `pow`/`isclose`/`groupby` gap below, and it was
  worse than an error: an unrecognised keyword was DISCARDED, so the call re-ran
  as its zero-argument form and answered SILENTLY WRONG. `float(x='1.5')` was
  `0.0`, `str(object=5)` was `''`, `bytes(source=[1,2])` was `b''`,
  `complex(real=1, imag=2)` was `0j`, `list(iterable=[1,2])` was `[]`,
  `bool(x=1)` was `False`, `set(iterable=[1])` was `set()`, `int(x='12')` was
  `0`, and `dir(object=1)` / `vars(object=1)` returned the caller's whole
  namespace. `round(2.567, ndigits=2)` was `3` and `sum([1,2,3], start=10)` was
  `6` — the keyword-only halves of two signatures, ignored. `sorted([3,1],
  bad=1)` and `map(str, [1], bad=1)` simply ran.
  The 42 names CPython defines without `…_WITH_KEYWORDS` now refuse every
  keyword (`NO_KWARG_BUILTINS`), and the ones that DO take keywords bind through
  a single `bind_named` — `round`, `sum`, `str`, `bytes`, `bytearray`,
  `complex`, `enumerate`, `memoryview`, `int`'s `base`, and `pow`, which was the
  one-off this generalises. Each name's contract was measured against
  /opt/homebrew/bin/python3 (Python 3.14.7), not inferred.
  Two orderings had to be measured rather than assumed. Argument Clinic reports a
  MISSING required argument BEFORE an unexpected keyword, so `pow(zz=1)` is
  `missing required argument 'base'` — the previous `pow_args` answered
  `unexpected keyword argument 'zz'`, so routing it through the shared binder
  fixed a residual of its own fix. The older `PyArg_ParseTupleAndKeywords` path
  reverses that and also words the refusal differently, which is why
  `enumerate(zz=1)` is `'zz' is an invalid keyword argument for enumerate()`
  (`KwStyle::Invalid`). `sum` counts keywords toward its arity before binding
  any, so `sum([1], start=1, bad=2)` is `takes at most 2 arguments (3 given)`.
- **A `slice` reprs its bounds.** `repr(slice(x, y))` printed the default
  `<Cls object at 0x…>` for an instance bound instead of dispatching its
  `__repr__`, because the host's `repr_of` is `&self` and cannot call back into a
  method. Slices now recurse through `py_repr` the way lists, tuples and dicts
  already did. Deliberately NOT joined to that layer's reentrancy guard:
  CPython's `slice_repr` takes no `Py_ReprEnter`, so for
  `l = []; s = slice(l); l.append(s)` the `[...]` marker comes from the list in
  between and the inner slice re-prints in full —
  `slice(None, [slice(None, [...], None)], None)`.
- **A `slice` is hashable and richly comparable.** CPython made slices hashable
  in 3.12; here the construction used to raise and the STORE form
  `d[slice(1, 2)] = 1` was worse — it was taken for a slice ASSIGNMENT, so the
  key never reached the dict at all. `PKey::Slice` now keys a slice by its three
  bounds (a DISTINCT variant from `Tuple`, because a slice and the tuple of its
  bounds must not share a dict slot), `pyhash::slice` reproduces CPython's own
  number, and the four subscript paths (`get_item_raw`, `del_item_raw`,
  `subscript_store`, `subscript_delete`) now tell a slice KEY from a slice INDEX
  by the RECEIVER, as CPython does, so a mapping takes it as a key while a
  sequence still splices. Slice `==`/`<`/`>` compare the bounds tuple, which also
  fixed `slice(1, 2) in [slice(1, 2)]` (it was False for every pair of distinct
  slice objects). `pyhash::slice` is NOT `pyhash::tuple`: CPython omits the
  length-mangling step, verified against CPython 3.14.7 over all 1 728 bound
  triples drawn from `None`/`0`/`1`/`-1`/`±2**63`/`10**30`/`'ab'`/`1.5`/`inf`/
  `(1, 2)`/`True` with zero mismatches.
- **Builtins bindable by keyword now read their keywords.** `pow`, `math.isclose`
  and `itertools.groupby` accept by keyword what they also accept positionally,
  and each read the positional slots alone — so the keyword forms did not raise,
  they answered with a DEFAULT: `pow(2, exp=3)` was `2`, `pow(2, 3, mod=5)` was
  `8`, `isclose(a, b, rel_tol=<obj with __float__>)` compared at `1e-09`, and
  `groupby(xs, key=f)` grouped by the raw element while reporting it as the key.
  `pow` now binds through CPython's Argument Clinic contract (`base`/`exp`/`mod`,
  the given-by-name-and-position error, the unexpected-keyword error and the
  at-most-3 arity), and `isclose` coerces both tolerances through `math_real`,
  the same protocol its positionals use.
- **`bool`'s numeric descriptors yield an `int`.** `True.real` and
  `True.conjugate()` handed the receiver straight back, so they were `True` where
  CPython gives `1` (`type(True.real)` is `int` and `True.real is True` is
  False). `.numerator` already normalized; the other two now match it.
- **A negative `r` is a `ValueError` from the combinatoric itertools.**
  `permutations(xs, -1)` cast `-1` to `usize`, wrapped it past the pool length
  and yielded nothing; `combinations(xs, -1)` clamped it to `0` and yielded the
  single empty tuple. Both now raise CPython's `ValueError: r must be
  non-negative`.
- **A module-level builtin reports its bare `__name__` and its own
  `__module__`.** `itertools.permutations.__name__` was the dotted
  `'itertools.permutations'` and `__module__` was `'builtins'`; the name split
  that type objects already got now applies to functions too.
- **`len`, `abs`, `min` and `max` check their argument count.** `len(a, b)` and
  `abs()` read the first slot and ignored the rest, and `min()` reported the
  empty-iterable `ValueError` — the message for `min([])`, a different mistake.
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
  handler. `AttributeError.obj` is bound too — see the entry above.
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
  integer, any other non-empty value is 1). Assert and docstring stripping
  under `-O`/`-OO` is covered above.
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
- **The `with` protocol check reaches every receiver.** It used to run only for
  user instances and the core scalars/containers, because the natively
  shadowed managers (a file, a lock, `contextlib.redirect_stdout`) and bridged
  CPython objects answered `__enter__`/`__exit__` only inside
  `call_method_inner`; `with len:`, `with sys:`, `with some_function:` and
  `with decimal.Decimal(1):` still failed with the old
  `AttributeError: ... has no attribute '__enter__'`. The check is now
  CPython's `_PyObject_LookupSpecial` — a lookup on the TYPE: a user instance
  asks its class (or the builtin base it extends), a class object asks its
  METACLASS (so `with C:` is an error even when `C` defines `__enter__`, and a
  metaclass defining the pair makes it work), a CPython object asks CPython's
  `type(obj).__mro__`, and every native value asks its type's method table, the
  same table `call_type_method` dispatches from. An `__exit__` stored on the
  instance no longer counts. The message names the type with CPython's `%T`
  (`'collections.OrderedDict' object …`, a script's own class bare), and an
  object implementing only the other protocol gets CPython 3.14's hint: `… but
  it supports the asynchronous context manager protocol. Did you mean to use
  'async with'?` (and the `with` counterpart).
- **`memoryview` is a context manager, and `release()` releases.** `with
  memoryview(b"ab"):` raised `AttributeError: ... '__enter__'`, and `release()`
  was a no-op. `memory_enter`/`memory_exit` are ported (enter yields the view,
  exit releases it), and a released view now refuses every operation with
  `ValueError: operation forbidden on released memoryview object` — methods,
  descriptor attributes, `len`, indexing and slicing, item and slice stores,
  iteration, membership, `bytes()`/`int()`/buffer conversion, re-wrapping in
  `memoryview()`, and entering it again — reprs as `<released memory at 0x…>`,
  and compares equal only to itself (`memory_richcompare`).
- **Vendored `_ast` fills field defaults and reprs like CPython.** In the
  self-contained build `repr(ast.Constant(1))` was `Constant(value=1)` and an
  unpassed field was simply absent. `AST.__init__` and `AST.__repr__` are now
  ports of 3.14's `ast_type_init` / `ast_repr`, driven by a `_field_types`
  table transcribed from CPython's ASDL-generated module: an optional field
  defaults to a class-level `None` (`Constant(value=1, kind=None)`), a sequence
  field to a fresh `[]`, an `expr_context` to `Load()`; a missing required
  field or an unknown keyword emits CPython's `DeprecationWarning` text; the
  constructor errors (`… takes at most N positional arguments`, `… got
  multiple values for argument 'id'`) match; the repr nests three nodes deep
  (`Name(...)`), elides the middle of a sequence longer than two, and guards
  self-reference. Node classes report `__module__ == 'ast'` and carry
  `__match_args__` and `_field_types`.
- **`contextlib.redirect_stdout` captures CPython-side writes.** pythonrs
  tracked the redirect only in `PyHost::stdout_target`, so output written
  through the embedded interpreter — `functools.partial(print, …)`, a bridged
  module printing — escaped the buffer and reached the terminal out of order.
  Every redirect (`redirect_stdout`/`redirect_stderr` enter and exit, `sys.stdout
  = x`) now goes through `PyHost::set_std_target`, which swaps the embedded
  interpreter's `sys.stdout` as well, since CPython has exactly one: a CPython
  target (`io.StringIO`) is installed as itself (`sys.stdout is buf` holds on
  both sides), a pythonrs target behind a write-through stream
  (`ffi::PyrsRedirectStream`) that receives CPython's piecewise `print` writes,
  and `None` as `None`. An object that grabbed the stream before the block — a
  `logging.StreamHandler` — keeps writing where it pointed, as in CPython. A
  redirect set before the interpreter starts is applied when it does. The
  stream is bound to the owning host's thread and generation, so a write from
  another thread or after a `reset_host` goes to the native stream rather than
  resolving the value against another heap.
- **A CPython call result that is not fresh keeps its identity.** Every call
  result was copied into a native value, so `with
  warnings.catch_warnings(record=True) as w:` bound a COPY of the list
  `warnings.warn` appends to — `w` stayed empty — and an `lru_cache`d list, or
  an argument the call handed back, came back as a new object. A result that
  is one of the call's own by-value arguments now returns the pythonrs
  original (already updated by the argument write-back), and a mutable
  container something else still references (refcount above the result's own)
  stays behind a handle like an attribute read. Only a container the call
  created is copied.
- **CPython's builtin types cross as pythonrs's, and `type(int | str) is
  types.UnionType` holds on the ffi build.** A CPython `int`/`list`/`dict`/…
  type object returned over the bridge (`dataclasses.fields(D)[0].type`) was a
  handle, so `is int` failed, and `type(handle_to_a_list)` was CPython's
  `list` rather than pythonrs's, so `type(x) is list` failed. Both now resolve
  to the native type object. The reverse holds for unions: `type(int | str)`
  and `(int | str).__class__` are CPython's own `typing.Union` — the object
  3.14's `types.UnionType` and `typing.Union` both name — so identity,
  `isinstance`, and `type(u)[int, None]` behave as in CPython.
- **A class reprs with its own module.** `repr(cls)` and the default
  `<… object at 0x…>` repr prefixed every program-defined class with
  `__main__.`, so a class from an imported module read `<class
  '__main__.Fraction'>` in the self-contained build and a class body's
  `__module__ = 'zz'` was ignored. The prefix is now the class's `__module__`
  (dropped for `builtins`), as CPython's `type_repr` does. The native `asyncio`
  primitives name their CPython modules too (`asyncio.locks.Lock`).
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
- **`__slots__` members** (CPython `type_new_descriptors`): each slot a class
  declares is a `member_descriptor` in its `__dict__` — `C.x` is `<member 'x'
  of 'C' objects>`, found at the declaring class's MRO position (a subclass
  that binds the name shadows it), one object per slot so `C.x is D.x`. It
  carries `__name__`, `__qualname__` and `__objclass__`; `__get__`, `__set__`
  and `__delete__` drive the slot by hand, `__get__(None, C)` is the
  descriptor, and an object not an instance of the declaring class is
  `TypeError: descriptor 'x' for 'C' objects doesn't apply to a 'int'
  object`. A slotted `__doc__` with no docstring reads as its member, `dir()`
  lists every class's slots, and a dict-valued `__slots__` declares its keys.
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
  output came out reordered, or was dropped at exit. The embedded interpreter's
  streams now write through into pythonrs's own (`ffi::route_std_streams`), so
  there is one buffer.
- **`sys.argv` reaches the bridged stdlib.** `argparse` — and `getopt`, `pdb`,
  `unittest` — run on the embedded interpreter and read ITS `sys.argv`, a list
  libpython builds at startup as `['']` because nothing passes the program's
  arguments to `Py_Initialize`. pythonrs's own `sys.argv` was correct all along,
  which is what made this so quiet: `parser.parse_args()` raised nothing, printed
  nothing and returned every option at its default with every positional dropped,
  so an argument-driven program ran to completion against the wrong inputs.
  pythonrs's live `sys.argv` is now mirrored across on every bridged import
  (`host::current_argv` → `ffi::queue_argv` → `ffi::apply_pending_argv`), the same
  queue-then-apply shape `sys.path` already used, and it is re-read from the `sys`
  module rather than from `PyHost::argv` so a program that rewrites `sys.argv`
  before importing argparse gets what it set. A rewrite performed after the last
  bridged import is not mirrored.
- **`open()` honours `encoding=` and universal newlines.** The builtin parsed
  only `file` and `mode`; `encoding`, `errors`, `newline` and `buffering` were
  accepted and dropped. Two silent wrong answers came out of that. A text handle
  always wrote UTF-8, so `open(p, 'w', encoding='latin-1').write('é')` put two
  bytes on disk where CPython puts one — the program asked for an encoding, got
  another, and nothing reported it. And no read translated line endings, so a
  CRLF file came back as `'a\r\nb'` from `read()` and `['a\r\n', 'b']` from
  `readlines()` instead of CPython's universal-newline `'a\nb'` / `['a\n', 'b']`.
  The handle now carries a `TextEncoding` (utf-8, ascii, latin-1, resolved through
  CPython's own name normalisation) and a `newline_translate` flag, and `open`
  takes CPython's real positional signature so the arguments land in the right
  slots. An encoding pythonrs cannot serve is `LookupError: unknown encoding: X`
  at open time rather than UTF-8 bytes at write time, a character the codec cannot
  represent is `UnicodeEncodeError` with CPython's wording, and binary mode
  rejects `encoding=`/`newline=` with CPython's two `ValueError`s. `read(n)` counts
  characters AFTER translation — a `\r\n` costs two bytes and yields one — so the
  reader tops up until it has `n` of them or reaches EOF. `errors=` is accepted and
  still ignored: decoding stays lossy.
- **A CPython-side file the program never closed keeps its writes.** `io.open`
  hands back a CPython stream, which is block-buffered, and the embedded
  interpreter is never `Py_Finalize`d — so nothing ran the teardown that flushes
  it. Every byte written to a handle the program did not close was lost, and the
  file was left on disk at the zero length `open` truncated it to: an empty file
  where a written one should be, with no error anywhere. Refcounting does not
  cover the dropped-handle case either (`io.open(p, 'w').write(s)` as a statement,
  or rebinding the only name), because the `Foreign` side-table holds a strong
  reference to every CPython object for the process lifetime, so the stream stays
  alive long past the program's last mention of it. Interpreter shutdown now
  flushes every writable, still-open `io.IOBase` found through `gc.get_objects()`
  and then the standard streams (`ffi::flush_open_files`, called from `lib.rs`
  beside the `atexit` teardown). A flush that raises is skipped: teardown must not
  replace the program's own outcome.
- **A `SystemExit` raised on the CPython side sets the exit status.** It crosses
  the bridge as an error string plus a `foreign_exc` record, never as a
  `PyObj::Exception`, so `classify_top_error` — which looked only at the latter —
  called it an ordinary uncaught exception. Every argparse program was affected at
  its two most common exits: `--help` printed the help text and then a traceback
  and exited **1** where CPython exits **0**, and a usage error printed CPython's
  `prog: error: …` line and then a traceback and exited **1** where CPython exits
  **2** — the status a caller actually tests for. `unittest.main()` and anything
  else that ends the program from bridged code were wrong the same way.
  `classify_top_error` now also recognises a foreign `SystemExit` and maps it
  through the same `system_exit_outcome` helper pythonrs's own `sys.exit` uses, so
  the code, the `str(code)` stderr message and the absent traceback all match. The
  record is matched against the error being classified, so a stale one from an
  earlier caught bridge call cannot claim an unrelated failure.
- **An exception raised by pythonrs code keeps its class for a CPython caller.**
  Every wrapper that hands a pythonrs callable to CPython — a `key=` function, a
  `json.dumps` default, a `unittest` test method, `functools.partial(fn)` — mapped
  a failure to `PyRuntimeError`, so `raise KeyError('k')` arrived as
  `RuntimeError: KeyError: 'k'`. A caller's `except KeyError` did not fire, and
  `unittest` — which decides FAIL vs ERROR purely on the class — logged every
  failed assertion as an ERROR. `ffi::call_err` now rebuilds the CPython exception
  from the live pythonrs exception object (falling back to parsing the class out
  of the `"Class: message"` rendering), the same two steps `body_err` already used
  for generator bodies, and is used on the three paths where the failure is the
  user's: `PyrsCallable::__call__`, `PyrsInstance::__getitem__` and the `PyrsFile`
  methods. The live object is only trusted when its class heads the error string,
  so a stale `h.exc` cannot claim an unrelated failure. A pythonrs-defined
  exception class that is not a builtin still arrives as `RuntimeError`.
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
  nonlocal '<x>' found` (a class body inside a function may bind the
  function's name), and one at module level is `SyntaxError: nonlocal
  declaration not allowed at module level`. The symbol table's declaration
  checks run over the whole module before any code is generated
  (`src/symtable.rs`): a `global`/`nonlocal` for a name already a parameter,
  used, annotated or bound in that scope is CPython's `name 'x' is parameter
  and global`, `is used prior to global declaration`, `annotated name 'x'
  can't be global` or `is assigned to before global declaration` (and the
  `nonlocal` forms), and a name declared both ways is `name 'x' is nonlocal
  and global`. Each is positioned at the declaring statement with `args ==
  (msg,)`, as CPython reports it; pythonrs used to run such a program.
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
  call whose *callee lookup* fails (`foo()` on an undefined name,
  `obj.missing()`) carets the callee, as CPython does. **Exception
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
  when neither resolves, a `TypeError` is raised. CPython's two ordering rules
  hold as well: the RIGHT operand goes first when its type is a proper subclass
  of the left's and overrides the reflected dunder (`A() + C()` runs
  `C.__radd__` and never reaches `A.__add__`), and two operands of the SAME type
  never consult the reflected half for ARITHMETIC — `A() + A()` whose `__add__`
  declines raises even though `__radd__` exists — while comparison does consult
  it (`B() < B()` tries `__lt__` then `__gt__`). An augmented assignment that no
  dunder answers names the augmented operator (`unsupported operand type(s) for
  >>=`), and a sequence reports its own concat/repeat refusal (`can only
  concatenate list (not "T") to list`, `can't multiply sequence by non-int of
  type 'T'`).
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
  backing is read-only. `<memory at 0x…>` repr. Item assignment THROUGH the view
  writes into the backing `bytearray` — `mv[i] = b`, `mv[i:j:k] = <bytes-like>`
  (every step, with CPython's fixed-length "different structures" rule rather
  than a splice), a sliced view writing at its own offset, and aliased views
  seeing each other's writes — with each refusal distinguished as CPython
  distinguishes it (`cannot modify read-only memory`,
  `memoryview: invalid type/value for format 'B'`,
  `index out of bounds on dimension 1`, `a bytes-like object is required`,
  `cannot delete memory`). Not covered: `cast` (format reinterpretation),
  multi-dimensional views, a view over any buffer that is not a
  `bytes`/`bytearray` (`memoryview(array.array('i', …))` raises
  `TypeError: memoryview: a bytes-like object is required, not 'array'`; CPython
  builds an `itemsize 4`, `format 'i'` view), and the export bookkeeping that
  makes CPython raise `BufferError: Existing exports of data: object cannot be
  re-sized` when a `bytearray` is resized while a view over it is alive.
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

- **`math.gamma`/`lgamma`/`erf`/`erfc` answer bit-for-bit**, which needed each
  from the same source CPython takes it from. `erf`/`erfc` are the platform's
  (CPython 3.14 declares them `FUNC1A(erf, erf, …)`); `gamma`/`lgamma` are ports
  of `m_tgamma`/`m_lgamma` from `Modules/mathmodule.c`, which CPython carries
  itself because the platform's are not accurate enough. The pure-Rust `libm`
  crate is neither, and disagreed in the last place on 312/1201, 390/1201,
  907/1194 and 976/1194 sampled points across `[-6, 6]`; a straight translation
  of the Lanczos code still disagreed on 524 and 637 until the multiply-add in
  `lanczos_sum` was contracted the way clang contracts it. `lgamma(-inf)` is
  `inf` — the log of a magnitude — not the domain error pythonrs raised.
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

- **Open from a parity probe against CPython 3.14.8.** Measured, not fixed:
  `format(EnumClass)` / `f'{EnumClass}'` raises `Enum.__format__() missing 1
  required positional argument` (the member `__format__` is called on the
  class); `ABC.register(C)` does not make `isinstance(C(), ABC)` true;
  `inspect.isgeneratorfunction(f)` raises `cannot pass 'code' to a CPython
  stdlib call`; a nested unpacking target
  (`a, (b, c) = 1, (2,)`) carets the outer target, CPython the inner one.
- **`m.lastindex` / `m.lastgroup` for a group closed inside a look-ahead.**
  Neither engine reports the order in which groups closed, so `lastindex` is
  rebuilt from the result: the group ending last, ties going to the group whose
  `)` comes later in the pattern (`PyRegex::last_closed_group`). A look-ahead
  closes its groups early but ends them late, so `re.match(r'(?=(ab))(a)',
  'ab').lastindex` is 1 here and 2 in CPython.

- **A misplaced `yield` after non-ASCII text is positioned in characters.**
  CPython's compiler raises `'yield' outside function` (and the other
  compiler-side `yield` errors) at `col_offset + 1`, a UTF-8 BYTE column, so
  `x = ("éé", (yield 1))` in a class body is offset 17 there and 15 here,
  and the caret CPython draws sits two columns further right. The parser
  records the `yield`'s `Span` in characters, which is what a runtime
  traceback caret needs; the compiler has no source to convert it with.
  Pattern errors carry their own byte-based `Loc` and are exact.
- **A bridged exception carries no CPython traceback.** An exception that crosses
  from pythonrs into CPython is rebuilt as a fresh exception object, so its
  `__traceback__` is empty. Two visible consequences, both in code that is not
  otherwise wrong: `logging.exception('…')` inside a pythonrs `except` block logs
  `NoneType: None` where CPython prints the four-line traceback (CPython's
  `sys.exc_info()` on the bridge side has no exception to report), and a
  `unittest` failure report carries the `AssertionError: 1 != 2` line without the
  `Traceback (most recent call last):` block above it. Repro:
  `import logging; logging.basicConfig(); \ntry: 1/0\nexcept ZeroDivisionError: logging.exception('boom')`.
- **A CPython-side `sys.stdout` reassignment does not redirect pythonrs's
  `print`.** The pythonrs → CPython direction is covered (see the Implemented
  entry on `redirect_stdout`); the reverse is not. When CPython code assigns
  the embedded interpreter's `sys.stdout` — `unittest`'s `buffer=True`
  runner, a CPython-side `contextlib` — pythonrs's `print` and `sys.stdout`
  still write to the native stream, so a buffered test run prints its
  captured output to the terminal (`TextTestRunner(buffer=True)` with a test
  that prints shows the text where CPython shows nothing). Seeing the
  assignment needs either a per-write probe of CPython's `sys.stdout` (a GIL
  round-trip on every `print`) or a `__setattr__` hook on the `sys` module
  (by reassigning its `__class__` to a `ModuleType` subclass, which changes
  `type(sys)`); neither is in place.
- **A pythonrs callable or object cannot be used from a worker thread.** On the
  bridged build `import threading` is CPython's, and CPython's `threading.py`
  imports the REAL C `_thread` — not the native `_thread` this crate ships — so
  `Thread.start()` spawns a genuine OS thread rather than running the target
  inline. `PyHost` is a `thread_local`, so on that thread the heap is empty and
  every pythonrs value resolves to nothing: the target comes back as a bare
  `object` and the thread dies with `TypeError: 'object' object is not callable`,
  once per thread, with the program's own result silently missing. Measured:
  `threading.Thread(target=print, args=('x',)).start()` works (the target is a
  CPython builtin) while `threading.Thread(target=res.append, args=(1,)).start()`
  fails for a pythonrs `res`, and `_thread.get_ident() != threading.get_ident()`
  because two different `_thread` modules are live at once. The native `_thread`
  arm and its inline-execution semantics therefore only apply to the
  `--no-default-features` build; the header comment in `src/stdlib/pythread.rs`
  describes that build, not this one. Closing the gap means putting the native
  `_thread` into the embedded interpreter's `sys.modules` before `threading` is
  imported, which would also make every thread on the default build serialise —
  a decision about what the default build IS, not a defect to patch.
- **Pickling across the bridge: what is still not CPython.** A class or function
  defined in an imported program module (not `__main__`) is looked up by CPython's
  importer under that module's name, which imports a separate CPython copy of the
  file, so `pickle` reports it is not the same object. `copy.copy`/`deepcopy` are
  native and do not consult `__copy__`/`__deepcopy__`/`__reduce_ex__`. A
  `bytearray` subclass is not a native builtin subclass at all.
- **A warning raised from pythonrs code is attributed to `<sys>:0`.**
  `warnings.warn` is CPython's C `_warnings.warn`, which locates the warning
  by walking CPython frames; a call from pythonrs has none, so the message
  renders as `<sys>:0: UserWarning: boom` with no source line (CPython:
  `script.py:3: UserWarning: boom` plus the line), a recorded warning's
  `filename`/`lineno` are `'<sys>'`/`0`, and `stacklevel=` has nothing to
  walk. Attributing it needs the executing line of every pythonrs frame at the
  moment of the call — `Frame::line` is only maintained by the DAP hook and the
  error path — plus each frame's filename and module globals for
  `warn_explicit`'s registry.

- **A left-recursive chain CPython compiles can exceed pythonrs's compile
  stack.** `a.b.c…`, `1+1+1…`, `f()()…` parse in a loop and fail, as in
  CPython, in the compiler's walks with `RecursionError: Stack overflow (used N
  kB) during compilation` once the stack runs out. Where that happens is a
  function of the stack and of each walk's frame size in both interpreters
  (CPython's own limit moves with the platform's thread stack): on macOS
  CPython 3.14.8 compiles up to 74 490 links, a debug pythonrs on its 512 MB
  stack up to 57 973 attributes, 50 617 additions and 48 563 calls, so a chain
  in that window compiles there and raises here.
- **pegen's level count is reproduced along the alternatives modelled.** The
  parser counts pegen's rule levels and refuses at `MAXSTACK` exactly where
  CPython does for every shape measured (unary, `not`, `lambda`, `**` and
  conditional chains; displays, calls, subscripts, slices, comprehensions,
  f-string fields and decorators; the statement contexts from a bare line to
  a nested `elif`). pegen's count is the deepest descent it makes, including
  alternatives it abandons, and those are modelled where they decide the
  answer — a statement's leading primary tried as an assignment target, the
  `genexp` tried before a call's arguments, an `expression` tried where none
  starts. Patterns, type parameters, `*args`/`**kwargs` annotations and
  format-spec fields follow the successful parse only, and can give way a few
  levels from CPython.
- **A CPython exception object raised from pythonrs is caught as a copy.**
  `e = binascii.Error('x'); raise e` raises (and `except ValueError as x`
  catches) the pythonrs exception paired with `e`, so `x is e` is `False`
  where CPython says `True`; class, args and type are right. A CPython
  exception held as a value stays a `Foreign` handle (keeping every attribute
  CPython gives it), while a raised one has to be a pythonrs exception for
  `except` matching, and nothing unifies the two.
- **`re.PatternError` from the regex engine has no `msg`/`pattern`/`pos`.**
  The class and its constructor are CPython's (see Implemented), but the
  native engine raises from a rendered line, so `re.compile('(')`'s error
  reads `'unclosed group'`-style wording and lacks the attributes
  `sre_parse` sets (`msg='missing ), unterminated subpattern'`, `pos=0`,
  `pattern='('`).
- **PEP 649: class-body annotations are evaluated eagerly.** Functions are lazy
  (see "Implemented"), but a class body still evaluates each simple annotation
  as it runs and drops one whose name does not resolve: `class C: x: Later`
  then `C.__annotations__` is `{}` where CPython 3.14 evaluates on that read
  and raises `NameError: name 'Later' is not defined` (or, once `Later`
  exists, returns it), and a class name rebound later in the body is seen at
  its old value. The class `__dict__` holds the evaluated `__annotations__`
  where CPython holds `__annotate_func__` (and `__annotations_cache__` after
  the first read). The annotation does see the class namespace (see
  "Implemented"). Making it lazy is blocked on the `FORWARDREF` substrate,
  not on the compiler: `typing.NamedTuple` and `TypedDict` (and `dataclasses`
  on a forward reference) read a namespace with no `__annotations__` through
  `annotationlib.call_annotate_function(..., FORWARDREF)`, which — the
  compiler's `__annotate__` refusing every format above 2, as CPython's does —
  re-runs the function under `types.FunctionType(annotate.__code__,
  fake_globals, closure=...)`. pythonrs functions have no `__builtins__` and a
  code object cannot be re-bound to other globals (measured:
  `call_annotate_function(f.__annotate__, Format.FORWARDREF)` raises
  `AttributeError: 'function' object has no attribute '__builtins__'`), so
  every `NamedTuple` class would stop building.
- **A `compile()` code object carries its source, not bytecode.**
  `compile(source, filename, mode)` checks the source in `exec`/`eval`/
  `single` mode — raising the positioned `SyntaxError` naming `filename` —
  and returns a `code` object with `co_filename`, `co_name` and
  `co_firstlineno` that `exec`/`eval` run as that mode (`ast.PyCF_ONLY_AST`
  returns `ast.parse`'s tree). It holds the source and recompiles it when run,
  so the rest of the code-object surface (`co_code`, `co_consts`,
  `co_varnames`, `dis.dis(code)`, `types.CodeType(...)`) is absent.
- **`bool()` of a released `memoryview` answers instead of raising.** Every
  other operation on a released view raises CPython's `ValueError: operation
  forbidden on released memoryview object` (see the Implemented entry), but
  truthiness goes through `PyHost::truthy`, which cannot fail, so
  `bool(released)` answers from the view's length where CPython's
  `memory_length` raises.
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
- **CPython frames between two pythonrs frames are not listed.** A pythonrs
  exception raised in a callback that CPython code called
  (`json.dumps(obj, default=f)` with `f` raising) is caught as itself, but its
  traceback goes straight from the calling line to `f`, where CPython lists
  `json/__init__.py` `dumps` → `encoder.py` `encode` → `iterencode` between
  them. The frames pythonrs captures carry no marker for where the error
  left for CPython, so the CPython traceback cannot be spliced in there. (An
  exception CPython itself raised does list its CPython frames — see
  Implemented.)
- **`dir()` of a native non-type VALUE is short or empty.** `dir()` of the 13
  builtin types (and their values) is CPython's full listing, but the other
  native kinds still come back empty — a function, a bound or unbound method, a
  builtin function, `super`, `staticmethod`/`classmethod`, a code object, the
  container iterators — or partial (an exception lacks `args`/`__traceback__`/
  `__cause__`/`__context__`/`__suppress_context__`/`__dict__`/`__setstate__`;
  `dict_keys`/`dict_items` lack the set operators and `mapping`; `memoryview`
  lacks `cast`/`count`/`index`/`toreadonly`/`suboffsets`/`__enter__`/`__exit__`).
  Listing them is gated on attribute access agreeing name for name, and several
  do not resolve yet: `f.__globals__`/`__builtins__`/`__call__`/
  `__type_params__`, `m.__func__`/`__self__`, `len.__self__`/
  `__text_signature__`, an iterator's `__length_hint__`/`__setstate__`, a code
  object's `co_code`/`co_lines`/`replace`.
- **`dir(type)` omits the five C-layout numbers.** `__basicsize__`,
  `__dictoffset__`, `__flags__`, `__itemsize__` and `__weakrefoffset__` describe
  a `PyTypeObject` struct pythonrs does not have, and a fabricated number would
  be read as a real one by the `Py_TPFLAGS_*` bit tests that consume them.
- **The `builtins` module is CPython's, not the native builtins.** `__main__`'s
  `__builtins__` and `__loader__` are bound (to the bridged `builtins` module
  and a `SourceFileLoader`/`BuiltinImporter`), but `import builtins` is the
  CPython module rather than the namespace pythonrs resolves names in, so
  `builtins.len is len` is `False` and `builtins.foo = 5` does not make a bare
  `foo` resolve (CPython prints `5`; pythonrs raises `NameError`).
- **`f.__annotate__`'s parameter introspects as `.format`.** The compiled
  annotate function binds its argument under CPython's internal symtable name
  `.format`, and pythonrs reports that name as is: `co_varnames` is
  `('.format',)` where CPython 3.14 renames it to `format` in the code object
  (and so in the `(format, /)` signature).
- **The `SyntaxError` keyword hint does not see names inside f-strings.**
  `_find_keyword_typos` walks `tokenize`'s NAME tokens, and since 3.12 those
  include the names in an f-string's replacement fields; pythonrs's lexer
  keeps an f-string as one token, so those names neither use up the
  ten-name budget nor get tried themselves. `x = f"{a}{b}{c}{d}{e}{f}{g}{h}{i}{j}"`
  followed by `whille x: pass` gets `Did you mean 'while'?` here and no hint
  in CPython, whose budget the ten field names exhausted.
- **`[nan] == [nan]` with one shared `nan` is False.** CPython's sequence
  comparison shortcuts on element IDENTITY before `==`, so a list holding the
  same `nan` object twice compares equal to itself. pythonrs stores a `float`
  unboxed, so two equal-valued floats are indistinguishable from one object and
  the shortcut cannot be reproduced. The identity shortcut IS applied to heap
  objects, which is what makes `[P(1)] == [P(1)]` and `[x] == [x]` correct for
  everything with a heap identity.

## VM-level limits (fusevm, not fixable from here)

pythonrs is a fusevm frontend and does not modify the VM crate. These are costs
measured inside it, recorded so the next round does not re-derive them.

- **The block-JIT eligibility answer is thrown away on every call.** fusevm
  caches it per VM (`VM::block_eligible_cached`, vm.rs:258-264) precisely to
  avoid a thread-local `HashMap` probe per run — but `VM::reset` clears that
  field unconditionally (vm.rs:895), and pythonrs's VM pool hands a pooled VM
  its OWN chunk straight back (`run_chunk_cached`, src/host.rs) and resets it.
  So the memo never survives a reuse and every Python call re-probes
  `BLOCK_ELIGIBLE_TLS` (jit.rs:4705-4743), a `std::collections::HashMap` with
  the default hasher, for an answer that is always the same and, for these
  chunks, always `false`. In a `sample` profile of a 400k-call benchmark
  (debug build) `JitCompiler::is_block_eligible` is 93 of 1984 main-thread
  samples (4.7%), of which 51 are `RandomState`/SipHash hashing the 9-byte
  `(op_hash, strict)` key. Nothing on the pythonrs side can reach the field or
  skip the reset in the fusevm pythonrs depends on: there `reset` is the only
  public way to reuse a VM. **Fixed upstream, not yet consumable:** fusevm main
  (`a5aa777edf`) adds `VM::rewind`, which restarts the chunk the VM already
  holds and keeps the eligibility memo (`reset` now delegates to it after
  clearing the per-chunk memos). What remains is on this side and needs a
  fusevm crates.io release containing it: bump the `fusevm` requirement and
  replace the `std::mem::take` + `vm.reset(own)` pair in `run_chunk_cached`
  with `vm.rewind()`. With fusevm main patched in locally, that one-line swap
  passes `lang`/`runtime`/`stdlib`/`opcodes`/`parity`.

## Tooling
- **`--build`** (AOT to a standalone native executable): implemented for the
  **libpython-free** build (`cargo build --no-default-features`). An uncaught
  exception in the AOT binary renders the same traceback the interpreter does —
  `File`/source line + CPython carets — and exits non-zero (the embedded image
  carries the source, filename, and caret position tables, and the binary
  recomputes each chunk's serde-skipped `op_hash` so caret lookups hit).
  `sys.exit(n)` returns `n`. A `stdlib-ffi` build cannot AOT (its
  CPython/pyo3 symbols can't be statically linked into a standalone binary — the
  build fails up front with that instruction).
- **`--dap`** (Debug Adapter Protocol): implemented — breakpoints, step
  in/out/over/continue, stack trace, locals, and program-stdout capture (pipe +
  dup2 → `output` events). Frame names in the stack use the function name (or
  `<module>`), shared with the traceback path. `evaluate` (watch, hover, debug
  console) runs any expression in the paused frame, as `eval` on the stopped
  line would.
- **`--lsp`**: full corpus — completion (builtins/keywords/methods), position-
  aware hover, diagnostics via the real parser, go-to-definition and signature
  help. The last two resolve names within the open document only — through its
  module, function and class scopes (`src/lsp_nav.rs`) — so a builtin, an
  attribute (`obj.name`, `self.method`) or a name imported from another file
  has no definition or signature to show.
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
  as-is against any CPython 3.9–3.14 (`abi3-py39`; no env pin, and
  `.cargo/config.toml` is gone with the `abi3-py313` floor that needed it).
  **Only a `--no-default-features` build drops the bridge** — there
  `import functools`/`import os` all raise `ModuleNotFoundError`.
- **`re` and `itertools` are NATIVE shadows in BOTH builds — they never reach
  CPython.** This entry previously listed both among the modules the FFI bridge
  serves, which was wrong in a way that matters: a probe that "passes" against a
  bridged module proves CPython works, while a probe against these two proves
  pythonrs's own code works, and only the second kind is evidence about the port.
  `module_ffi_fallback` covers exactly `math`, `collections`, `functools` and
  `contextlib`; `re` is not in that list, so a miss on the native namespace is a
  hard `AttributeError` and never defers. Checking against CPython 3.14.6:
  `hasattr(re, 'RegexFlag')` is `False` here and `True` there; `hasattr(itertools, 'batched')` is `False` here and `True`
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
- **`re.RegexFlag` is absent; the flag constants are plain ints.** CPython's
  `re.I` IS `re.RegexFlag.IGNORECASE`, an `enum.IntFlag` member that reprs as
  `re.IGNORECASE` and combines to `re.IGNORECASE|re.MULTILINE`. pythonrs has no
  native `enum`: in the default build `enum.IntFlag` exists only as a CPython
  class across the FFI bridge, and building `RegexFlag` there makes every
  `import re` start libpython (measured on the debug build: `-c 'import re'`
  0.01s, `-c 'import enum'` 0.03s) and turns every `flags` argument into a
  bridged object the native engine would convert per call; defining the class
  body on pythonrs fails outright (`__str__ = object.__str__` raises `cannot
  pass 'wrapper_descriptor' to a CPython stdlib call`). It needs a native
  `IntFlag`.
- **`re.ASCII | re.IGNORECASE` on a str subject folds a literal `k`/`s` to
  U+212A KELVIN SIGN / U+017F LONG S.** Classes, `\w` and non-ASCII literals
  fold ASCII-only (see the bytes entry in Implemented), but an ASCII letter
  outside a class keeps the crate's Unicode `(?i)`: `re.findall(r'(?i)k',
  'K\u212a', re.A)` is `['K', 'K']` here, `['K']` in CPython. Bytes subjects
  cannot hold those characters, so only str patterns under `re.ASCII` see it.
- **`SRE_Scanner.match()` after an empty match ends the scan.** `_sre.c` then
  requires the next match at the same place to be non-empty and tries the
  pattern's other alternatives for one; the engines cannot be asked for that,
  so `re.compile(r'x*|a').scanner('a')` answers `match()` with `None` where
  CPython finds `'a'`. `search()` (and so `finditer`, `findall`, `sub`,
  `split`) resumes one character on, which is what CPython's search does too.

### `hash()` values: what is reproduced, and what cannot be

`hash(x)` now returns CPython's own number. The algorithms are ported from the
CPython 3.14.6 C sources in `src/pyhash.rs` (`long_hash`, `_Py_HashDouble`,
`complex_hash`, `Py_HashBuffer`/`siphash13`, `tuple_hash`, `frozenset_hash`),
`range` and a read-only `memoryview` hash as `range_hash` and `memory_hash`
define them (a `(len, start, step)` tuple; the viewed bytes), and the
cross-bridge container collapse that follows from them works in both
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

### `set` iteration order after an element is removed

`setobject.c`'s table is reproduced by REPLAY (`host.rs`, `SetTable`): a set
keeps its elements in an order that, inserted one by one into a table of a
recorded starting size, lands each in CPython's slot. That covers every way a
set is BUILT — `.add()`, `set(iterable)`, constant and starred displays, the
presizing set/dict merges (`set(s)`, `.copy()`, `|`, `|=`, `.update()`), and
`&`, `&=` and the fresh-set path of `-` — for elements whose hash is
reproducible (ints, floats, complex, tuples of those). It does not cover
REMOVAL. CPython leaves a dummy in a removed element's slot, which later
probes step over and a resize later drops, and `.pop()` resumes from a search
finger. pythonrs replays a set that has lost elements as if they had never
been inserted, so a set shaped by `.discard()`/`.remove()`/`.pop()`/`-=`, by
`a - b` when `a` is over four times `b`'s size (a copy of `a` with `b`'s
elements deleted), or by `^` with shared elements can iterate differently:

```
a = set([14, 275, 22, 264, 205, 278, 288, 62, 251, 47, 353, 85])
a - {275, 234}   # CPython [..., 85, 22, 278, 251, 62]
                 # pythonrs [..., 85, 278, 22, 251, 62]
```

A 300-case differential run over `&`, `-`, `^`, `.intersection`,
`.difference`, discards, `-=` and `pop` differs from CPython 3.14 on 114 of its
1,500 output lines (942 before the merge layouts were modelled). Closing it
means recording deletions — the dummies and `pop`'s finger — in the replay.

### Open divergences found by round-3 protocol probing

Round 2 probed values -- numbers, strings, containers, exception text. Round 3
probed the PROTOCOLS: generators, context managers, descriptors, class
machinery, argument binding, control flow, operators. Most of that surface
already agreed with CPython 3.14.7 byte for byte. Four bugs came out of it and
are fixed (`zip`/`map` over a user iterator class, the `__iter__`-returned-a-
non-iterator message, the implicit `__hash__ = None`, `__len__` validation plus
PEP 479). The unhashable-key message was the fifth and is fixed too: an
unhashable key now names the role it was playing (`cannot use 'X' as a dict
key (unhashable type: 'X')`), matching CPython at all 17 spellings. Round 4
added the `__index__` coercion boundaries and the `__slots__` `"__dict__"`
entry. These remain open:

- **PEP 695 `type` aliases: what the native `TypeAliasType` still lacks.**
  The statement builds a lazy `typing.TypeAliasType` (see "Implemented"),
  whose value is an annotation scope (inside a class body it reads the class
  namespace). A type parameter's bound, constraints and
  default (`type B[T: int] = …`) are parsed and discarded (`__bound__` is
  `None`); `__parameters__` lists a `TypeVarTuple` bare where CPython shows
  `typing.Unpack[Ts]`; `evaluate_value` is absent; and under the bridge
  `type(A)` is pythonrs's own type object, not CPython's
  `typing.TypeAliasType` (`isinstance` does agree).

- **A `SyntaxError` the compiler raises from `eval`/`exec` omits the inner
  block.** One the parser or tokenizer raises is rendered as CPython renders
  it — the calling frames, then the `File "<string>"` block with the source
  line and caret, then `SyntaxError: msg`. One from the list above that has no
  position is rendered as the calling frames and the bare message line.

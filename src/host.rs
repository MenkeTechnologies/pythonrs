//! The Python object heap and runtime, reached from fusevm through registered
//! builtins (`register_builtin`) and the strict numeric hook.
//!
//! pythonrs owns no VM and no JIT: the compiler lowers Python to `fusevm::Chunk`,
//! and every Python-specific operation the VM can't do natively is a builtin call
//! that lands here. Local variables live in `Rc<RefCell>` environments chained
//! parent-to-child, so a nested function captures its enclosing scope by
//! reference (real Python closure cells), while function params stay call-local.
//!
//! Value representation:
//!   - immediate: `Value::Int` (int), `Value::Float` (float), `Value::Bool`
//!     (True/False), `Value::Undef` (None);
//!   - heap `Value::Obj(u32)` handles: str, list, tuple, dict, set, range,
//!     function, class, instance, bound-method, exception, iterator, module,
//!     bignum, complex — the reference types.

use crate::async_rt;
use fusevm::{Chunk, NumOp, VMResult, Value, VM};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// Builtin ids emitted by the compiler and registered on every VM. The compiler
/// (`compiler.rs`) and the handler table (`builtins.rs::install`) must agree on
/// these exactly.
pub mod ops {
    pub const GETLOCAL: u16 = 1; // [name] -> value (LEGB read)
    pub const SETLOCAL: u16 = 2; // [name, value] -> value
    pub const SETGLOBAL: u16 = 3; // [name, value] -> value (a `global` target)
    pub const DELNAME: u16 = 4; // [name]
    pub const GETATTR: u16 = 5; // [recv, name] -> value
    pub const SETATTR: u16 = 6; // [recv, name, value]
    pub const DELATTR: u16 = 7; // [recv, name]
    pub const GETITEM: u16 = 8; // [recv, idx] -> value
    pub const SETITEM: u16 = 9; // [recv, idx, value]
    pub const DELITEM: u16 = 10; // [recv, idx]
    pub const MKSTR: u16 = 11; // [parts...] -> str
    pub const MKLIST: u16 = 12; // [items...] -> list
    pub const MKTUPLE: u16 = 13; // [items...] -> tuple
    pub const MKSET: u16 = 14; // [items...] -> set
    pub const MKDICT: u16 = 15; // [k,v,...] -> dict
    pub const MKSLICE: u16 = 16; // [lo, hi, step] -> slice
    pub const CALL: u16 = 17; // [name, args...] -> resolve name & call
    pub const CALL_KW: u16 = 18; // [name, args..., kwdict]
    pub const CALL_METHOD: u16 = 19; // [recv, name, args...]
    pub const CALL_METHOD_KW: u16 = 20; // [recv, name, args..., kwdict]
    pub const CALL_VALUE: u16 = 21; // [callable, args...]
    pub const CALL_VALUE_KW: u16 = 22; // [callable, args..., kwdict]
    pub const TRUTHY: u16 = 23; // [v] -> Bool (Python truthiness)
    pub const TOSTR: u16 = 24; // [v] -> str via str()
    pub const FORMAT: u16 = 25; // [v, conv(int), spec(str)] -> str (f-string field)
    pub const MKFUNC: u16 = 26; // [func_id, defaults...] -> function
    pub const MKLAMBDA: u16 = 27; // [proc_id, defaults...] -> function
    pub const BUILD_CLASS: u16 = 28; // [name, bases_list, class_func] -> class
    pub const GETITER: u16 = 29; // [iterable] -> iterator (left on stack)
    pub const FORITER: u16 = 30; // peek iterator -> pushes value + Bool(has_next)
    pub const CONTAINS: u16 = 31; // [item, container] -> Bool (`in`)
    pub const IS: u16 = 32; // [a, b] -> Bool (identity)
    pub const RAISE: u16 = 33; // [exc] -> raise
    pub const RERAISE: u16 = 34; // [] -> re-raise the active exception
    pub const SIG_RETURN: u16 = 35; // [v] -> return v from the function
    pub const SIG_BREAK: u16 = 36; // [] -> break
    pub const SIG_CONTINUE: u16 = 37; // [] -> continue
    pub const IMPORT: u16 = 38; // [name] -> module object
    pub const IMPORT_FROM: u16 = 39; // [module, name] -> attribute
    pub const UNPACK: u16 = 40; // [iterable, count, star_index] -> pushes count values
    pub const BINOP: u16 = 41; // [op(int), a, b] -> Python binary op (//, @, etc.)
    pub const GETGLOBAL: u16 = 42; // [name] -> global/builtin (module scope read)
    pub const GETSELF: u16 = 43; // [] -> the current bound self
    pub const ASSERT_FAIL: u16 = 44; // [msg] -> raise AssertionError
    pub const MKEXC: u16 = 45; // [class_name, args...] -> exception instance
    pub const YIELDV: u16 = 46; // [v] -> generator yield (suspends)
    pub const UNARY: u16 = 47; // [op(int), v] -> unary result (~, unary +)
    pub const DBG_LINE: u16 = 48; // [line] -> DAP statement marker (debug only)
    pub const TRY: u16 = 49; // [try_id] -> run a try/except/else/finally block
    pub const DECLARE_GLOBAL: u16 = 50; // [name] -> mark name global in this frame
    pub const DECLARE_NONLOCAL: u16 = 51; // [name] -> mark name nonlocal in this frame
    pub const CALL_EX: u16 = 52; // [name, args_list, kwargs_dict] -> resolve name & call
    pub const CALL_VALUE_EX: u16 = 53; // [callable, args_list, kwargs_dict]
    pub const CALL_METHOD_EX: u16 = 54; // [recv, name, args_list, kwargs_dict]
    pub const BUILD_ARGS: u16 = 55; // [tag,val,...] -> positional list (tag 1 = *spread)
    pub const BUILD_KWARGS: u16 = 56; // [key,val,...] -> kwargs dict (key Undef = **spread)
    pub const MKDICT_EX: u16 = 57; // [tag,a,b,...] -> dict (tag 1 = **spread of a)
    pub const MATCH_SEQ: u16 = 58; // [subject, count, star] -> [elems_list, Bool] | [Bool(false)]
    pub const MATCH_MAP_CHECK: u16 = 59; // [subject] -> Bool (is a mapping)
    pub const MATCH_KEY: u16 = 60; // [subject, key] -> [value, Bool] | [Bool(false)]
    pub const MATCH_MAP_REST: u16 = 61; // [subject, keylist] -> dict of remaining keys
    pub const MATCH_CLASS: u16 = 62; // [subject, class, npos, kwnames...] -> [vals_list, Bool] | [Bool]
    pub const MKBYTES: u16 = 63; // [latin1_str] -> bytes (one byte per code point 0..=255)
    pub const GENRET: u16 = 64; // [iter] -> the exhausted (sub)generator's return value (`yield from`)
    pub const AWAIT: u16 = 65; // [awaitable] -> drive it, suspending the coroutine until it settles
    pub const INPLACE: u16 = 66; // [iop(int), a, b] -> augmented op (`+=`, `|=`, …): in-place dunder / mutate, else binary fallback
    pub const WITH_EXIT: u16 = 67; // [mgr] -> call `mgr.__exit__` with the active exception triple; -> Bool(suppress)
    pub const YIELD_FROM: u16 = 68; // [iterable] -> `yield from` delegation (PEP 380); -> sub-iterator's return value
    pub const LOOP_BODY: u16 = 69; // [try_id] -> run a loop body chunk (whose break/continue cross a try/with boundary); consume Break/Continue signals -> Int(0=next, 1=break); Return stops the loop chunk
    pub const DISPLAYHOOK: u16 = 70; // [v] -> interactive REPL echo: if v is not None, print repr(v) and bind `_` (CPython sys.displayhook)
                                     // Chunked-build extends for collection literals whose element count exceeds
                                     // the u8 argc cap of `CallBuiltin`. The first ≤255-slot chunk is built with
                                     // the matching `MK*` op; each further chunk folds into the accumulator that
                                     // sits beneath it on the stack (mirrors CPython's LIST_EXTEND / SET_UPDATE /
                                     // DICT_UPDATE / BUILD_STRING). Each pops [acc, items...] and pushes acc.
    pub const EXTEND_LIST: u16 = 71; // [list, items...] -> list (append items)
    pub const EXTEND_TUPLE: u16 = 72; // [tuple, items...] -> tuple (acc ++ items)
    pub const EXTEND_SET: u16 = 73; // [set, items...] -> set (add items)
    pub const EXTEND_DICT: u16 = 74; // [dict, k,v,...] -> dict (insert pairs)
    pub const EXTEND_STR: u16 = 75; // [str, parts...] -> str (concat parts)
}

/// In-place (augmented-assignment) op tags carried by `ops::INPLACE`. One per
/// `BinOp`, in `BinOp` declaration order; `b_inplace` maps each to its `__i*__`
/// dunder and its binary fallback.
pub mod iop {
    pub const ADD: i64 = 0; // +=
    pub const SUB: i64 = 1; // -=
    pub const MUL: i64 = 2; // *=
    pub const DIV: i64 = 3; // /=
    pub const FLOORDIV: i64 = 4; // //=
    pub const MOD: i64 = 5; // %=
    pub const POW: i64 = 6; // **=
    pub const MATMUL: i64 = 7; // @=
    pub const BITAND: i64 = 8; // &=
    pub const BITOR: i64 = 9; // |=
    pub const BITXOR: i64 = 10; // ^=
    pub const SHL: i64 = 11; // <<=
    pub const SHR: i64 = 12; // >>=
}

/// Binary-op tags carried by `ops::BINOP` (the non-native operators).
pub mod binop {
    pub const DIV: i64 = 0; // /
    pub const FLOORDIV: i64 = 1; // //
    pub const MOD: i64 = 2; // %
    pub const POW: i64 = 3; // **
    pub const MATMUL: i64 = 4; // @
    pub const BITAND: i64 = 5; // &
    pub const BITOR: i64 = 6; // |
    pub const BITXOR: i64 = 7; // ^
    pub const SHL: i64 = 8; // <<
    pub const SHR: i64 = 9; // >>
}

/// Unary-op tags carried by `ops::UNARY`.
pub mod unop {
    pub const INVERT: i64 = 0; // ~
    pub const POS: i64 = 1; // unary +
}

/// The `%`-conversion flags parsed once and threaded into `format_conv`: `+`
/// (force sign), space (leading space on non-negatives), `#` (alternate form).
#[derive(Clone, Copy)]
struct ConvFlags {
    plus: bool,
    space: bool,
    hash: bool,
}

// ── heap objects ───────────────────────────────────────────────────────────

/// A key usable in a dict/set: Python hashes by value for the immutable types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PKey {
    None,
    Int(i64),
    /// Integer outside the `i64` range (a normalized `BigInt`). Never overlaps
    /// `Int`, since `norm_big` collapses any in-range bignum back to `Int`.
    Big(num_bigint::BigInt),
    /// A non-integral float. Integral floats normalize to `Int`/`Big` so that
    /// `1`, `1.0`, and `True` share one key (`1.0 in {1}` → True).
    FloatBits(u64),
    /// A `complex` with a non-zero imaginary part (real+zero-imag complex keys
    /// normalize to the matching real key so `complex(1,0)` unifies with `1`).
    Complex(u64, u64),
    Str(String),
    /// An immutable `bytes` key (a `bytearray` is mutable and stays unhashable).
    Bytes(Vec<u8>),
    Tuple(Vec<PKey>),
    /// A `frozenset` key: the element keys sorted+deduped into a canonical order,
    /// so two equal frozensets (any insertion order) share one key.
    Frozenset(Vec<PKey>),
    /// A user-instance key. `hash` is the value's `__hash__()` result (or the
    /// heap id for the default identity hash); `id` is the heap id of the object
    /// this key is *equal to* (its own, or a value-equal existing key it collapsed
    /// onto — see `prepare_key`). Two keys are the same dict/set slot iff both the
    /// hash and the collapsed id match, giving identity semantics by default and
    /// value semantics when the class defines `__hash__` + `__eq__`.
    Instance {
        hash: i64,
        id: u32,
    },
    /// A type object (`PyObj::Class` or a builtin type/function) used as a key.
    /// Types are conceptual singletons by name, so they key by name — matching
    /// `is`/`==` on classes (`{int: 1}[int]`, `{C: 1}` for a user class `C`).
    Class(String),
}

/// A compiled function template: parameter shape + body chunk. Shared by every
/// closure created from the same `def`/`lambda`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FuncDef {
    pub name: String,
    /// The qualified name (`__qualname__` / `co_qualname`): the dotted path from
    /// the module scope, e.g. `f`, `C.m`, `outer.<locals>.inner`. Defaults to
    /// `name` for bytecode predating this field.
    #[serde(default)]
    pub qualname: String,
    /// Positional-or-keyword parameter names, in order.
    pub params: Vec<String>,
    /// How many leading `params` are positional-only (before a `/`). These
    /// cannot be passed by keyword.
    #[serde(default)]
    pub posonly: usize,
    /// How many trailing `params` have defaults.
    pub ndefaults: usize,
    pub star: Option<String>,
    pub kwonly: Vec<String>,
    /// Which kwonly params are required (no default).
    pub kwonly_required: Vec<bool>,
    pub kwargs: Option<String>,
    pub chunk: Chunk,
    /// Names that are *local* to this function scope: assigned somewhere in the
    /// body (not declared `global`/`nonlocal`). Reading one before it is bound is
    /// an `UnboundLocalError`, never an LEGB fall-through to an enclosing/global
    /// binding — CPython decides this at compile time, so we carry the set here.
    #[serde(default)]
    pub locals: Vec<String>,
    /// True if the body contains a `yield` (a generator function).
    pub is_generator: bool,
    /// True for an `async def`: calling it builds a coroutine object (the body
    /// does NOT run) which the asyncio event loop drives.
    #[serde(default)]
    pub is_async: bool,
}

/// A compiled lambda/comprehension body (same shape, unnamed).
pub type ProcDef = FuncDef;

/// A compiled `try`/`except`/`else`/`finally` block. Bodies are bare chunks run
/// in the *current* scope (so assignments persist), not fresh frames.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TryDef {
    pub body: Chunk,
    /// `(type_chunk, as_name, handler_body)` per `except` clause. A `None`
    /// type_chunk is a bare `except:` (catches everything).
    pub handlers: Vec<(Option<Chunk>, Option<String>, Chunk)>,
    pub orelse: Option<Chunk>,
    pub finalbody: Option<Chunk>,
}

/// A class definition: name, base class names, and its own methods/attrs.
#[derive(Clone, Default)]
pub struct ClassDef {
    pub name: String,
    /// The qualified name (`__qualname__`): the dotted lexical path, e.g. `C`,
    /// `A.B`, `f.<locals>.C`. Empty falls back to `name` (a top-level class).
    pub qualname: String,
    pub bases: Vec<String>,
    /// The class namespace populated by running the class body.
    pub ns: IndexMap<String, Value>,
    /// The C3-ish MRO (this class first), by name.
    pub mro: Vec<String>,
    /// The metaclass name (`type(cls)`). `"type"` for an ordinary class; a user
    /// metaclass name for `class A(metaclass=M)`.
    pub metaclass: String,
}

/// A live closure value.
#[derive(Clone)]
pub struct FuncVal {
    pub def_id: usize,
    /// Captured lexical environment (enclosing scope chain), for free vars.
    pub env: Option<Env>,
    /// Default values for the trailing positional params.
    pub defaults: Vec<Value>,
    /// Default values for the keyword-only params that have one, in `kwonly`
    /// order (evaluated at def time, like `defaults`).
    pub kwonly_defaults: Vec<Value>,
    /// Bound receiver for a bound method (`instance.method`).
    pub bound: Option<Value>,
    /// Owning class name (for `super()` and method identity).
    pub owner: Option<String>,
}

/// A user-defined class instance. Its attribute storage (`__dict__`) is a real
/// heap [`PyObj::Dict`] referenced by `dict`, exactly as CPython backs an
/// instance with a live dict. So `obj.__dict__` hands back this same handle:
/// identity is stable (`obj.__dict__ is obj.__dict__`), reads reflect current
/// attributes, and `obj.__dict__[k] = v` / `del obj.__dict__[k]` write through
/// to the instance. A fully `__slots__`-restricted instance has no dict.
#[derive(Clone)]
pub struct Instance {
    pub class: String,
    pub dict: Value,
    /// For a subclass of a builtin type (`class Stack(list)`, `class C(int)`),
    /// the native heap object / value holding the inherited builtin payload
    /// (the list storage, the int value, …). `Value::Undef` for a plain
    /// `object` subclass. Builtin operations (`len`, `[]`, `+`, iteration,
    /// `repr`, inherited methods) delegate to this when the subclass does not
    /// override the corresponding dunder. See [`PyHost::builtin_base_of`].
    pub payload: Value,
}

/// A heap object.
#[derive(Clone)]
pub enum PyObj {
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Dict(IndexMap<PKey, (Value, Value)>),
    Set(IndexMap<PKey, Value>),
    /// An immutable, hashable `frozenset`. Same storage as `Set`, but usable as
    /// a dict key / set member (see `PKey::Frozenset`) and immutable.
    Frozenset(IndexMap<PKey, Value>),
    /// A live `dict_keys`/`dict_values`/`dict_items` view. Holds a handle to the
    /// backing dict (not a snapshot), so it reflects later mutations. `kind`:
    /// 0 = keys, 1 = values, 2 = items.
    DictView {
        dict: Value,
        kind: u8,
    },
    Range {
        start: i64,
        stop: i64,
        step: i64,
    },
    Slice {
        lo: Value,
        hi: Value,
        step: Value,
    },
    Func(FuncVal),
    /// A first-class reference to a builtin function (`len`, `print`, …).
    Builtin(String),
    Class(String),
    Instance(Instance),
    BoundMethod {
        recv: Value,
        func: Value,
    },
    Exception {
        class: String,
        args: Vec<Value>,
    },
    /// A live iterator over a heap object, with a cursor.
    Iter(IterState),
    /// A lazy `zip(*iterables[, strict])` iterator. `sources` are pre-made
    /// iterators (one per argument); each step pulls one item from each and
    /// yields a tuple, stopping at the shortest (or, with `strict`, raising on a
    /// length mismatch). `done` latches exhaustion so it never re-yields.
    Zip {
        sources: Vec<Value>,
        strict: bool,
        done: bool,
    },
    /// A lazy `map(func, *iterables)` iterator.
    MapObj {
        func: Value,
        sources: Vec<Value>,
        done: bool,
    },
    /// A lazy `filter(func, iterable)` iterator (`func` = `Undef` → identity).
    FilterObj {
        func: Value,
        source: Value,
        done: bool,
    },
    /// A lazy `enumerate(iterable, start)` iterator; `next` is the running index.
    EnumerateObj {
        source: Value,
        next: i64,
        done: bool,
    },
    /// The two-argument `iter(callable, sentinel)` form: call `func()` with no
    /// arguments on each step, yielding the result until it equals `sentinel`
    /// (by `==`), at which point the iterator is exhausted. `done` latches.
    CallIter {
        func: Value,
        sentinel: Value,
        done: bool,
    },
    Module {
        name: String,
        ns: IndexMap<String, Value>,
    },
    BigInt(num_bigint::BigInt),
    Complex(f64, f64),
    /// A live generator (from a `def` with `yield`, or a generator expression),
    /// backed by a stackful `corosensei` coroutine in `PyHost.generators`.
    Generator {
        id: u32,
    },
    /// An `asyncio` Future or Task, backed by a `FutureCell` in the async
    /// runtime side-table (`crate::async_rt`). A Task additionally drives a
    /// coroutine; both settle to a result or exception and fire done-callbacks.
    Future {
        id: u32,
    },
    /// The singleton asyncio event loop object (`get_event_loop()`), a thin
    /// handle over the native ready-queue + timer-heap runtime.
    EventLoop,
    /// An `asyncio` synchronization primitive (`Event`/`Lock`/`Queue`), backed by
    /// a cell in the async runtime side-table (`crate::async_rt`).
    AsyncObj {
        id: u32,
    },
    /// A mutable byte string (`bytearray`). Held inline (a plain `Vec<u8>`),
    /// unlike the immutable [`PyObj::Bytes`].
    Bytearray(Vec<u8>),
    /// A `memoryview` over a `bytes`/`bytearray` buffer. Holds a handle to the
    /// backing object (not a snapshot), so a view over a `bytearray` reflects
    /// later mutations. `start`/`len` bound the (possibly sliced) window;
    /// `readonly` is true for a `bytes` backing. A faithful 1-D unsigned-byte
    /// (`format 'B'`, `itemsize 1`) subset.
    Memoryview {
        obj: Value,
        start: usize,
        len: usize,
        readonly: bool,
    },
    /// An open file / standard stream. Holds only an index into
    /// `PyHost.io_handles`; the underlying `std::fs::File` is neither `Clone`
    /// nor storable inline, so it lives in the side table (ported from
    /// rubylang's `IoCell`).
    File {
        id: u32,
    },
    /// A `collections.deque`: a double-ended queue with an optional bound.
    Deque {
        items: VecDeque<Value>,
        maxlen: Option<usize>,
    },
    /// The class object returned by `collections.namedtuple(name, fields)`. A
    /// callable that constructs `PyObj::Tuple` instances tagged in
    /// `PyHost.nt_meta` so their fields resolve by name.
    NamedTupleType {
        type_name: String,
        fields: Vec<String>,
    },
    /// A `functools.partial`: a callable that pre-binds positional/keyword args
    /// over an arbitrary callable. Handled directly by [`invoke`].
    Partial {
        func: Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    /// A `functools.lru_cache`-wrapped callable. The memo table lives out of
    /// line in `PyHost.lru_caches` (indexed by `cache_id`) so cloning the heap
    /// object never copies — or forks — the cache.
    LruCache {
        func: Value,
        cache_id: u32,
    },
    /// A bound `super` proxy: attribute/method lookup starts in the MRO of
    /// `instance`'s class strictly AFTER `owner` (the defining class), binding
    /// `instance` as `self`. Built by the `super()` builtin.
    Super {
        owner: String,
        instance: Value,
    },
    /// `@staticmethod`-wrapped function: called with no implicit first argument.
    StaticMethod(Value),
    /// `@classmethod`-wrapped function: called with the class bound as the first
    /// argument (`cls`).
    ClassMethod(Value),
    /// A `property` descriptor. Each accessor is `Value::Undef` when unset. A
    /// property is a *data* descriptor (it defines `__set__`/`__delete__`), so it
    /// takes priority over an instance `__dict__` entry of the same name.
    Property {
        fget: Value,
        fset: Value,
        fdel: Value,
    },
    /// The `NotImplemented` singleton: returned by a binary/comparison dunder to
    /// signal "this operand pair is not my business", so the interpreter tries the
    /// reflected operation (and, for `==`/`!=`, falls back to identity).
    NotImplemented,
    /// A live CPython object owned by the `stdlib-ffi` bridge — a handle (index)
    /// into `crate::ffi`'s side-table. Any object the real CPython stdlib returns
    /// that pythonrs can't represent by value (compiled regex, `datetime`,
    /// sockets, iterators, module objects, …) is carried this way; attribute
    /// access / calls / indexing / iteration / `len` / `str` / membership route
    /// back through `crate::ffi`.
    #[cfg(feature = "stdlib-ffi")]
    Foreign(u32),
}

/// The plan for reading `recv.name` when a descriptor may be involved. Computed
/// under a host borrow by [`PyHost::plan_attr_get`], then executed *without* one
/// (the accessor runs user code, which re-enters the host).
pub enum AttrGet {
    /// No descriptor — resolve via [`PyHost::get_attr`].
    Plain,
    /// A `property`: invoke `fget(inst)`, or raise if `fget` is unset. `owner`
    /// is the class in the MRO that defines the property (for `super()` inside
    /// the accessor).
    Property {
        fget: Value,
        inst: Value,
        owner: Option<String>,
    },
    /// A user descriptor: call `desc.__get__(inst, cls)`.
    Descriptor {
        desc: Value,
        inst: Value,
        cls: Value,
    },
}

/// The plan for `recv.name = val` when a descriptor may intercept it.
pub enum AttrSet {
    /// No descriptor — store via [`PyHost::set_attr`].
    Plain,
    /// A `property`: invoke `fset(inst, val)`, or raise if `fset` is unset.
    /// `owner` is the defining class (for `super()` inside the setter).
    Property {
        fset: Value,
        inst: Value,
        val: Value,
        owner: Option<String>,
    },
    /// A user data descriptor: call `desc.__set__(inst, val)`.
    Descriptor {
        desc: Value,
        inst: Value,
        val: Value,
    },
}

/// The plan for `del recv.name` when a descriptor may intercept it.
pub enum AttrDel {
    /// No descriptor — remove from the instance dict via [`PyHost::del_attr`].
    Plain,
    /// A `property`: invoke `fdel(inst)`, or raise if `fdel` is unset. `owner`
    /// is the defining class (for `super()` inside the deleter).
    Property {
        fdel: Value,
        inst: Value,
        owner: Option<String>,
    },
    /// A user data descriptor: call `desc.__delete__(inst)`. `has_delete` is
    /// false when the class attribute is a data descriptor (defines `__set__`)
    /// yet lacks `__delete__` — CPython then raises `AttributeError: __delete__`.
    Descriptor {
        desc: Value,
        inst: Value,
        has_delete: bool,
    },
}

/// Iterator cursor state.
#[derive(Clone)]
pub enum IterState {
    Seq { items: Vec<Value>, idx: usize },
    RangeIter { cur: i64, stop: i64, step: i64 },
    DictKeys { keys: Vec<Value>, idx: usize },
}

// ── I/O side table ───────────────────────────────────────────────────────────

/// One live file / standard stream, indexed by `PyObj::File.id`. Slots 0/1/2 are
/// always `Stdout`/`Stderr`/`Stdin`. A `File` holds the owned `std::fs::File`
/// (`None` once closed), the path (for `repr`), and whether it was opened for
/// reading and/or writing. `std::fs::File` is not `Clone`, so — like rubylang's
/// `IoCell` — the handle lives here, never inline in a `PyObj`.
pub enum IoCell {
    Stdout,
    Stderr,
    Stdin,
    File {
        file: Option<std::fs::File>,
        path: String,
        readable: bool,
        writable: bool,
    },
}

// ── collections side tables ──────────────────────────────────────────────────

/// Which `dict` subclass a `PyObj::Dict` heap object actually is. A plain dict
/// has no entry in `PyHost.dict_meta`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DictKind {
    Counter,
    DefaultDict,
    OrderedDict,
}

/// Metadata tagging a `PyObj::Dict` as a `collections` dict subclass. `factory`
/// is the `defaultdict` `default_factory` (a callable or `None`).
#[derive(Clone)]
pub struct DictMeta {
    pub kind: DictKind,
    pub factory: Option<Value>,
}

/// Metadata tagging a `PyObj::Tuple` as a `namedtuple` instance: its type name
/// and ordered field names, so `.field` access resolves to a tuple index.
#[derive(Clone)]
pub struct NtMeta {
    pub type_name: String,
    pub fields: Vec<String>,
}

/// The memo table behind one `functools.lru_cache`-wrapped callable, indexed by
/// `PyObj::LruCache.cache_id`. `order` records insertion order for eviction when
/// `maxsize` is set (`None` == unbounded). Keys are the hashable arg tuple.
pub struct LruData {
    pub map: IndexMap<PKey, Value>,
    pub order: VecDeque<PKey>,
    pub maxsize: Option<usize>,
    pub hits: u64,
    pub misses: u64,
}

// ── environments ─────────────────────────────────────────────────────────────

/// A local-variable environment, shared (by `Rc`) between a frame and any nested
/// function that captures it.
pub struct EnvData {
    pub vars: IndexMap<String, Value>,
    pub parent: Option<Env>,
}
pub type Env = Rc<RefCell<EnvData>>;

fn new_env(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(EnvData {
        vars: IndexMap::new(),
        parent,
    }))
}

/// One function activation.
pub struct Frame {
    pub env: Env,
    pub globals_decl: HashSet<String>,
    /// Names declared `nonlocal` in this frame — writes target the nearest
    /// enclosing function scope that binds the name, not the local env.
    pub nonlocals_decl: HashSet<String>,
    /// Names local to this function scope (see `FuncDef::locals`). A read of a
    /// name in this set resolves ONLY in `env`; if absent it is an
    /// `UnboundLocalError`, not an LEGB fall-through. Empty for the module frame
    /// and class-body frames (whose reads stay dynamic, giving `NameError`).
    pub locals_set: HashSet<String>,
    /// True for a class-body frame. A class scope is NOT an enclosing scope for
    /// nested functions (methods, comprehensions), so a closure defined here
    /// captures the class body's PARENT env, skipping the class namespace.
    pub is_class_body: bool,
    pub self_obj: Option<Value>,
    pub owner: Option<String>,
    /// The scope name shown in a traceback frame (`<module>`, a function name, or
    /// a class name for a class body).
    pub name: String,
    /// Source line currently executing in this frame — updated by the DAP debug
    /// line hook (`--dap`) and by the error path when an exception aborts a chunk.
    pub line: u32,
}

/// A non-local control signal.
#[derive(Clone)]
pub enum Signal {
    Return(Value),
    Break,
    Continue,
}

/// The Python runtime.
pub struct PyHost {
    heap: Vec<PyObj>,
    /// Function/lambda templates, indexed by def id.
    pub funcs: Vec<FuncDef>,
    /// Class templates by name.
    pub classes: IndexMap<String, ClassDef>,
    /// try/except/finally block templates, indexed by try id.
    pub tries: Vec<TryDef>,
    /// Module-level (global) names.
    globals: IndexMap<String, Value>,
    /// The frame stack (bottom = module).
    frames: Vec<Frame>,
    pub error: Option<String>,
    /// The in-flight exception object, if any.
    pub exc: Option<Value>,
    pub signal: Option<Signal>,
    /// A duplicate keyword key detected while merging a call's `**mapping`
    /// spreads (set by `BUILD_KWARGS`, consumed by the `CALL_*_EX` handlers so
    /// the raised `TypeError` can name the callable). `f(**a, **b)` with a shared
    /// key, or `f(k=v, **{'k': ...})`, is an error in CPython even though a plain
    /// `{**a, **b}` dict display silently keeps the last value.
    pub pending_kw_dup: Option<String>,
    /// Suspended generator coroutines, indexed by `PyObj::Generator.id`.
    generators: Vec<GenCell>,
    /// Live file / standard-stream objects, indexed by `PyObj::File.id`. Slots
    /// 0/1/2 are stdout/stderr/stdin.
    io_handles: Vec<IoCell>,
    /// `dict` subclass tags, keyed by the `PyObj::Dict` heap index. Absent for a
    /// plain dict.
    pub dict_meta: HashMap<u32, DictMeta>,
    /// `namedtuple` instance tags, keyed by the `PyObj::Tuple` heap index.
    pub nt_meta: HashMap<u32, NtMeta>,
    /// `lru_cache` memo tables, indexed by `PyObj::LruCache.cache_id`.
    lru_caches: Vec<LruData>,
    /// Exception chaining links, keyed by the exception object's heap index.
    /// `.0` = `__cause__` (`raise X from Y`), `.1` = `__context__` (the
    /// exception being handled when this one was raised). `Value::Undef` = unset.
    pub exc_links: HashMap<u32, (Value, Value)>,
    /// Process arguments exposed to the program as `sys.argv`. Set once per run
    /// by `init_runtime` (`['']` for the REPL/stdin default, `['script', …]` for
    /// a file, `['-c', …]` for `-c`).
    pub argv: Vec<String>,
    /// Absolute path bound to the top-level `__file__`, `None` for `-c`/stdin.
    pub main_file: Option<String>,
    /// The full program source — used to reconstruct traceback source lines.
    pub prog_source: String,
    /// The filename shown in traceback frames (`<string>`, `<stdin>`, or a path).
    pub tb_filename: String,
    /// Whether traceback frames print their source line (true for a file / `-c`,
    /// false for stdin — CPython cannot retrieve stdin source).
    pub tb_show_source: bool,
    /// Frames captured (innermost first) as an exception unwinds the call stack,
    /// each `(scope_name, line)`. Cleared when the exception is caught.
    pub traceback: Vec<(String, u32)>,
}

/// Whether a `GenCell` backs a plain generator, an `async def` coroutine, or
/// an async generator (`async def` containing `yield`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GenKind {
    Generator,
    Coroutine,
    AsyncGen,
}

/// The pending operation an async generator's next drive should perform. Set by
/// `agen.asend(v)` / `agen.athrow(exc)` / `agen.aclose()` (and by `__anext__`,
/// which defaults to `Send(None)`); consumed by `async_rt::drive_async_gen`.
#[derive(Clone)]
pub enum AGenOp {
    /// `asend(v)` / `__anext__`: resume the body sending `v` to the current
    /// `yield`, returning the next produced value (or `StopAsyncIteration`).
    Send(Value),
    /// `athrow(exc)`: raise `exc` at the current `yield` point.
    Throw(Value),
    /// `aclose()`: raise `GeneratorExit`, expecting the body to finish.
    Close,
}

/// One suspended generator. `coro` is `None` only while this generator is
/// actively running (taken out across `Coroutine::resume`); `ctx` holds its
/// volatile execution context (frames/signal/error/exc) while suspended.
struct GenCell {
    /// Whether this cell backs a plain generator or an `async def` coroutine
    /// (drives `type().__name__` and `repr`, and gates `next()`/`for`).
    kind: GenKind,
    coro: Option<corosensei::Coroutine<Value, Value, Result<Value, String>>>,
    /// Raw pointer to the coroutine body's `Yielder`, published on entry (same
    /// thread → valid for the body's life). Read by `yield` to suspend.
    yielder: *const (),
    ctx: GenContext,
    done: bool,
    /// Whether the body has run past its first resume (a fresh generator only
    /// accepts `send(None)` / `next()`).
    started: bool,
    /// An exception queued by `.throw()`/`.close()` to raise at the current
    /// `yield` point on the next resume.
    pending_throw: Option<Value>,
    /// The value the body `return`ed (carried by `StopIteration.value` and by
    /// `yield from`). `Undef` for a plain fall-off-the-end return.
    ret_value: Value,
    /// For an async generator: whether the most recent suspension was an `await`
    /// (yielding a Future to the loop) rather than a `yield` (producing a value).
    /// Read by the async-gen `__anext__` driver to tell the two apart.
    awaiting: bool,
    /// For an async generator: the operation `asend`/`athrow`/`aclose` queued on
    /// the awaitable, consumed by the next `drive_async_gen` (`None` = `__anext__`,
    /// i.e. `Send(None)`).
    agen_op: Option<AGenOp>,
    /// The defining function's name (used by the un-awaited-coroutine warning).
    func_name: String,
}

/// The mutable "execution registers" swapped at every generator resume/suspend
/// boundary so a suspended generator's half-finished frame/signal state never
/// leaks into the resuming caller (and vice-versa). The object heap, function
/// table, classes, tries and globals are shared and never swapped.
#[derive(Default)]
struct GenContext {
    frames: Vec<Frame>,
    error: Option<String>,
    exc: Option<Value>,
    signal: Option<Signal>,
}

thread_local! {
    /// Id of the generator whose body is currently executing on this thread, or
    /// `None` at the root. `yield` suspends this generator.
    static CUR_GEN: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    static HOST: RefCell<PyHost> = RefCell::new(PyHost::new());
}

thread_local! {
    /// Resolved dict/set keys for user instances (heap id → `PKey::Instance`),
    /// computed by [`prepare_key`] *outside* any host borrow (running `__hash__`/
    /// `__eq__`), then read by the borrowed [`PyHost::to_key`] which cannot itself
    /// run user code. A single container op prepares its instance key(s) here just
    /// before the borrowed access and clears them right after.
    static PENDING_KEY: RefCell<HashMap<u32, PKey>> = RefCell::new(HashMap::new());
}

/// Insert a resolved instance key for `id` into the pending-key table.
fn pending_key_set(id: u32, key: PKey) {
    PENDING_KEY.with(|p| p.borrow_mut().insert(id, key));
}

/// Read (without removing) the resolved instance key for `id`, if prepared.
fn pending_key_get(id: u32) -> Option<PKey> {
    PENDING_KEY.with(|p| p.borrow().get(&id).cloned())
}

/// Drop all pending instance keys (called at the end of a container op).
fn pending_key_clear() {
    PENDING_KEY.with(|p| p.borrow_mut().clear());
}

/// Run `f` with mutable access to the thread-local host.
pub fn with_host<R>(f: impl FnOnce(&mut PyHost) -> R) -> R {
    HOST.with(|h| f(&mut h.borrow_mut()))
}

/// Reset the host to a clean slate (fresh module frame).
pub fn reset_host() {
    with_host(|h| *h = PyHost::new());
    async_rt::reset();
}

/// Install the per-run CLI/runtime context on a freshly reset host: `sys.argv`,
/// the top-level `__name__`/`__file__` globals, and the traceback source/filename
/// metadata. Call after `reset_host`, before running the program.
pub fn init_runtime(
    argv: Vec<String>,
    main_file: Option<String>,
    source: &str,
    tb_filename: &str,
    tb_show_source: bool,
) {
    with_host(|h| {
        h.argv = argv;
        h.main_file = main_file.clone();
        h.prog_source = source.to_string();
        h.tb_filename = tb_filename.to_string();
        h.tb_show_source = tb_show_source;
        h.traceback.clear();
        // The top-level script always runs as `__main__`.
        let name = h.new_str("__main__");
        h.set_global("__name__", name);
        if let Some(path) = main_file {
            let f = h.new_str(path);
            h.set_global("__file__", f);
        }
    });
}

impl Default for PyHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PyHost {
    pub fn new() -> PyHost {
        let module_env = new_env(None);
        PyHost {
            heap: Vec::new(),
            funcs: Vec::new(),
            classes: IndexMap::new(),
            tries: Vec::new(),
            globals: IndexMap::new(),
            frames: vec![Frame {
                env: module_env,
                globals_decl: HashSet::new(),
                nonlocals_decl: HashSet::new(),
                locals_set: HashSet::new(),
                is_class_body: false,
                self_obj: None,
                owner: None,
                name: "<module>".to_string(),
                line: 0,
            }],
            error: None,
            exc: None,
            signal: None,
            pending_kw_dup: None,
            generators: Vec::new(),
            io_handles: vec![IoCell::Stdout, IoCell::Stderr, IoCell::Stdin],
            dict_meta: HashMap::new(),
            nt_meta: HashMap::new(),
            lru_caches: Vec::new(),
            exc_links: HashMap::new(),
            argv: vec![String::new()],
            main_file: None,
            prog_source: String::new(),
            tb_filename: "<string>".to_string(),
            tb_show_source: true,
            traceback: Vec::new(),
        }
    }

    /// Record `__cause__`/`__context__` for an exception object. `Undef` leaves
    /// a slot unset. Merges with any existing links (a later implicit
    /// `__context__` must not clobber an explicit `__cause__`).
    pub fn set_exc_link(&mut self, exc: &Value, cause: Value, context: Value) {
        if let Value::Obj(i) = exc {
            let slot = self
                .exc_links
                .entry(*i)
                .or_insert((Value::Undef, Value::Undef));
            if !matches!(cause, Value::Undef) {
                slot.0 = cause;
            }
            if !matches!(context, Value::Undef) {
                slot.1 = context;
            }
        }
    }

    /// Read `__cause__` (`.0`) / `__context__` (`.1`) for an exception object.
    pub fn exc_link(&self, exc: &Value) -> (Value, Value) {
        if let Value::Obj(i) = exc {
            if let Some(links) = self.exc_links.get(i) {
                return links.clone();
            }
        }
        (Value::Undef, Value::Undef)
    }

    // ── program loading ──────────────────────────────────────────────────
    /// `(func_offset, try_offset)` — the base ids a freshly compiled program's
    /// func/try references must be rebased above so they never alias what is
    /// already loaded (REPL lines, `import`).
    pub fn program_offsets(&self) -> (usize, usize) {
        (self.funcs.len(), self.tries.len())
    }
    pub fn load_program(&mut self, funcs: Vec<FuncDef>, tries: Vec<TryDef>) {
        self.funcs.extend(funcs);
        self.tries.extend(tries);
    }
    pub fn try_def(&self, id: usize) -> Option<TryDef> {
        self.tries.get(id).cloned()
    }
    /// The value a (sub)generator `return`ed — its `StopIteration.value`, read by
    /// the `yield from` delegation op.
    pub fn gen_return_value(&self, id: u32) -> Value {
        self.generators
            .get(id as usize)
            .map(|g| g.ret_value.clone())
            .unwrap_or(Value::Undef)
    }

    // ── heap allocation / accessors ──────────────────────────────────────
    pub fn alloc(&mut self, obj: PyObj) -> Value {
        self.heap.push(obj);
        Value::Obj((self.heap.len() - 1) as u32)
    }
    /// A stable pseudo-address for an object (its heap index), used only for the
    /// `<… object at 0x…>` reprs where CPython prints an opaque pointer.
    pub fn addr_of(&self, v: &Value) -> u64 {
        match v {
            Value::Obj(i) => *i as u64,
            _ => 0,
        }
    }

    pub fn get(&self, v: &Value) -> Option<&PyObj> {
        if let Value::Obj(i) = v {
            self.heap.get(*i as usize)
        } else {
            None
        }
    }
    pub fn get_mut(&mut self, v: &Value) -> Option<&mut PyObj> {
        if let Value::Obj(i) = v {
            self.heap.get_mut(*i as usize)
        } else {
            None
        }
    }

    /// The `stdlib-ffi` handle id if `v` is a CPython `Foreign` object, else
    /// `None`. Copying the id out ends the heap borrow before dispatching to the
    /// bridge (which needs `&mut self` to marshal the result back).
    #[cfg(feature = "stdlib-ffi")]
    pub fn foreign_id(&self, v: &Value) -> Option<u32> {
        match self.get(v) {
            Some(PyObj::Foreign(id)) => Some(*id),
            _ => None,
        }
    }

    pub fn new_str(&mut self, s: impl Into<String>) -> Value {
        self.alloc(PyObj::Str(s.into()))
    }
    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(PyObj::List(items))
    }
    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        self.alloc(PyObj::Tuple(items))
    }
    /// A one-shot sequence iterator (for `reversed`, etc.): `next()` walks the
    /// items once, then exhausts.
    pub fn new_iter_seq(&mut self, items: Vec<Value>) -> Value {
        self.alloc(PyObj::Iter(IterState::Seq { items, idx: 0 }))
    }
    pub fn new_dict(&mut self, pairs: IndexMap<PKey, (Value, Value)>) -> Value {
        self.alloc(PyObj::Dict(pairs))
    }

    /// Allocate a class instance with a fresh live `__dict__` (a real
    /// [`PyObj::Dict`]) seeded from `attrs`. Every `PyObj::Instance` must be
    /// built through here so its `dict` field points at heap storage that
    /// `obj.__dict__` can hand back by handle (see [`Instance`]).
    pub fn new_instance(&mut self, class: String, attrs: IndexMap<String, Value>) -> Value {
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::with_capacity(attrs.len());
        for (k, v) in attrs {
            let kv = self.new_str(k.clone());
            d.insert(PKey::Str(k), (kv, v));
        }
        let dict = self.alloc(PyObj::Dict(d));
        self.alloc(PyObj::Instance(Instance {
            class,
            dict,
            payload: Value::Undef,
        }))
    }

    /// Allocate a builtin-subclass instance carrying `payload` (the inherited
    /// native list/dict/str/int/… storage). Attributes start empty.
    pub fn new_instance_payload(&mut self, class: String, payload: Value) -> Value {
        let dict = self.alloc(PyObj::Dict(IndexMap::new()));
        self.alloc(PyObj::Instance(Instance {
            class,
            dict,
            payload,
        }))
    }

    /// The builtin base type in `class`'s MRO (`list`/`dict`/`str`/`int`/…),
    /// making the class a subclass of a builtin type. `None` for a plain
    /// `object` subclass. Walks the MRO so an indirect subclass
    /// (`class B(A)` where `A(list)`) is also detected.
    pub fn builtin_base_of(&self, class: &str) -> Option<&'static str> {
        for c in self.mro_of(class) {
            match c.as_str() {
                "list" => return Some("list"),
                "dict" => return Some("dict"),
                "str" => return Some("str"),
                "int" => return Some("int"),
                "float" => return Some("float"),
                "tuple" => return Some("tuple"),
                "set" => return Some("set"),
                "frozenset" => return Some("frozenset"),
                _ => {}
            }
        }
        None
    }

    /// Read instance attribute `name` from a live instance `__dict__` handle.
    pub fn inst_attr(&self, dict: &Value, name: &str) -> Option<Value> {
        match self.get(dict) {
            Some(PyObj::Dict(m)) => m.get(&PKey::Str(name.to_string())).map(|(_, v)| v.clone()),
            _ => None,
        }
    }

    /// Whether a live instance `__dict__` holds `name`.
    pub fn inst_has(&self, dict: &Value, name: &str) -> bool {
        matches!(self.get(dict), Some(PyObj::Dict(m)) if m.contains_key(&PKey::Str(name.to_string())))
    }

    /// Set `name = val` on a live instance `__dict__`, preserving the existing
    /// key object on update (no fresh string alloc) so repr/iteration order is
    /// stable across reassignment, matching CPython dict semantics.
    pub fn inst_attr_set(&mut self, dict: &Value, name: &str, val: Value) {
        let key = PKey::Str(name.to_string());
        if let Some(PyObj::Dict(m)) = self.get(dict) {
            if m.contains_key(&key) {
                if let Some(PyObj::Dict(m)) = self.get_mut(dict) {
                    if let Some(slot) = m.get_mut(&key) {
                        slot.1 = val;
                    }
                }
                return;
            }
        }
        let kv = self.new_str(name.to_string());
        if let Some(PyObj::Dict(m)) = self.get_mut(dict) {
            m.insert(key, (kv, val));
        }
    }

    /// Delete `name` from a live instance `__dict__`; returns whether it existed.
    pub fn inst_attr_del(&mut self, dict: &Value, name: &str) -> bool {
        match self.get_mut(dict) {
            Some(PyObj::Dict(m)) => m.shift_remove(&PKey::Str(name.to_string())).is_some(),
            _ => false,
        }
    }

    /// The attribute names of a live instance `__dict__`, in insertion order.
    pub fn inst_attr_names(&self, dict: &Value) -> Vec<String> {
        match self.get(dict) {
            Some(PyObj::Dict(m)) => m
                .keys()
                .filter_map(|k| match k {
                    PKey::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
    pub fn new_set(&mut self, items: IndexMap<PKey, Value>) -> Value {
        self.alloc(PyObj::Set(items))
    }
    /// A set/frozenset's elements in CPython iteration/`repr` order. For a set
    /// whose every key is a plain machine int this is the open-addressing table
    /// order (`{3, 1, 2}` → `1, 2, 3`); any other element type falls back to
    /// insertion order (CPython randomizes those hashes, so no fixed order can
    /// match byte-for-byte).
    pub fn set_ordered_values(&self, s: &IndexMap<PKey, Value>) -> Vec<Value> {
        let mut hashes = Vec::with_capacity(s.len());
        for k in s.keys() {
            match k {
                PKey::Int(n) => hashes.push(cpython_int_hash(*n)),
                // Not the deterministic subset: keep insertion order.
                _ => return s.values().cloned().collect(),
            }
        }
        let vals: Vec<&Value> = s.values().collect();
        cpython_set_order(&hashes)
            .into_iter()
            .map(|i| vals[i].clone())
            .collect()
    }

    pub fn new_frozenset(&mut self, items: IndexMap<PKey, Value>) -> Value {
        self.alloc(PyObj::Frozenset(items))
    }
    /// A `set` or `frozenset` result, choosing the variant by `frozen` — used by
    /// the set-algebra operators, whose result type follows the left operand.
    pub fn new_setlike(&mut self, items: IndexMap<PKey, Value>, frozen: bool) -> Value {
        if frozen {
            self.alloc(PyObj::Frozenset(items))
        } else {
            self.alloc(PyObj::Set(items))
        }
    }
    /// The backing map of a `set` or `frozenset`, else `None`.
    pub fn setlike(&self, v: &Value) -> Option<&IndexMap<PKey, Value>> {
        match self.get(v) {
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => Some(s),
            _ => None,
        }
    }
    /// Whether `v` is a `frozenset`.
    pub fn is_frozenset(&self, v: &Value) -> bool {
        matches!(self.get(v), Some(PyObj::Frozenset(_)))
    }

    /// The live elements of a `dict_keys`/`dict_values`/`dict_items` view,
    /// materialized (allocating item tuples) from the backing dict at call time
    /// — so the view reflects mutations. `None` if `v` is not a view.
    pub fn view_items(&mut self, v: &Value) -> Option<Vec<Value>> {
        let (dict, kind) = match self.get(v) {
            Some(PyObj::DictView { dict, kind }) => (dict.clone(), *kind),
            _ => return None,
        };
        let pairs: Vec<(Value, Value)> = match self.get(&dict) {
            Some(PyObj::Dict(d)) => d.values().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => vec![],
        };
        Some(
            pairs
                .into_iter()
                .map(|(k, v)| match kind {
                    0 => k,
                    1 => v,
                    _ => self.new_tuple(vec![k, v]),
                })
                .collect(),
        )
    }

    /// A set-map of `v` for the set-algebra operators: `set`/`frozenset`, or a
    /// `dict_keys`/`dict_items` view coerced to a key-set. `None` otherwise
    /// (a `dict_values` view has no set algebra).
    pub fn setmap_of(&mut self, v: &Value) -> Option<IndexMap<PKey, Value>> {
        if let Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) = self.get(v) {
            return Some(s.clone());
        }
        let kind = match self.get(v) {
            Some(PyObj::DictView { kind, .. }) if *kind == 0 || *kind == 2 => *kind,
            _ => return None,
        };
        let items = self.view_items(v)?;
        let mut out: IndexMap<PKey, Value> = IndexMap::new();
        for it in items {
            if let Ok(k) = self.to_key(&it) {
                out.insert(k, it);
            }
        }
        let _ = kind;
        Some(out)
    }

    pub fn as_str(&self, v: &Value) -> Option<String> {
        match self.get(v) {
            Some(PyObj::Str(s)) => Some(s.clone()),
            // A `str` subclass instance coerces through its native payload.
            Some(PyObj::Instance(_)) => self.base_payload_any(v).and_then(|p| self.as_str(&p)),
            _ => None,
        }
    }

    // ── scope / names ────────────────────────────────────────────────────
    fn frame(&self) -> &Frame {
        self.frames.last().unwrap()
    }
    fn cur_env(&self) -> Env {
        self.frame().env.clone()
    }

    // ── DAP debug introspection (used only under `--dap`) ────────────────────
    /// Number of active call frames (the debugger's step-depth reference).
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }
    /// Record the source line the innermost frame is executing (DAP line hook).
    pub fn set_cur_line(&mut self, line: u32) {
        if let Some(f) = self.frames.last_mut() {
            f.line = line;
        }
    }
    /// Capture the innermost frame's `(name, line)` into the in-flight traceback
    /// as an exception unwinds past it. Called just before the frame is popped.
    pub fn push_tb_frame(&mut self) {
        if let Some(f) = self.frames.last() {
            self.traceback.push((f.name.clone(), f.line));
        }
    }
    /// The call stack as (frame name, line) pairs, innermost first — for the DAP
    /// `stackTrace`. `owner` carries the function/class name where known.
    pub fn dbg_stack(&self) -> Vec<(String, u32)> {
        // `f.name` is the frame's own name (`<module>` for the module frame, the
        // function/method name for a call). `f.owner` is the *defining class* and
        // is `None` for top-level functions, so reporting it collapsed every frame
        // to `<module>` and broke both `stackTrace` names and function breakpoints.
        self.frames
            .iter()
            .rev()
            .map(|f| (f.name.clone(), f.line))
            .collect()
    }
    /// The innermost frame's locals as (name, repr) pairs — for DAP `variables`.
    /// Dunder names are hidden, matching a debugger's default locals view.
    pub fn dbg_locals(&self) -> Vec<(String, String)> {
        let env = self.cur_env();
        let names: Vec<String> = env
            .borrow()
            .vars
            .keys()
            .filter(|k| !k.starts_with("__"))
            .cloned()
            .collect();
        names
            .into_iter()
            .map(|n| {
                let v = self.read_name(&n).unwrap_or(Value::Undef);
                (n, self.repr_of(&v))
            })
            .collect()
    }

    /// LEGB read: local + enclosing chain, then globals. Returns None if unbound
    /// (the caller decides whether it is a builtin or a NameError).
    pub fn read_name(&self, name: &str) -> Option<Value> {
        let mut env = Some(self.cur_env());
        while let Some(e) = env {
            if let Some(v) = e.borrow().vars.get(name) {
                return Some(v.clone());
            }
            env = e.borrow().parent.clone();
        }
        self.globals.get(name).cloned()
    }

    pub fn read_global(&self, name: &str) -> Option<Value> {
        self.globals.get(name).cloned()
    }

    /// `UnboundLocalError`-aware read for a bare-name load. If `name` is a genuine
    /// local of the current frame (in `locals_set`, not declared `global`/
    /// `nonlocal`) it resolves ONLY in the current env: present → its value,
    /// absent → [`NameRead::Unbound`] (an `UnboundLocalError`, never a fall-through
    /// to an enclosing or global binding). Otherwise it is a normal LEGB read.
    pub fn read_name_checked(&self, name: &str) -> NameRead {
        let f = self.frame();
        if f.locals_set.contains(name)
            && !f.globals_decl.contains(name)
            && !f.nonlocals_decl.contains(name)
        {
            return match self.cur_env().borrow().vars.get(name) {
                Some(v) => NameRead::Value(v.clone()),
                None => NameRead::Unbound,
            };
        }
        match self.read_name(name) {
            Some(v) => NameRead::Value(v),
            None => NameRead::Missing,
        }
    }

    /// CPython's callable display for the `**`-merge duplicate-keyword error:
    /// a user function/lambda/class is module-qualified (`__main__.f`), while an
    /// unresolved name (i.e. a builtin like `dict`) stays bare.
    pub fn call_display_name(&self, name: &str) -> String {
        match self.read_name(name).and_then(|v| self.get(&v).cloned()) {
            Some(PyObj::Func(fv)) => {
                let q = self.funcs.get(fv.def_id).map_or(name, |d| d.name.as_str());
                format!("__main__.{q}")
            }
            Some(PyObj::Class(_)) => format!("__main__.{name}"),
            _ => name.to_string(),
        }
    }

    /// Assign to `name` following Python scope rules: a `global`-declared name
    /// (or module scope) writes to globals; otherwise the current local env.
    pub fn set_name(&mut self, name: &str, val: Value) {
        if self.frame().globals_decl.contains(name) {
            self.globals.insert(name.to_string(), val);
            return;
        }
        if self.frame().nonlocals_decl.contains(name) {
            // Rebind the nearest ENCLOSING function scope that binds `name`
            // (skip the current env — that is what distinguishes it from a plain
            // local assignment and from `global`).
            let cur = self.cur_env();
            let mut env = cur.borrow().parent.clone();
            while let Some(e) = env {
                if e.borrow().vars.contains_key(name) {
                    e.borrow_mut().vars.insert(name.to_string(), val);
                    return;
                }
                let parent = e.borrow().parent.clone();
                env = parent;
            }
            // No binding found up the chain: fall back to the immediate parent.
            let parent = cur.borrow().parent.clone();
            if let Some(p) = parent {
                p.borrow_mut().vars.insert(name.to_string(), val);
                return;
            }
        }
        // Module scope is the only env with no parent. Test that, not
        // `frames.len() == 1`: a generator/coroutine runs on an isolated
        // single-frame stack, so the length test would wrongly route its locals
        // to globals (invisible to an `UnboundLocalError`-aware local read).
        let cur = self.cur_env();
        if cur.borrow().parent.is_none() {
            self.globals.insert(name.to_string(), val);
        } else {
            cur.borrow_mut().vars.insert(name.to_string(), val);
        }
    }

    pub fn set_global(&mut self, name: &str, val: Value) {
        self.globals.insert(name.to_string(), val);
    }

    pub fn del_name(&mut self, name: &str) -> Result<(), String> {
        if self
            .cur_env()
            .borrow_mut()
            .vars
            .shift_remove(name)
            .is_some()
        {
            return Ok(());
        }
        if self.globals.shift_remove(name).is_some() {
            return Ok(());
        }
        Err(name_error(name))
    }

    pub fn declare_global(&mut self, name: &str) {
        self.frames
            .last_mut()
            .unwrap()
            .globals_decl
            .insert(name.to_string());
    }

    pub fn declare_nonlocal(&mut self, name: &str) {
        self.frames
            .last_mut()
            .unwrap()
            .nonlocals_decl
            .insert(name.to_string());
    }

    pub fn current_self(&self) -> Option<Value> {
        self.frame().self_obj.clone()
    }
    pub fn current_owner(&self) -> Option<String> {
        self.frame().owner.clone()
    }

    // ── signals / errors ─────────────────────────────────────────────────
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
    pub fn has_signal(&self) -> bool {
        self.signal.is_some() || self.error.is_some()
    }
    pub fn raise_str(&mut self, class: &str, msg: &str) -> String {
        let s = if msg.is_empty() {
            class.to_string()
        } else {
            format!("{class}: {msg}")
        };
        self.error = Some(s.clone());
        s
    }
}

// ── constructors used across modules ─────────────────────────────────────────

pub fn name_error(name: &str) -> String {
    format!("NameError: name '{name}' is not defined")
}
/// CPython's `UnboundLocalError` message (a `NameError` subclass), raised when a
/// function reads a local name before it has been bound.
pub fn unbound_local_error(name: &str) -> String {
    format!(
        "UnboundLocalError: cannot access local variable '{name}' where it is not associated with a value"
    )
}

/// Outcome of an `UnboundLocalError`-aware bare-name read (see
/// [`PyHost::read_name_checked`]).
pub enum NameRead {
    /// The name resolved to this value.
    Value(Value),
    /// A genuine local read before binding → `UnboundLocalError`.
    Unbound,
    /// Not found in any scope → the caller falls back to builtins / `NameError`.
    Missing,
}
pub fn type_error(msg: &str) -> String {
    format!("TypeError: {msg}")
}

/// Callable display for the `**`-merge duplicate-keyword error when the callee
/// is an already-evaluated value (the `CALL_VALUE_EX` path): a user function is
/// module-qualified like CPython, anything else falls back to `<callable>`.
pub fn callable_display_name(callable: &Value) -> String {
    with_host(|h| match h.get(callable) {
        Some(PyObj::Func(fv)) => {
            let q = h
                .funcs
                .get(fv.def_id)
                .map_or("<callable>", |d| d.name.as_str());
            format!("__main__.{q}")
        }
        _ => "<callable>".to_string(),
    })
}

/// The CPython version pythonrs emulates byte-for-byte. `sys.version`/
/// `sys.version_info` report this rather than pythonrs's own crate version.
pub const PY_MAJOR: i64 = 3;
pub const PY_MINOR: i64 = 14;
pub const PY_MICRO: i64 = 6;

/// CPython's `sys.platform` string for the host OS (`darwin`/`linux`/…), mapped
/// from Rust's `std::env::consts::OS`.
pub fn py_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

// ── the fusevm run plumbing ──────────────────────────────────────────────────

thread_local! {
    static DEBUG_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable DAP debug execution.
pub fn set_debug_mode(on: bool) {
    DEBUG_MODE.with(|d| d.set(on));
}

thread_local! {
    /// Object ids currently mid-`repr` — CPython's `Py_ReprEnter`/`Py_ReprLeave`
    /// stack. A container that (directly or transitively) contains itself would
    /// otherwise recurse forever; instead the inner re-entry emits `[...]`/`{...}`.
    static REPR_GUARD: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Begin repr-ing container `id`. Returns `true` if `id` is ALREADY on the repr
/// stack (a reference cycle) — the caller must then emit the recursion marker and
/// NOT recurse. Returns `false` after recording `id`; pair every `false` with a
/// matching `repr_guard_leave(id)`.
pub fn repr_guard_enter(id: u32) -> bool {
    REPR_GUARD.with(|g| {
        let mut g = g.borrow_mut();
        if g.contains(&id) {
            true
        } else {
            g.push(id);
            false
        }
    })
}

/// End repr-ing container `id` (pops the most recent matching entry).
pub fn repr_guard_leave(id: u32) {
    REPR_GUARD.with(|g| {
        let mut g = g.borrow_mut();
        if let Some(pos) = g.iter().rposition(|&x| x == id) {
            g.remove(pos);
        }
    });
}

/// Register every pythonrs builtin + the numeric hook on a VM, then run it.
pub fn run_chunk_on(chunk: Chunk) -> Result<Value, String> {
    let mut vm = VM::new(chunk);
    crate::builtins::install(&mut vm);
    vm.set_numeric_hook(std::sync::Arc::new(|op, a, b| {
        crate::builtins::numeric_hook(op, a, b)
    }));
    if DEBUG_MODE.with(|d| d.get()) {
        vm.set_extension_handler(Box::new(|vm, id, _| {
            crate::dap::on_ext(vm, id);
        }));
    } else {
        vm.enable_tracing_jit();
    }
    let outcome = vm.run();
    if let Some(e) = with_host(|h| h.take_error()) {
        return Err(e);
    }
    match outcome {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.last().cloned().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Run the top-level program chunk.
pub fn run_main(chunk: Chunk) -> Result<Value, String> {
    let r = run_chunk_on(chunk);
    with_host(|h| h.signal = None);
    r
}

// ── value operations (pure over builtin types) ───────────────────────────────

/// If `n` (a `PyObj::Builtin` name) is a *type object* — the kind `type(x)`
/// returns — its CPython class name for `repr` (`<class '…'>`), module-qualified
/// where CPython qualifies it. Returns `None` for callable builtins (`len`,
/// `print`, `math.sqrt`), which repr as `<built-in function …>`. The set mirrors
/// every name `PyHost::type_name` can emit.
fn type_object_class_name(n: &str) -> Option<String> {
    // Module-qualified stdlib types.
    let qualified = match n {
        "Counter" => Some("collections.Counter"),
        "defaultdict" => Some("collections.defaultdict"),
        "OrderedDict" => Some("collections.OrderedDict"),
        "deque" => Some("collections.deque"),
        "partial" => Some("functools.partial"),
        "TextIOWrapper" => Some("_io.TextIOWrapper"),
        // `type_name` already returns these fully qualified.
        "functools._lru_cache_wrapper" => Some("functools._lru_cache_wrapper"),
        _ => None,
    };
    if let Some(q) = qualified {
        return Some(q.to_string());
    }
    // Builtin exception classes (`ValueError`, `KeyError`, …) are type objects.
    if crate::builtins::is_exception_class(n) {
        return Some(n.to_string());
    }
    // Unqualified builtin type names: constructors plus the names `type()`
    // yields for functions, methods, iterators, views, sentinels, descriptors.
    let unqualified = matches!(
        n,
        "int"
            | "float"
            | "str"
            | "bool"
            | "list"
            | "tuple"
            | "dict"
            | "set"
            | "frozenset"
            | "bytes"
            | "bytearray"
            | "memoryview"
            | "complex"
            | "object"
            | "type"
            | "range"
            | "slice"
            | "NoneType"
            | "NotImplementedType"
            | "ellipsis"
            | "function"
            | "builtin_function_or_method"
            | "method"
            | "module"
            | "property"
            | "staticmethod"
            | "classmethod"
            | "super"
            | "iterator"
            | "callable_iterator"
            | "zip"
            | "map"
            | "filter"
            | "enumerate"
            | "generator"
            | "coroutine"
            | "async_generator"
            | "dict_keys"
            | "dict_values"
            | "dict_items"
    );
    unqualified.then(|| n.to_string())
}

/// A native-shadowed stdlib module whose native namespace is only a fast-path
/// subset. On a miss, defer to the real CPython module over the FFI bridge so
/// every symbol CPython's module exposes still resolves — `math.isqrt`/`trunc`/
/// `comb` (absent from the native `math` arm), `collections.ChainMap`/`UserDict`/
/// `abc` (absent from the native `collections` arm). The native members (`math`
/// constants/functions, `collections.deque`/`Counter`/`defaultdict`/`OrderedDict`/
/// `namedtuple`) are hit first, so only genuine misses defer. `Some(Ok/Err)` =
/// the module is shadowed and the FFI lookup ran; `None` = no fallback (not a
/// shadowed module, or the bridge is compiled out). `sys` keeps its native
/// objects (`stdout`/`argv`/…) and is intentionally not deferred.
#[cfg(feature = "stdlib-ffi")]
fn module_ffi_fallback(
    host: &mut PyHost,
    mname: &str,
    name: &str,
) -> Option<Result<Value, String>> {
    if !matches!(mname, "math" | "collections") {
        return None;
    }
    match crate::ffi::import(mname) {
        Ok(id) => Some(crate::ffi::get_attr(host, id, name)),
        Err(e) => Some(Err(e)),
    }
}
#[cfg(not(feature = "stdlib-ffi"))]
fn module_ffi_fallback(
    _host: &mut PyHost,
    _mname: &str,
    _name: &str,
) -> Option<Result<Value, String>> {
    None
}

impl PyHost {
    /// The Python type name of `v`.
    pub fn type_name(&self, v: &Value) -> String {
        match v {
            Value::Undef => "NoneType".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "str".into(),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(_)) => "str".into(),
                Some(PyObj::Bytes(_)) => "bytes".into(),
                Some(PyObj::Bytearray(_)) => "bytearray".into(),
                Some(PyObj::Memoryview { .. }) => "memoryview".into(),
                Some(PyObj::List(_)) => "list".into(),
                Some(PyObj::Tuple(_)) => match v {
                    Value::Obj(i) => match self.nt_meta.get(i) {
                        Some(m) => m.type_name.clone(),
                        None => "tuple".into(),
                    },
                    _ => "tuple".into(),
                },
                Some(PyObj::Dict(_)) => match v {
                    Value::Obj(i) => match self.dict_meta.get(i).map(|m| m.kind) {
                        Some(DictKind::Counter) => "Counter".into(),
                        Some(DictKind::DefaultDict) => "defaultdict".into(),
                        Some(DictKind::OrderedDict) => "OrderedDict".into(),
                        None => "dict".into(),
                    },
                    _ => "dict".into(),
                },
                Some(PyObj::Set(_)) => "set".into(),
                Some(PyObj::Frozenset(_)) => "frozenset".into(),
                Some(PyObj::DictView { kind, .. }) => match kind {
                    0 => "dict_keys".into(),
                    1 => "dict_values".into(),
                    _ => "dict_items".into(),
                },
                Some(PyObj::Range { .. }) => "range".into(),
                Some(PyObj::Slice { .. }) => "slice".into(),
                Some(PyObj::Func(_)) => "function".into(),
                // A builtin type/exception constructor (`int`, `ValueError`) is a
                // type object, so its type is `type`; a builtin function (`len`)
                // is a `builtin_function_or_method`.
                Some(PyObj::Builtin(n)) => {
                    if crate::builtins::is_type_like_builtin(n) {
                        "type".into()
                    } else {
                        "builtin_function_or_method".into()
                    }
                }
                // `type(cls)` is the class's metaclass (`type` unless overridden).
                Some(PyObj::Class(n)) => self
                    .classes
                    .get(n)
                    .map(|c| c.metaclass.clone())
                    .unwrap_or_else(|| "type".into()),
                Some(PyObj::Instance(i)) => i.class.clone(),
                Some(PyObj::BoundMethod { .. }) => "method".into(),
                Some(PyObj::Exception { class, .. }) => class.clone(),
                Some(PyObj::Iter(_)) => "iterator".into(),
                Some(PyObj::Zip { .. }) => "zip".into(),
                Some(PyObj::MapObj { .. }) => "map".into(),
                Some(PyObj::FilterObj { .. }) => "filter".into(),
                Some(PyObj::EnumerateObj { .. }) => "enumerate".into(),
                Some(PyObj::CallIter { .. }) => "callable_iterator".into(),
                Some(PyObj::Module { .. }) => "module".into(),
                Some(PyObj::BigInt(_)) => "int".into(),
                Some(PyObj::Complex(..)) => "complex".into(),
                Some(PyObj::Generator { id }) => match self.generators[*id as usize].kind {
                    GenKind::Coroutine => "coroutine".into(),
                    GenKind::Generator => "generator".into(),
                    GenKind::AsyncGen => "async_generator".into(),
                },
                Some(PyObj::Future { id }) => async_rt::future_type_name(*id).into(),
                Some(PyObj::EventLoop) => "_UnixSelectorEventLoop".into(),
                Some(PyObj::AsyncObj { id }) => async_rt::async_obj_type_name(*id).into(),
                Some(PyObj::File { .. }) => "TextIOWrapper".into(),
                Some(PyObj::Deque { .. }) => "deque".into(),
                Some(PyObj::NamedTupleType { .. }) => "type".into(),
                Some(PyObj::Partial { .. }) => "partial".into(),
                Some(PyObj::LruCache { .. }) => "functools._lru_cache_wrapper".into(),
                Some(PyObj::Super { .. }) => "super".into(),
                Some(PyObj::StaticMethod(_)) => "staticmethod".into(),
                Some(PyObj::ClassMethod(_)) => "classmethod".into(),
                Some(PyObj::Property { .. }) => "property".into(),
                Some(PyObj::NotImplemented) => "NotImplementedType".into(),
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(id)) => crate::ffi::type_name(*id),
                None => "object".into(),
            },
            _ => "object".into(),
        }
    }

    /// Python truthiness: None/False/0/0.0/""/[]/{}/set()/() are false.
    pub fn truthy(&self, v: &Value) -> bool {
        match v {
            Value::Undef => false,
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => !s.is_empty(),
                Some(PyObj::Bytes(b)) => !b.is_empty(),
                Some(PyObj::Bytearray(b)) => !b.is_empty(),
                Some(PyObj::Memoryview { len, .. }) => *len != 0,
                Some(PyObj::Deque { items, .. }) => !items.is_empty(),
                Some(PyObj::List(l)) => !l.is_empty(),
                Some(PyObj::Tuple(l)) => !l.is_empty(),
                Some(PyObj::Dict(d)) => !d.is_empty(),
                Some(PyObj::Set(s)) => !s.is_empty(),
                Some(PyObj::Frozenset(s)) => !s.is_empty(),
                Some(PyObj::DictView { dict, .. }) => {
                    matches!(self.get(dict), Some(PyObj::Dict(d)) if !d.is_empty())
                }
                Some(PyObj::Range { start, stop, step }) => range_len(*start, *stop, *step) != 0,
                Some(PyObj::BigInt(b)) => *b != num_bigint::BigInt::from(0),
                Some(PyObj::Complex(r, i)) => *r != 0.0 || *i != 0.0,
                Some(PyObj::Instance(_)) => true, // __bool__/__len__ handled by caller
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(id)) => crate::ffi::truthy(*id),
                _ => true,
            },
            _ => true,
        }
    }

    /// `str(v)` — the human string form.
    pub fn str_of(&self, v: &Value) -> String {
        match v {
            Value::Undef => "None".into(),
            Value::Bool(b) => if *b { "True" } else { "False" }.into(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => fmt_float(*f),
            Value::Str(s) => (**s).clone(),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => s.clone(),
                Some(PyObj::BigInt(b)) => b.to_string(),
                Some(PyObj::Complex(r, i)) => fmt_complex(*r, *i),
                Some(PyObj::Bytes(b)) => format!("b{}", quote_bytes(b, false)),
                Some(PyObj::Instance(inst)) => {
                    // A user exception instance stringifies to its message
                    // (`BaseException.__str__`): ''/str(arg)/repr(tuple).
                    if self.class_is_exception(&inst.class) {
                        let a = self.exc_instance_args(&inst.dict);
                        self.exc_message(&inst.class, &a)
                    } else if !matches!(inst.payload, Value::Undef)
                        && self.builtin_base_of(&inst.class).is_some()
                        && self.class_lookup(&inst.class, "__str__").is_none()
                        && self.class_lookup(&inst.class, "__repr__").is_none()
                    {
                        // Builtin-type subclass without a `__str__`/`__repr__`
                        // override: the base type's string form (`str(Stack(...))`
                        // → the list form, `str(U("hi"))` → `"hi"`).
                        self.str_of(&inst.payload)
                    } else {
                        // `object.__repr__` default: `<__main__.Cls object at 0x…>`.
                        // Instances defined under `-c`/a script live in `__main__`
                        // (matching the `Class` repr above).
                        format!(
                            "<__main__.{} object at 0x{:012x}>",
                            inst.class,
                            self.addr_of(v)
                        )
                    }
                }
                // User classes are defined in the top-level module, which under
                // `-c`/a script CPython names `__main__` (builtins stay bare).
                Some(PyObj::Class(n)) => format!("<class '__main__.{n}'>"),
                Some(PyObj::Func(f)) => {
                    let name = self
                        .funcs
                        .get(f.def_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    format!("<function {name}>")
                }
                // A `PyObj::Builtin` is an unbound builtin method
                // (`str.upper`), a *type object* returned by `type(x)` (repr
                // `<class 'X'>`), or a plain callable builtin (`len`,
                // `math.sqrt` -> `<built-in function X>`).
                Some(PyObj::Builtin(n)) => {
                    if let Some((tp, meth)) = n.split_once('.') {
                        if crate::builtins::type_has_method(tp, meth) {
                            return format!("<method '{meth}' of '{tp}' objects>");
                        }
                    }
                    match type_object_class_name(n) {
                        Some(cls) => format!("<class '{cls}'>"),
                        None => format!("<built-in function {n}>"),
                    }
                }
                Some(PyObj::BoundMethod { .. }) => "<bound method>".into(),
                Some(PyObj::Exception { class, args }) => self.exc_str(class, args),
                Some(PyObj::Module { name, .. }) => format!("<module '{name}'>"),
                Some(PyObj::Range { start, stop, step }) => {
                    if *step == 1 {
                        format!("range({start}, {stop})")
                    } else {
                        format!("range({start}, {stop}, {step})")
                    }
                }
                Some(PyObj::Iter(_)) => "<iterator>".into(),
                Some(PyObj::Zip { .. }) => format!("<zip object at 0x{:012x}>", self.addr_of(v)),
                Some(PyObj::MapObj { .. }) => format!("<map object at 0x{:012x}>", self.addr_of(v)),
                Some(PyObj::FilterObj { .. }) => {
                    format!("<filter object at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::EnumerateObj { .. }) => {
                    format!("<enumerate object at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::CallIter { .. }) => {
                    format!("<callable_iterator object at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::Generator { id }) => {
                    let g = &self.generators[*id as usize];
                    let nm = g
                        .ctx
                        .frames
                        .first()
                        .map(|f| f.name.clone())
                        .unwrap_or_default();
                    match g.kind {
                        GenKind::Coroutine => {
                            format!("<coroutine object {nm} at 0x{:012x}>", self.addr_of(v))
                        }
                        GenKind::Generator => {
                            format!("<generator object {nm} at 0x{:012x}>", self.addr_of(v))
                        }
                        GenKind::AsyncGen => {
                            format!(
                                "<async_generator object {nm} at 0x{:012x}>",
                                self.addr_of(v)
                            )
                        }
                    }
                }
                Some(PyObj::Future { id }) => async_rt::future_repr(*id),
                Some(PyObj::EventLoop) => {
                    "<_UnixSelectorEventLoop running=False closed=False debug=False>".into()
                }
                Some(PyObj::AsyncObj { id }) => async_rt::async_obj_repr(*id),
                Some(PyObj::Bytearray(b)) => format!("bytearray(b{})", quote_bytes(b, true)),
                Some(PyObj::Memoryview { .. }) => {
                    format!("<memory at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::File { id }) => self.file_repr(*id),
                Some(PyObj::Deque { items, maxlen }) => {
                    let inner: Vec<String> = items.iter().map(|x| self.repr_of(x)).collect();
                    match maxlen {
                        Some(m) => format!("deque([{}], maxlen={m})", inner.join(", ")),
                        None => format!("deque([{}])", inner.join(", ")),
                    }
                }
                Some(PyObj::NamedTupleType { type_name, .. }) => format!("<class '{type_name}'>"),
                Some(PyObj::Partial { func, .. }) => {
                    format!("functools.partial({})", self.repr_of(func))
                }
                Some(PyObj::LruCache { func, .. }) => {
                    format!("<functools._lru_cache_wrapper {}>", self.str_of(func))
                }
                Some(PyObj::Super { owner, instance }) => {
                    let icls = match self.get(instance) {
                        Some(PyObj::Instance(i)) => i.class.clone(),
                        _ => owner.clone(),
                    };
                    format!("<super: <class '{owner}'>, <{icls} object>>")
                }
                Some(PyObj::StaticMethod(f)) => {
                    format!("<staticmethod({})>", self.str_of(f))
                }
                Some(PyObj::ClassMethod(f)) => {
                    format!("<classmethod({})>", self.str_of(f))
                }
                Some(PyObj::Property { .. }) => "<property object>".into(),
                Some(PyObj::NotImplemented) => "NotImplemented".into(),
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(id)) => crate::ffi::str_of(*id),
                Some(PyObj::Slice { .. })
                | Some(PyObj::List(_))
                | Some(PyObj::Tuple(_))
                | Some(PyObj::Dict(_))
                | Some(PyObj::Set(_))
                | Some(PyObj::Frozenset(_))
                | Some(PyObj::DictView { .. }) => self.repr_of(v),
                None => "<object>".into(),
            },
            _ => "<object>".into(),
        }
    }

    fn exc_str(&self, class: &str, args: &[Value]) -> String {
        self.exc_message(class, args)
    }

    /// `repr(v)`.
    pub fn repr_of(&self, v: &Value) -> String {
        match v {
            Value::Str(s) => quote_str(s),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => quote_str(s),
                Some(PyObj::List(l)) => {
                    let id = if let Value::Obj(i) = v { *i } else { 0 };
                    if repr_guard_enter(id) {
                        return "[...]".into();
                    }
                    let inner: Vec<String> = l.iter().map(|x| self.repr_of(x)).collect();
                    repr_guard_leave(id);
                    format!("[{}]", inner.join(", "))
                }
                Some(PyObj::Tuple(l)) => {
                    let id = if let Value::Obj(i) = v { *i } else { 0 };
                    if repr_guard_enter(id) {
                        return "(...)".into();
                    }
                    // A namedtuple instance reprs as `Type(field=value, …)`.
                    let nt = match v {
                        Value::Obj(i) => self.nt_meta.get(i),
                        _ => None,
                    };
                    let out = if let Some(m) = nt {
                        let inner: Vec<String> = m
                            .fields
                            .iter()
                            .zip(l.iter())
                            .map(|(f, x)| format!("{f}={}", self.repr_of(x)))
                            .collect();
                        format!("{}({})", m.type_name, inner.join(", "))
                    } else {
                        let inner: Vec<String> = l.iter().map(|x| self.repr_of(x)).collect();
                        if l.len() == 1 {
                            format!("({},)", inner[0])
                        } else {
                            format!("({})", inner.join(", "))
                        }
                    };
                    repr_guard_leave(id);
                    out
                }
                Some(PyObj::Dict(d)) => {
                    let id = if let Value::Obj(i) = v { *i } else { 0 };
                    if repr_guard_enter(id) {
                        return "{...}".into();
                    }
                    let body: Vec<String> = d
                        .values()
                        .map(|(k, val)| format!("{}: {}", self.repr_of(k), self.repr_of(val)))
                        .collect();
                    let dict_repr = format!("{{{}}}", body.join(", "));
                    let meta = match v {
                        Value::Obj(i) => self.dict_meta.get(i),
                        _ => None,
                    };
                    let empty = d.is_empty();
                    let out = match meta.map(|m| (m.kind, m.factory.clone())) {
                        Some((DictKind::Counter, _)) if empty => "Counter()".into(),
                        Some((DictKind::Counter, _)) => format!("Counter({dict_repr})"),
                        Some((DictKind::DefaultDict, factory)) => {
                            let f = factory
                                .map(|fv| self.repr_of(&fv))
                                .unwrap_or_else(|| "None".into());
                            format!("defaultdict({f}, {dict_repr})")
                        }
                        // CPython 3.12+ reprs OrderedDict dict-style, not as a
                        // list of pairs; an empty one is the bare `OrderedDict()`.
                        Some((DictKind::OrderedDict, _)) if empty => "OrderedDict()".into(),
                        Some((DictKind::OrderedDict, _)) => format!("OrderedDict({dict_repr})"),
                        None => dict_repr,
                    };
                    repr_guard_leave(id);
                    out
                }
                Some(PyObj::Set(s)) => {
                    if s.is_empty() {
                        "set()".into()
                    } else {
                        let id = if let Value::Obj(i) = v { *i } else { 0 };
                        if repr_guard_enter(id) {
                            return "{...}".into();
                        }
                        let inner: Vec<String> = self
                            .set_ordered_values(s)
                            .iter()
                            .map(|x| self.repr_of(x))
                            .collect();
                        repr_guard_leave(id);
                        format!("{{{}}}", inner.join(", "))
                    }
                }
                Some(PyObj::Frozenset(s)) => {
                    if s.is_empty() {
                        "frozenset()".into()
                    } else {
                        let id = if let Value::Obj(i) = v { *i } else { 0 };
                        if repr_guard_enter(id) {
                            return "frozenset(...)".into();
                        }
                        let inner: Vec<String> = self
                            .set_ordered_values(s)
                            .iter()
                            .map(|x| self.repr_of(x))
                            .collect();
                        repr_guard_leave(id);
                        format!("frozenset({{{}}})", inner.join(", "))
                    }
                }
                Some(PyObj::DictView { dict, kind }) => {
                    let (kind, dict) = (*kind, dict.clone());
                    let label = match kind {
                        0 => "dict_keys",
                        1 => "dict_values",
                        _ => "dict_items",
                    };
                    let inner: Vec<String> = match self.get(&dict) {
                        Some(PyObj::Dict(d)) => d
                            .values()
                            .map(|(k, v)| match kind {
                                0 => self.repr_of(k),
                                1 => self.repr_of(v),
                                _ => format!("({}, {})", self.repr_of(k), self.repr_of(v)),
                            })
                            .collect(),
                        _ => vec![],
                    };
                    format!("{label}([{}])", inner.join(", "))
                }
                Some(PyObj::Exception { class, args }) => {
                    let inner: Vec<String> = args.iter().map(|a| self.repr_of(a)).collect();
                    format!("{class}({})", inner.join(", "))
                }
                // A user exception instance reprs as `Class(repr(arg), …)` from
                // its stored `args`, mirroring `BaseException.__repr__`.
                Some(PyObj::Instance(inst)) if self.class_is_exception(&inst.class) => {
                    let a = self.exc_instance_args(&inst.dict);
                    let inner: Vec<String> = a.iter().map(|x| self.repr_of(x)).collect();
                    format!("{}({})", inst.class, inner.join(", "))
                }
                // Builtin-type subclass without a `__repr__` override: the base
                // type's repr (`repr(Stack([1,2]))` → `[1, 2]`).
                Some(PyObj::Instance(inst))
                    if !matches!(inst.payload, Value::Undef)
                        && self.builtin_base_of(&inst.class).is_some()
                        && self.class_lookup(&inst.class, "__repr__").is_none() =>
                {
                    self.repr_of(&inst.payload)
                }
                Some(PyObj::Slice { lo, hi, step }) => format!(
                    "slice({}, {}, {})",
                    self.repr_of(lo),
                    self.repr_of(hi),
                    self.repr_of(step)
                ),
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(id)) => crate::ffi::repr_of(*id),
                _ => self.str_of(v),
            },
            _ => self.str_of(v),
        }
    }

    /// A hashable key for a dict/set. Returns an error for unhashable types.
    pub fn to_key(&self, v: &Value) -> Result<PKey, String> {
        Ok(match v {
            Value::Undef => PKey::None,
            // Numbers hash by value: `1`, `1.0`, and `True` share one key.
            Value::Bool(b) => PKey::Int(*b as i64),
            Value::Int(n) => PKey::Int(*n),
            Value::Float(f) => float_pkey(*f),
            Value::Str(s) => PKey::Str((**s).clone()),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => PKey::Str(s.clone()),
                // `bytes` is hashable by its byte content; `bytearray` is not.
                Some(PyObj::Bytes(b)) => PKey::Bytes(b.clone()),
                Some(PyObj::BigInt(b)) => PKey::Big(b.clone()),
                Some(PyObj::Complex(r, i)) => {
                    if *i == 0.0 {
                        float_pkey(*r)
                    } else {
                        PKey::Complex(r.to_bits(), i.to_bits())
                    }
                }
                Some(PyObj::Tuple(items)) => {
                    let mut ks = Vec::with_capacity(items.len());
                    for it in items {
                        ks.push(self.to_key(it)?);
                    }
                    PKey::Tuple(ks)
                }
                Some(PyObj::Frozenset(s)) => {
                    // Canonicalize: element keys sorted + deduped, so any two
                    // equal frozensets hash and compare identically.
                    let mut ks: Vec<PKey> = s.keys().cloned().collect();
                    ks.sort();
                    ks.dedup();
                    PKey::Frozenset(ks)
                }
                // A type object keys by name (types are singletons by name).
                Some(PyObj::Class(n)) => PKey::Class(n.clone()),
                Some(PyObj::Builtin(n)) => PKey::Class(n.clone()),
                Some(PyObj::Instance(inst)) => {
                    let id = match v {
                        Value::Obj(i) => *i,
                        _ => 0,
                    };
                    let class = inst.class.clone();
                    // A key resolved by `prepare_key` (user `__hash__` ran outside
                    // the borrow) wins; otherwise fall back to what we can decide
                    // here without user code.
                    if let Some(k) = pending_key_get(id) {
                        k
                    } else {
                        match self.class_lookup(&class, "__hash__") {
                            // `__hash__ = None` (or `__eq__` without `__hash__`)
                            // makes instances unhashable (CPython rule).
                            Some(Value::Undef) => {
                                return Err(type_error(&format!("unhashable type: '{class}'")))
                            }
                            None if self.class_lookup(&class, "__eq__").is_some() => {
                                return Err(type_error(&format!("unhashable type: '{class}'")))
                            }
                            // A builtin-subclass instance that overrides neither
                            // `__hash__` nor `__eq__` inherits the base type's
                            // hash, so it keys by its payload — `U("a")` (with
                            // `class U(str)`) keys and compares identically to
                            // `"a"`. Only a payload-bearing subclass; a plain
                            // `object` subclass keeps the identity hash below.
                            None if !matches!(inst.payload, Value::Undef) => {
                                return self.to_key(&inst.payload);
                            }
                            // Default identity hash — no user code needed.
                            None => PKey::Instance {
                                hash: id as i64,
                                id,
                            },
                            // A user `__hash__` must be resolved via `prepare_key`
                            // before the borrowed key lookup; reaching here means a
                            // keying path was not routed. Fail visibly, never guess.
                            Some(_) => {
                                return Err(type_error(&format!("unhashable type: '{class}'")))
                            }
                        }
                    }
                }
                Some(other) => {
                    return Err(type_error(&format!(
                        "unhashable type: '{}'",
                        self.type_name_obj(other)
                    )))
                }
                None => PKey::None,
            },
            _ => return Err(type_error("unhashable type")),
        })
    }

    fn type_name_obj(&self, o: &PyObj) -> &'static str {
        match o {
            PyObj::List(_) => "list",
            PyObj::Dict(_) => "dict",
            PyObj::Set(_) => "set",
            PyObj::Frozenset(_) => "frozenset",
            _ => "object",
        }
    }

    /// Structural equality (`==`).
    pub fn equal(&self, a: &Value, b: &Value) -> bool {
        // A builtin-subclass instance with no `__eq__` override compares by its
        // native payload value (`'cat' == U('cat')`, `Stack([1]) == [1]`).
        let ua;
        let a = if self.class_lookup_eq_free(a) {
            ua = self.base_payload_any(a).unwrap();
            &ua
        } else {
            a
        };
        let ub;
        let b = if self.class_lookup_eq_free(b) {
            ub = self.base_payload_any(b).unwrap();
            &ub
        } else {
            b
        };
        match (a, b) {
            (Value::Undef, Value::Undef) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            _ => {
                if let (Some(x), Some(y)) = (self.num_val(a), self.num_val(b)) {
                    return x == y;
                }
                // complex == complex / complex == real
                if self.is_complex(a) || self.is_complex(b) {
                    if let (Some((ar, ai)), Some((br, bi))) =
                        (self.complex_val(a), self.complex_val(b))
                    {
                        return ar == br && ai == bi;
                    }
                }
                match (self.get(a), self.get(b)) {
                    (Some(PyObj::Str(x)), Some(PyObj::Str(y))) => x == y,
                    (Some(PyObj::List(x)), Some(PyObj::List(y)))
                    | (Some(PyObj::Tuple(x)), Some(PyObj::Tuple(y))) => {
                        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.equal(p, q))
                    }
                    (Some(PyObj::Dict(x)), Some(PyObj::Dict(y))) => {
                        x.len() == y.len()
                            && x.iter().all(|(k, (_, xv))| {
                                y.get(k).map(|(_, yv)| self.equal(xv, yv)).unwrap_or(false)
                            })
                    }
                    // `set == frozenset` compares by membership, so
                    // `{1,2} == frozenset({1,2})` holds.
                    (Some(PyObj::Set(x)), Some(PyObj::Set(y)))
                    | (Some(PyObj::Set(x)), Some(PyObj::Frozenset(y)))
                    | (Some(PyObj::Frozenset(x)), Some(PyObj::Set(y)))
                    | (Some(PyObj::Frozenset(x)), Some(PyObj::Frozenset(y))) => {
                        x.len() == y.len() && x.keys().all(|k| y.contains_key(k))
                    }
                    (Some(PyObj::Deque { items: x, .. }), Some(PyObj::Deque { items: y, .. })) => {
                        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.equal(p, q))
                    }
                    // Two ranges are equal iff they yield the same sequence: same
                    // length, and (empty, or same start and (len 1 or same step)).
                    (
                        Some(PyObj::Range {
                            start: s1,
                            stop: e1,
                            step: t1,
                        }),
                        Some(PyObj::Range {
                            start: s2,
                            stop: e2,
                            step: t2,
                        }),
                    ) => {
                        let (l1, l2) = (range_len(*s1, *e1, *t1), range_len(*s2, *e2, *t2));
                        l1 == l2 && (l1 == 0 || (s1 == s2 && (l1 == 1 || t1 == t2)))
                    }
                    // bytes/bytearray compare equal by content (`b'a' == bytearray(b'a')`).
                    (Some(PyObj::Bytes(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytes(x)), Some(PyObj::Bytearray(y)))
                    | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytearray(y))) => x == y,
                    // A memoryview compares by its bytes against another view or a
                    // bytes/bytearray (`memoryview(b'ab') == b'ab'`).
                    (Some(PyObj::Memoryview { .. }), _)
                        if matches!(
                            self.get(b),
                            Some(PyObj::Memoryview { .. })
                                | Some(PyObj::Bytes(_))
                                | Some(PyObj::Bytearray(_))
                        ) =>
                    {
                        let yb = match self.get(b) {
                            Some(PyObj::Bytes(y)) | Some(PyObj::Bytearray(y)) => y.clone(),
                            _ => self.mv_bytes(b),
                        };
                        self.mv_bytes(a) == yb
                    }
                    (_, Some(PyObj::Memoryview { .. }))
                        if matches!(
                            self.get(a),
                            Some(PyObj::Bytes(_)) | Some(PyObj::Bytearray(_))
                        ) =>
                    {
                        let xb = match self.get(a) {
                            Some(PyObj::Bytes(x)) | Some(PyObj::Bytearray(x)) => x.clone(),
                            _ => Vec::new(),
                        };
                        xb == self.mv_bytes(b)
                    }
                    // Type/function objects compare by name, so `type(5) == int`
                    // and `type(b) == B` hold regardless of heap identity.
                    (Some(PyObj::Builtin(x)), Some(PyObj::Builtin(y))) => x == y,
                    (Some(PyObj::Class(x)), Some(PyObj::Class(y))) => x == y,
                    _ => match (a, b) {
                        (Value::Str(x), Value::Str(y)) => x == y,
                        _ => a == b,
                    },
                }
            }
        }
    }

    /// A value as `(real, imag)` if it participates in complex arithmetic: any
    /// real number (imag = 0) or a `complex`. `None` for non-numerics.
    pub fn complex_val(&self, v: &Value) -> Option<(f64, f64)> {
        if let Some(PyObj::Complex(r, i)) = self.get(v) {
            return Some((*r, *i));
        }
        self.num_val(v).map(|r| (r, 0.0))
    }

    /// True if `v` is a `complex` heap object.
    pub fn is_complex(&self, v: &Value) -> bool {
        matches!(self.get(v), Some(PyObj::Complex(..)))
    }

    /// A numeric value as f64 if `v` is a number (int/float/bool/bigint).
    pub fn num_val(&self, v: &Value) -> Option<f64> {
        match v {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(*b as i64 as f64),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::BigInt(b)) => Some(bigint_to_f64(b)),
                // An `int`/`float` subclass coerces through its native payload.
                Some(PyObj::Instance(_)) => self.base_payload_num(v).and_then(|p| self.num_val(&p)),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_int(&self, v: &Value) -> Option<i64> {
        match v {
            Value::Int(n) => Some(*n),
            Value::Bool(b) => Some(*b as i64),
            // An `int` subclass instance coerces through its native payload.
            Value::Obj(_) => self.base_payload_num(v).and_then(|p| self.as_int(&p)),
            _ => None,
        }
    }

    /// The native numeric payload of a builtin-subclass instance whose base is
    /// `int`/`float` and which does not override the numeric-coercion dunders —
    /// so value-level coercion (`as_int`/`num_val`) sees through the subclass.
    fn base_payload_num(&self, v: &Value) -> Option<Value> {
        match self.get(v) {
            Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
                match self.builtin_base_of(&i.class) {
                    Some("int") | Some("float") => Some(i.payload.clone()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether `v` is a builtin-subclass instance that compares by its base
    /// value (has a native payload and no user `__eq__` override).
    fn class_lookup_eq_free(&self, v: &Value) -> bool {
        match self.get(v) {
            Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
                self.builtin_base_of(&i.class).is_some()
                    && self.class_lookup(&i.class, "__eq__").is_none()
            }
            _ => false,
        }
    }

    /// The native payload of any builtin-subclass instance (value-level
    /// unwrapping for `as_str`/`equal`), or `None` for a plain object subclass.
    fn base_payload_any(&self, v: &Value) -> Option<Value> {
        match self.get(v) {
            Some(PyObj::Instance(i))
                if !matches!(i.payload, Value::Undef)
                    && self.builtin_base_of(&i.class).is_some() =>
            {
                Some(i.payload.clone())
            }
            _ => None,
        }
    }
}

/// Insert into a dict with CPython semantics: on a duplicate key, keep the
/// FIRST-inserted key object but update the value (`{1: 'a', 1.0: 'b'}` → `{1: 'b'}`).
pub fn dict_put(d: &mut IndexMap<PKey, (Value, Value)>, key: PKey, kv: Value, val: Value) {
    use indexmap::map::Entry;
    match d.entry(key) {
        Entry::Occupied(mut e) => e.get_mut().1 = val,
        Entry::Vacant(e) => {
            e.insert((kv, val));
        }
    }
}

/// Insert into a set with CPython semantics: a duplicate keeps the FIRST element
/// object (`{1, 1.0, True}` → `{1}`).
pub fn set_put(s: &mut IndexMap<PKey, Value>, key: PKey, item: Value) {
    s.entry(key).or_insert(item);
}

// ── CPython set iteration order (`setobject.c`) ──────────────────────────────
//
// A set/frozenset iterates (and reprs) in open-addressing table order, not
// insertion order. For a set of plain machine ints that order is deterministic
// (`hash(n) == n`, bar `hash(-1) == -2`), so pythonrs reproduces it faithfully.
// String/other-object hashes are per-process randomized in CPython (SipHash with
// a random key), so no interpreter can match them byte-for-byte across runs —
// those sets stay in insertion order (the documented boundary).

const SET_MINSIZE: usize = 8;
const SET_LINEAR_PROBES: usize = 9;
const SET_PERTURB_SHIFT: u32 = 5;

/// CPython's `hash()` for a machine int: the value itself, except `-1` maps to
/// `-2` (`-1` is CPython's error sentinel for `tp_hash`).
fn cpython_int_hash(n: i64) -> i64 {
    if n == -1 {
        -2
    } else {
        n
    }
}

/// A faithful port of CPython `setobject.c`'s open-addressing table, restricted
/// to what iteration order needs: it places each element (given by its hash and
/// its original insertion index) and reports the final slot order. Elements are
/// already distinct (deduped by `PKey`), so the equality branch never fires.
struct SetTable {
    slots: Vec<Option<(i64, usize)>>,
    mask: usize,
    fill: usize,
    used: usize,
}

impl SetTable {
    fn new() -> SetTable {
        SetTable {
            slots: vec![None; SET_MINSIZE],
            mask: SET_MINSIZE - 1,
            fill: 0,
            used: 0,
        }
    }

    /// Probe for the first empty slot for `hash` (CPython perturb + linear-probe
    /// sequence). All live elements are distinct, so we never match an occupant.
    fn find_empty(slots: &[Option<(i64, usize)>], mask: usize, hash: i64) -> usize {
        let mut perturb = hash as u64;
        let mut i = (hash as u64 as usize) & mask;
        loop {
            let probes = if i + SET_LINEAR_PROBES <= mask {
                SET_LINEAR_PROBES
            } else {
                0
            };
            for (entry, slot) in slots.iter().enumerate().skip(i).take(probes + 1) {
                if slot.is_none() {
                    return entry;
                }
            }
            perturb >>= SET_PERTURB_SHIFT;
            i = (i
                .wrapping_mul(5)
                .wrapping_add(1)
                .wrapping_add(perturb as usize))
                & mask;
        }
    }

    fn add(&mut self, hash: i64, idx: usize) {
        let slot = Self::find_empty(&self.slots, self.mask, hash);
        self.slots[slot] = Some((hash, idx));
        self.fill += 1;
        self.used += 1;
        // Grow when the table is ~3/5 full (CPython `fill*5 >= mask*3`).
        if self.fill * 5 >= self.mask * 3 {
            let minused = if self.used > 50000 {
                self.used * 2
            } else {
                self.used * 4
            };
            self.resize(minused);
        }
    }

    fn resize(&mut self, minused: usize) {
        let mut newsize = SET_MINSIZE;
        while newsize <= minused {
            newsize <<= 1;
        }
        let old = std::mem::replace(&mut self.slots, vec![None; newsize]);
        self.mask = newsize - 1;
        self.fill = self.used;
        // Reinsert the live entries in old-table slot order (`set_insert_clean`).
        for entry in old.into_iter().flatten() {
            let (hash, idx) = entry;
            let slot = Self::find_empty(&self.slots, self.mask, hash);
            self.slots[slot] = Some((hash, idx));
        }
    }
}

/// The original-insertion indices of `hashes`, reordered into CPython set
/// iteration order. `hashes[k]` is the CPython hash of the `k`-th inserted
/// element.
fn cpython_set_order(hashes: &[i64]) -> Vec<usize> {
    let mut t = SetTable::new();
    for (idx, &h) in hashes.iter().enumerate() {
        t.add(h, idx);
    }
    t.slots
        .iter()
        .filter_map(|s| s.map(|(_, idx)| idx))
        .collect()
}

// ── instance hashing (user `__hash__` / `__eq__` as dict/set keys) ───────────

/// The `Py_hash_t` (i64) value of a `__hash__` result. CPython truncates a
/// returned int to the platform hash width; a non-int result is a `TypeError`.
fn hash_int_of(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Int(n) => Ok(*n),
        _ => with_host(|h| match h.get(v) {
            // A bignum `__hash__` result is reduced the way CPython hashes ints
            // (mod 2**61-1 on 64-bit), keeping equal values' hashes equal.
            Some(PyObj::BigInt(b)) => Ok(bigint_pyhash(b)),
            _ => Err(type_error("__hash__ method should return an integer")),
        }),
    }
}

/// CPython's `long_hash`: `x mod (2**61 - 1)` with sign, `-1` mapped to `-2`.
fn bigint_pyhash(b: &num_bigint::BigInt) -> i64 {
    use num_bigint::BigInt;
    use num_traits::ToPrimitive;
    let modulus = BigInt::from((1i64 << 61) - 1);
    let mut r = b % &modulus;
    if r.sign() == num_bigint::Sign::Minus {
        r += &modulus;
    }
    // `r` is now in [0, 2**61-1); re-apply the original sign.
    let mut h = r.to_i64().unwrap_or(0);
    if b.sign() == num_bigint::Sign::Minus {
        h = -h;
    }
    if h == -1 {
        -2
    } else {
        h
    }
}

/// Resolve — outside any host borrow — the dict/set key for a user instance whose
/// class defines `__hash__`, stashing it in the pending-key table so the borrowed
/// [`PyHost::to_key`] can pick it up. `candidates` are the `(key, key-object)`
/// pairs already in the target container; if the instance's `__hash__` matches an
/// existing instance key whose object is `__eq__`-equal, the key collapses onto
/// that entry (CPython value semantics). A no-op for non-instances and for
/// identity-hashed instances (`to_key` handles those inline).
pub fn prepare_key(v: &Value, candidates: &[(PKey, Value)]) -> Result<(), String> {
    let (id, class) = match with_host(|h| match v {
        Value::Obj(i) => match h.get(v) {
            Some(PyObj::Instance(inst)) => Some((*i, inst.class.clone())),
            _ => None,
        },
        _ => None,
    }) {
        Some(t) => t,
        None => return Ok(()),
    };
    let hashf = with_host(|h| h.class_lookup(&class, "__hash__"));
    match &hashf {
        // `__hash__ = None`, or `__eq__` without `__hash__` → unhashable.
        Some(Value::Undef) => return Err(type_error(&format!("unhashable type: '{class}'"))),
        None => {
            if with_host(|h| h.class_lookup(&class, "__eq__").is_some()) {
                return Err(type_error(&format!("unhashable type: '{class}'")));
            }
            // Default identity hash: `to_key` resolves it inline, no prep needed.
            return Ok(());
        }
        Some(_) => {}
    }
    let hres = call_method(v, "__hash__", vec![], vec![])?;
    let hash = hash_int_of(&hres)?;
    // Collapse onto a value-equal existing instance key of the same hash.
    let mut canonical = PKey::Instance { hash, id };
    for (pk, kobj) in candidates {
        if let PKey::Instance { hash: ch, .. } = pk {
            if *ch == hash {
                let eqr = call_method(v, "__eq__", vec![kobj.clone()], vec![])?;
                if with_host(|h| h.truthy(&eqr)) {
                    canonical = pk.clone();
                    break;
                }
            }
        }
    }
    pending_key_set(id, canonical);
    Ok(())
}

/// `hash(instance)`: the class's `__hash__()` result verbatim (default identity
/// hash if undefined), or a `TypeError` if the instance is unhashable.
pub fn instance_hash_value(v: &Value) -> Result<i64, String> {
    let (id, class) = match with_host(|h| match v {
        Value::Obj(i) => match h.get(v) {
            Some(PyObj::Instance(inst)) => Some((*i, inst.class.clone())),
            _ => None,
        },
        _ => None,
    }) {
        Some(t) => t,
        None => return Err(type_error("unhashable type")),
    };
    match with_host(|h| h.class_lookup(&class, "__hash__")) {
        Some(Value::Undef) => Err(type_error(&format!("unhashable type: '{class}'"))),
        None => {
            if with_host(|h| h.class_lookup(&class, "__eq__").is_some()) {
                return Err(type_error(&format!("unhashable type: '{class}'")));
            }
            // A builtin-type subclass hashes by its base value
            // (str/int/float/tuple/frozenset); a list/dict/set subclass is
            // unhashable, exactly like its base.
            if let Some((base, payload)) = with_host(|h| match h.get(v) {
                Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
                    h.builtin_base_of(&i.class).map(|b| (b, i.payload.clone()))
                }
                _ => None,
            }) {
                if base_provides(base, "__hash__") {
                    let k = with_host(|h| h.to_key(&payload))?;
                    return Ok(crate::builtins::hash_key(&k));
                }
                return Err(type_error(&format!("unhashable type: '{class}'")));
            }
            Ok(id as i64)
        }
        Some(_) => {
            let r = call_method(v, "__hash__", vec![], vec![])?;
            hash_int_of(&r)
        }
    }
}

/// Instance-key collapse candidates from an in-flight set map (a literal/ctor
/// being built element by element).
pub fn set_local_candidates(s: &IndexMap<PKey, Value>) -> Vec<(PKey, Value)> {
    s.iter()
        .filter(|(k, _)| matches!(k, PKey::Instance { .. }))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Instance-key collapse candidates from an in-flight dict map.
pub fn dict_local_candidates(d: &IndexMap<PKey, (Value, Value)>) -> Vec<(PKey, Value)> {
    d.iter()
        .filter(|(k, _)| matches!(k, PKey::Instance { .. }))
        .map(|(k, (kv, _))| (k.clone(), kv.clone()))
        .collect()
}

/// The `(key, key-object)` pairs of any instance keys already present in a heap
/// dict or set/frozenset — the collapse candidates for [`prepare_key`].
pub fn instance_key_candidates(container: &Value) -> Vec<(PKey, Value)> {
    with_host(|h| match h.get(container) {
        Some(PyObj::Dict(d)) => d
            .iter()
            .filter(|(k, _)| matches!(k, PKey::Instance { .. }))
            .map(|(k, (kv, _))| (k.clone(), kv.clone()))
            .collect(),
        Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => s
            .iter()
            .filter(|(k, _)| matches!(k, PKey::Instance { .. }))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => vec![],
    })
}

/// Prepare an instance key for a container op, run `f` (the borrowed access that
/// calls `to_key`), then clear the pending table. `candidates` collapses a
/// value-equal existing key. Any non-instance `v` makes this a thin passthrough.
pub fn with_instance_key<R>(
    v: &Value,
    candidates: &[(PKey, Value)],
    f: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    let prep = prepare_key(v, candidates);
    let r = prep.and_then(|()| f());
    pending_key_clear();
    r
}

/// Canonical dict/set key for a float. An integral, finite float normalizes to
/// the matching integer key (`Int`/`Big`) so it unifies with `int`/`bool`
/// (`1.0 in {1}` → True); everything else keys by its raw bits.
fn float_pkey(f: f64) -> PKey {
    if f.is_finite() && f.fract() == 0.0 {
        if f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            return PKey::Int(f as i64);
        }
        use num_traits::FromPrimitive;
        if let Some(b) = num_bigint::BigInt::from_f64(f) {
            return PKey::Big(b);
        }
    }
    PKey::FloatBits(f.to_bits())
}

// ── integer floor-division / modulo (Python semantics, BigInt path) ──────────

/// `x // y` for BigInts, flooring toward −∞ (remainder takes the divisor's sign).
fn bigint_floordiv(x: &num_bigint::BigInt, y: &num_bigint::BigInt) -> num_bigint::BigInt {
    let q = x / y;
    let r = x % y;
    let zero = num_bigint::BigInt::from(0);
    if r != zero && (r < zero) != (*y < zero) {
        q - num_bigint::BigInt::from(1)
    } else {
        q
    }
}

/// `x % y` for BigInts, with the result taking the sign of `y` (floored).
fn bigint_mod(x: &num_bigint::BigInt, y: &num_bigint::BigInt) -> num_bigint::BigInt {
    let r = x % y;
    let zero = num_bigint::BigInt::from(0);
    if r != zero && (r < zero) != (*y < zero) {
        r + y
    } else {
        r
    }
}

// ── formatting helpers ───────────────────────────────────────────────────────

/// Python `repr`/`str` float: integral floats keep a trailing `.0`.
pub fn fmt_float(f: f64) -> String {
    if f.is_infinite() {
        return if f < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if f.is_nan() {
        return "nan".into();
    }
    // Python's `repr(float)`: the shortest round-trip decimal, switching to
    // scientific notation when the base-10 exponent is < -4 or >= 16, with a
    // sign and a min-2-digit exponent (`1e+16`, `1e-05`, `1.5e+300`). Rust's `{}`
    // never uses exponent form (so `1e16` prints as a 17-digit integer) and its
    // `{:e}` writes `e3`/`e-5` (no sign, no zero-pad) — neither matches CPython.
    let sci = format!("{f:e}"); // shortest scientific: "1.2345e3", "1e-5", "-1.5e300"
    let epos = sci
        .rfind('e')
        .expect("scientific format carries an exponent");
    let exp: i32 = sci[epos + 1..].parse().expect("valid exponent");
    if (-4..16).contains(&exp) {
        let mut s = format!("{f}");
        if !s.contains('.') {
            s.push_str(".0"); // integral value in fixed range -> Python's trailing `.0`
        }
        s
    } else {
        let mantissa = &sci[..epos];
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// Float `//` and `%`, ported from CPython's `float_divmod` (floatobject.c).
/// Uses `fmod` (not `x - floor(x/y)*y`) and carries the correct signed-zero and
/// the `div - floordiv > 0.5` correction, matching CPython bit-for-bit. Returns
/// `(floordiv, mod)`. Caller handles the `wx == 0` (ZeroDivisionError) case.
fn float_divmod(vx: f64, wx: f64) -> (f64, f64) {
    let mut mod_ = vx % wx; // C fmod
    let div = (vx - mod_) / wx;
    let mut div = div;
    if mod_ != 0.0 {
        if (wx < 0.0) != (mod_ < 0.0) {
            mod_ += wx;
            div -= 1.0;
        }
    } else {
        // A zero remainder takes the sign of the divisor.
        mod_ = 0.0_f64.copysign(wx);
    }
    let floordiv = if div != 0.0 {
        let fd = div.floor();
        if div - fd > 0.5 {
            fd + 1.0
        } else {
            fd
        }
    } else {
        0.0_f64.copysign(vx / wx)
    };
    (floordiv, mod_)
}

/// Complex division, ported from CPython 3.14's `_Py_c_quot` (complexobject.c) —
/// Smith's algorithm with fused multiply-add. Scaling by the larger-magnitude
/// divisor component avoids intermediate overflow, and the `fma` (Rust's
/// `mul_add`) reproduces CPython's rounding bit-for-bit.
fn c_quot(ar: f64, ai: f64, br: f64, bi: f64) -> (f64, f64) {
    let abs_br = br.abs();
    let abs_bi = bi.abs();
    if abs_br >= abs_bi {
        // Divide top and bottom by br.
        if abs_br == 0.0 {
            (0.0, 0.0)
        } else {
            let ratio = bi / br;
            let denom = bi.mul_add(ratio, br); // br + bi*ratio
            (
                ai.mul_add(ratio, ar) / denom,    // (ar + ai*ratio)/denom
                (-ar).mul_add(ratio, ai) / denom, // (ai - ar*ratio)/denom
            )
        }
    } else if abs_bi >= abs_br {
        // Divide top and bottom by bi.
        let ratio = br / bi;
        let denom = br.mul_add(ratio, bi); // br*ratio + bi
        (
            ar.mul_add(ratio, ai) / denom,  // (ar*ratio + ai)/denom
            ai.mul_add(ratio, -ar) / denom, // (ai*ratio - ar)/denom
        )
    } else {
        // At least one of br or bi is NaN.
        (f64::NAN, f64::NAN)
    }
}

fn fmt_complex(r: f64, i: f64) -> String {
    // A complex part reprs like a float but drops a trailing `.0` for integral
    // values (`complex(1,2)` → `(1+2j)`, not `(1.0+2.0j)`).
    if r == 0.0 && r.is_sign_positive() {
        format!("{}j", fmt_complex_part(i))
    } else {
        let sign = if i >= 0.0 || i.is_nan() { "+" } else { "-" };
        format!(
            "({}{}{}j)",
            fmt_complex_part(r),
            sign,
            fmt_complex_part(i.abs())
        )
    }
}

/// A single `complex` component: the float repr with a trailing `.0` stripped.
fn fmt_complex_part(f: f64) -> String {
    let s = fmt_float(f);
    match s.strip_suffix(".0") {
        Some(t) => t.to_string(),
        None => s,
    }
}

/// Complex exponentiation (`complex.__pow__`), a faithful port of CPython's
/// `complex_pow` (`Objects/complexobject.c`): a small integral exponent uses
/// exact repeated-squaring (`c_powi`); anything else the polar `_Py_c_pow`.
fn c_pow(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    if b.1 == 0.0 && b.0 == b.0.floor() && b.0.abs() <= 100.0 {
        return c_powi(a, b.0 as i64);
    }
    c_pow_polar(a, b)
}

fn c_pow_polar(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    if b.0 == 0.0 && b.1 == 0.0 {
        return (1.0, 0.0);
    }
    if a.0 == 0.0 && a.1 == 0.0 {
        return (0.0, 0.0);
    }
    let vabs = a.0.hypot(a.1);
    let mut len = vabs.powf(b.0);
    let at = a.1.atan2(a.0);
    let mut phase = at * b.0;
    if b.1 != 0.0 {
        len /= (at * b.1).exp();
        phase += b.1 * vabs.ln();
    }
    (len * phase.cos(), len * phase.sin())
}

fn c_prod(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

/// `c_powi`: integer complex power via repeated squaring (CPython's `c_powu`,
/// with the reciprocal for a negative exponent).
fn c_powi(x: (f64, f64), n: i64) -> (f64, f64) {
    if n < 0 {
        let p = c_powu(x, -n);
        // reciprocal 1/p
        let d = p.0 * p.0 + p.1 * p.1;
        return (p.0 / d, -p.1 / d);
    }
    c_powu(x, n)
}

fn c_powu(x: (f64, f64), n: i64) -> (f64, f64) {
    let mut r = (1.0, 0.0);
    let mut p = x;
    let mut mask = 1i64;
    while mask > 0 && n >= mask {
        if n & mask != 0 {
            r = c_prod(r, p);
        }
        mask <<= 1;
        p = c_prod(p, p);
    }
    r
}

/// CPython `Py_UNICODE_ISPRINTABLE`: a code point is printable unless its
/// general category is Other (Cc, Cf, Cs, Co, Cn) or Separator (Zl, Zp, Zs) —
/// with the sole exception that ASCII space (U+0020, a Zs) IS printable. Used by
/// `repr`, `ascii`, and `str.isprintable` to decide what to escape. Unicode 16.0
/// data (matches CPython 3.14's `unicodedata`).
pub fn is_printable_char(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    !matches!(
        get_general_category(c),
        G::Control
            | G::Format
            | G::Surrogate
            | G::PrivateUse
            | G::Unassigned
            | G::LineSeparator
            | G::ParagraphSeparator
            | G::SpaceSeparator
    )
}

fn quote_str(s: &str) -> String {
    let has_single = s.contains('\'');
    let has_double = s.contains('"');
    let (q, esc_q) = if has_single && !has_double {
        ('"', '"')
    } else {
        ('\'', '\'')
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == esc_q => {
                out.push('\\');
                out.push(c);
            }
            // Non-printable (controls, format, separators, unassigned, private
            // use — see `is_printable_char`): CPython repr escapes these as
            // `\xXX` (≤0xff), `\uXXXX` (≤0xffff), or `\UXXXXXXXX`. Printable
            // Unicode (e.g. `é`) is kept verbatim.
            c if !is_printable_char(c) => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Render the `'…'`/`"…"` quoted body of a `bytes`/`bytearray` repr. CPython
/// defaults to a single quote, switching to a double quote only when the buffer
/// contains a `'` but no `"`. `bytes` escapes just the chosen quote char; a
/// `bytearray` always escapes `'` (even under a `"` quote) — a CPython quirk
/// (`bytearray(b"a'b")` → `bytearray(b"a\'b")`).
fn quote_bytes(b: &[u8], is_bytearray: bool) -> String {
    let has_single = b.contains(&b'\'');
    let has_double = b.contains(&b'"');
    let quote = if has_single && !has_double {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::new();
    out.push(quote as char);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if c == quote => {
                out.push('\\');
                out.push(quote as char);
            }
            b'\'' if is_bytearray => out.push_str("\\'"),
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(quote as char);
    out
}

/// Number of elements in `range(start, stop, step)`.
pub fn range_len(start: i64, stop: i64, step: i64) -> i64 {
    if step == 0 {
        return 0;
    }
    if step > 0 {
        if stop > start {
            (stop - start + step - 1) / step
        } else {
            0
        }
    } else if start > stop {
        (start - stop - step - 1) / (-step)
    } else {
        0
    }
}

fn bigint_to_f64(b: &num_bigint::BigInt) -> f64 {
    use num_traits::ToPrimitive;
    b.to_f64().unwrap_or(f64::INFINITY)
}

// ── arithmetic / comparison delegated from the numeric hook ──────────────────

impl PyHost {
    /// The strict numeric-hook path: `op` on operands where at least one is not a
    /// native int/float (bool, bignum, str, list, …), or an int op overflowed.
    pub fn arith(&mut self, op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
        use NumOp::*;
        // A CPython `Foreign` operand (stdlib-ffi): `date + timedelta`,
        // `Decimal + Decimal`, `datetime < datetime`, unary `-` on a stdlib
        // object, … route through the bridge so the real CPython operation runs.
        #[cfg(feature = "stdlib-ffi")]
        {
            if matches!(op, Neg) {
                if self.foreign_id(a).is_some() {
                    return crate::ffi::unary_op(self, "neg", a);
                }
            } else if self.foreign_id(a).is_some() || self.foreign_id(b).is_some() {
                let func = match op {
                    Add => "add",
                    Sub => "sub",
                    Mul => "mul",
                    Div => "truediv",
                    Mod => "mod",
                    Pow => "pow",
                    Eq => "eq",
                    Ne => "ne",
                    Lt => "lt",
                    Le => "le",
                    Gt => "gt",
                    Ge => "ge",
                    Neg => unreachable!(),
                };
                return crate::ffi::binary_op(self, func, a, b);
            }
        }
        // Bool participates as int.
        let ai = self.as_int(a);
        let bi = self.as_int(b);
        match op {
            Add => {
                if let (Some(x), Some(y)) = (ai, bi) {
                    return Ok(self.int_result(x as i128 + y as i128));
                }
                // Bignum (both integers, exact)
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    return Ok(self.norm_big(x + y));
                }
                // Mixed/float numeric
                if let (Some(x), Some(y)) = (self.num_val(a), self.num_val(b)) {
                    return Ok(Value::Float(x + y));
                }
                // complex + complex / int + complex / …
                if self.is_complex(a) || self.is_complex(b) {
                    if let (Some((ar, ai)), Some((br, bi))) =
                        (self.complex_val(a), self.complex_val(b))
                    {
                        return Ok(self.alloc(PyObj::Complex(ar + br, ai + bi)));
                    }
                }
                // str + str, list + list, tuple + tuple
                match (self.get(a), self.get(b)) {
                    (Some(PyObj::Str(x)), Some(PyObj::Str(y))) => {
                        let s = format!("{x}{y}");
                        Ok(self.new_str(s))
                    }
                    (Some(PyObj::List(x)), Some(PyObj::List(y))) => {
                        let mut v = x.clone();
                        v.extend(y.clone());
                        Ok(self.new_list(v))
                    }
                    (Some(PyObj::Tuple(x)), Some(PyObj::Tuple(y))) => {
                        let mut v = x.clone();
                        v.extend(y.clone());
                        Ok(self.new_tuple(v))
                    }
                    // bytes/bytearray concat — the result type follows the left
                    // operand (`b'a' + bytearray(b'b')` → bytes;
                    // `bytearray(b'a') + b'b'` → bytearray).
                    (Some(PyObj::Bytes(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytes(x)), Some(PyObj::Bytearray(y))) => {
                        let mut v = x.clone();
                        v.extend_from_slice(y);
                        Ok(self.alloc(PyObj::Bytes(v)))
                    }
                    (Some(PyObj::Bytearray(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytearray(y))) => {
                        let mut v = x.clone();
                        v.extend_from_slice(y);
                        Ok(self.alloc(PyObj::Bytearray(v)))
                    }
                    // A sequence left operand with an incompatible right operand
                    // gives the type-specific concat error, not the generic
                    // "unsupported operand type(s)" one.
                    _ => {
                        let rt = self.type_name(b);
                        Err(match self.get(a) {
                            Some(PyObj::Str(_)) => type_error(&format!(
                                "can only concatenate str (not \"{rt}\") to str"
                            )),
                            Some(PyObj::List(_)) => type_error(&format!(
                                "can only concatenate list (not \"{rt}\") to list"
                            )),
                            Some(PyObj::Tuple(_)) => type_error(&format!(
                                "can only concatenate tuple (not \"{rt}\") to tuple"
                            )),
                            Some(PyObj::Bytes(_)) => {
                                type_error(&format!("can't concat {rt} to bytes"))
                            }
                            Some(PyObj::Bytearray(_)) => {
                                type_error(&format!("can't concat {rt} to bytearray"))
                            }
                            _ => self.optype_err("+", a, b),
                        })
                    }
                }
            }
            Sub => {
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    return Ok(self.norm_big(x - y));
                }
                if let (Some(x), Some(y)) = (self.num_val(a), self.num_val(b)) {
                    return Ok(Value::Float(x - y));
                }
                if self.is_complex(a) || self.is_complex(b) {
                    if let (Some((ar, ai)), Some((br, bi))) =
                        (self.complex_val(a), self.complex_val(b))
                    {
                        return Ok(self.alloc(PyObj::Complex(ar - br, ai - bi)));
                    }
                }
                // set difference (result type follows the left operand;
                // dict_keys/dict_items views participate as key-sets)
                if let (Some(mut out), Some(y)) = (self.setmap_of(a), self.setmap_of(b)) {
                    for k in y.keys() {
                        out.shift_remove(k);
                    }
                    let frozen = self.is_frozenset(a);
                    return Ok(self.new_setlike(out, frozen));
                }
                Err(self.optype_err("-", a, b))
            }
            Mul => {
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    return Ok(self.norm_big(x * y));
                }
                // str * int, list * int (either order)
                if let Some(r) = self.repeat_seq(a, b)? {
                    return Ok(r);
                }
                if let (Some(x), Some(y)) = (self.num_val(a), self.num_val(b)) {
                    return Ok(Value::Float(x * y));
                }
                if self.is_complex(a) || self.is_complex(b) {
                    if let (Some((ar, ai)), Some((br, bi))) =
                        (self.complex_val(a), self.complex_val(b))
                    {
                        return Ok(self.alloc(PyObj::Complex(ar * br - ai * bi, ar * bi + ai * br)));
                    }
                }
                Err(self.optype_err("*", a, b))
            }
            Div => self.binop(binop::DIV, a, b),
            Mod => self.binop(binop::MOD, a, b),
            Pow => self.binop(binop::POW, a, b),
            Neg => {
                if let Some(x) = self.big_val(a) {
                    return Ok(self.norm_big(-x));
                }
                if let Some(PyObj::Complex(r, i)) = self.get(a) {
                    let (r, i) = (*r, *i);
                    return Ok(self.alloc(PyObj::Complex(-r, -i)));
                }
                Err(type_error(&format!(
                    "bad operand type for unary -: '{}'",
                    self.type_name(a)
                )))
            }
            Eq => Ok(Value::Bool(self.equal(a, b))),
            Ne => Ok(Value::Bool(!self.equal(a, b))),
            Lt | Gt | Le | Ge => self.compare(op, a, b),
        }
    }

    fn optype_err(&self, op: &str, a: &Value, b: &Value) -> String {
        type_error(&format!(
            "unsupported operand type(s) for {op}: '{}' and '{}'",
            self.type_name(a),
            self.type_name(b)
        ))
    }

    fn int_result(&mut self, n: i128) -> Value {
        if let Ok(v) = i64::try_from(n) {
            Value::Int(v)
        } else {
            self.alloc(PyObj::BigInt(num_bigint::BigInt::from(n)))
        }
    }

    pub fn big_val(&self, v: &Value) -> Option<num_bigint::BigInt> {
        match v {
            Value::Int(n) => Some(num_bigint::BigInt::from(*n)),
            Value::Bool(b) => Some(num_bigint::BigInt::from(*b as i64)),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::BigInt(b)) => Some(b.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn norm_big(&mut self, b: num_bigint::BigInt) -> Value {
        use num_traits::ToPrimitive;
        if let Some(n) = b.to_i64() {
            Value::Int(n)
        } else {
            self.alloc(PyObj::BigInt(b))
        }
    }

    fn repeat_seq(&mut self, a: &Value, b: &Value) -> Result<Option<Value>, String> {
        let (seq, count) = if let Some(n) = self.as_int(b) {
            (a.clone(), n)
        } else if let Some(n) = self.as_int(a) {
            (b.clone(), n)
        } else {
            return Ok(None);
        };
        let n = count.max(0) as usize;
        match self.get(&seq) {
            Some(PyObj::Str(s)) => {
                let r = s.repeat(n);
                Ok(Some(self.new_str(r)))
            }
            Some(PyObj::List(l)) => {
                let mut out = Vec::with_capacity(l.len() * n);
                let base = l.clone();
                for _ in 0..n {
                    out.extend(base.clone());
                }
                Ok(Some(self.new_list(out)))
            }
            Some(PyObj::Tuple(l)) => {
                let base = l.clone();
                let mut out = Vec::with_capacity(base.len() * n);
                for _ in 0..n {
                    out.extend(base.clone());
                }
                Ok(Some(self.new_tuple(out)))
            }
            Some(PyObj::Bytes(s)) => {
                let base = s.clone();
                let mut out = Vec::with_capacity(base.len() * n);
                for _ in 0..n {
                    out.extend_from_slice(&base);
                }
                Ok(Some(self.alloc(PyObj::Bytes(out))))
            }
            Some(PyObj::Bytearray(s)) => {
                let base = s.clone();
                let mut out = Vec::with_capacity(base.len() * n);
                for _ in 0..n {
                    out.extend_from_slice(&base);
                }
                Ok(Some(self.alloc(PyObj::Bytearray(out))))
            }
            _ => Ok(None),
        }
    }

    /// Comparison ops for non-native operands (`<`, `>`, `<=`, `>=`).
    pub fn compare(&mut self, op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
        use std::cmp::Ordering;
        // Sets/frozensets use the subset partial order, not a total order — the
        // operands can be incomparable (`{1} < {2}` and `{1} > {2}` both False).
        if let (Some(x), Some(y)) = (self.setlike(a), self.setlike(b)) {
            let a_sub_b = x.keys().all(|k| y.contains_key(k)); // a ⊆ b
            let b_sub_a = y.keys().all(|k| x.contains_key(k)); // b ⊆ a
            let (la, lb) = (x.len(), y.len());
            let res = match op {
                NumOp::Le => a_sub_b,
                NumOp::Lt => a_sub_b && la < lb,
                NumOp::Ge => b_sub_a,
                NumOp::Gt => b_sub_a && lb < la,
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        let ord = self.order(a, b)?;
        let res = match op {
            NumOp::Lt => ord == Ordering::Less,
            NumOp::Le => ord != Ordering::Greater,
            NumOp::Gt => ord == Ordering::Greater,
            NumOp::Ge => ord != Ordering::Less,
            _ => unreachable!(),
        };
        Ok(Value::Bool(res))
    }

    fn order(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
        use std::cmp::Ordering;
        // Exact integer comparison first: two integers (either may be a bignum
        // beyond f64 precision) must compare by value, not by lossy f64.
        let a_is_float = matches!(a, Value::Float(_));
        let b_is_float = matches!(b, Value::Float(_));
        if !a_is_float && !b_is_float {
            if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                return Ok(x.cmp(&y));
            }
        }
        if let (Some(x), Some(y)) = (self.num_val(a), self.num_val(b)) {
            return Ok(x.partial_cmp(&y).unwrap_or(Ordering::Equal));
        }
        match (self.get(a), self.get(b)) {
            (Some(PyObj::Str(x)), Some(PyObj::Str(y))) => Ok(x.cmp(y)),
            // bytes/bytearray order lexicographically by byte value (a bytes and
            // a bytearray of equal content compare equal).
            (Some(PyObj::Bytes(x)), Some(PyObj::Bytes(y)))
            | (Some(PyObj::Bytes(x)), Some(PyObj::Bytearray(y)))
            | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytes(y)))
            | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytearray(y))) => Ok(x.cmp(y)),
            (Some(PyObj::List(x)), Some(PyObj::List(y)))
            | (Some(PyObj::Tuple(x)), Some(PyObj::Tuple(y))) => {
                for (p, q) in x.iter().zip(y.iter()) {
                    let o = self.order(p, q)?;
                    if o != Ordering::Equal {
                        return Ok(o);
                    }
                }
                Ok(x.len().cmp(&y.len()))
            }
            _ => Err(type_error(&format!(
                "'<' not supported between instances of '{}' and '{}'",
                self.type_name(a),
                self.type_name(b)
            ))),
        }
    }

    /// The non-native binary operators (`/ // % ** @ & | ^ << >>`).
    pub fn binop(&mut self, tag: i64, a: &Value, b: &Value) -> Result<Value, String> {
        // A CPython `Foreign` operand (stdlib-ffi) for `/ // % ** @ & | ^ << >>`
        // routes through the bridge (`Decimal / Decimal`, an `IntFlag | IntFlag`, …).
        #[cfg(feature = "stdlib-ffi")]
        if self.foreign_id(a).is_some() || self.foreign_id(b).is_some() {
            let func = match tag {
                binop::DIV => "truediv",
                binop::FLOORDIV => "floordiv",
                binop::MOD => "mod",
                binop::POW => "pow",
                binop::MATMUL => "matmul",
                binop::BITAND => "and_",
                binop::BITOR => "or_",
                binop::BITXOR => "xor",
                binop::SHL => "lshift",
                binop::SHR => "rshift",
                _ => return Err(type_error("unknown binop")),
            };
            return crate::ffi::binary_op(self, func, a, b);
        }
        let ai = self.as_int(a);
        let bi = self.as_int(b);
        let af = self.num_val(a);
        let bf = self.num_val(b);
        match tag {
            binop::DIV => match (af, bf) {
                (Some(_), Some(0.0)) => Err("ZeroDivisionError: division by zero".into()),
                (Some(x), Some(y)) => Ok(Value::Float(x / y)),
                _ if self.is_complex(a) || self.is_complex(b) => {
                    match (self.complex_val(a), self.complex_val(b)) {
                        (Some((ar, ai)), Some((br, bi))) => {
                            if br == 0.0 && bi == 0.0 {
                                return Err("ZeroDivisionError: division by zero".into());
                            }
                            let (rr, ri) = c_quot(ar, ai, br, bi);
                            Ok(self.alloc(PyObj::Complex(rr, ri)))
                        }
                        _ => Err(self.optype_err("/", a, b)),
                    }
                }
                _ => Err(self.optype_err("/", a, b)),
            },
            binop::FLOORDIV => {
                // Python `//` floors toward −∞ (not Rust truncation).
                if let (Some(x), Some(y)) = (ai, bi) {
                    if y == 0 {
                        return Err("ZeroDivisionError: division by zero".into());
                    }
                    let (x, y) = (x as i128, y as i128);
                    let q = x / y;
                    let r = x % y;
                    let q = if r != 0 && (r < 0) != (y < 0) {
                        q - 1
                    } else {
                        q
                    };
                    return Ok(self.int_result(q));
                }
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    if y == num_bigint::BigInt::from(0) {
                        return Err("ZeroDivisionError: division by zero".into());
                    }
                    return Ok(self.norm_big(bigint_floordiv(&x, &y)));
                }
                match (af, bf) {
                    (Some(_), Some(0.0)) => Err("ZeroDivisionError: division by zero".into()),
                    (Some(x), Some(y)) => Ok(Value::Float(float_divmod(x, y).0)),
                    _ => Err(self.optype_err("//", a, b)),
                }
            }
            binop::MOD => {
                // str % formatting. Reached only via internal numeric fallbacks
                // (the `str % args` opcode path pre-resolves instance dispatch in
                // `b_binop` and calls `str_format_percent` directly); an empty
                // premap here keeps the non-dispatching fallback behavior.
                if let Some(PyObj::Str(fmt)) = self.get(a) {
                    let fmt = fmt.clone();
                    return self.str_format_percent(&fmt, b, &HashMap::new());
                }
                // Python `%` takes the sign of the divisor (floored remainder).
                if let (Some(x), Some(y)) = (ai, bi) {
                    if y == 0 {
                        return Err("ZeroDivisionError: division by zero".into());
                    }
                    let r = x % y;
                    let r = if r != 0 && (r < 0) != (y < 0) {
                        r + y
                    } else {
                        r
                    };
                    return Ok(Value::Int(r));
                }
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    if y == num_bigint::BigInt::from(0) {
                        return Err("ZeroDivisionError: division by zero".into());
                    }
                    return Ok(self.norm_big(bigint_mod(&x, &y)));
                }
                match (af, bf) {
                    (Some(_), Some(0.0)) => Err("ZeroDivisionError: division by zero".into()),
                    (Some(x), Some(y)) => Ok(Value::Float(float_divmod(x, y).1)),
                    _ => Err(self.optype_err("%", a, b)),
                }
            }
            binop::POW => match (ai, bi) {
                (Some(x), Some(y)) if y >= 0 => {
                    let mut acc = num_bigint::BigInt::from(1);
                    let base = num_bigint::BigInt::from(x);
                    for _ in 0..y {
                        acc *= &base;
                    }
                    Ok(self.norm_big(acc))
                }
                _ if self.is_complex(a) || self.is_complex(b) => {
                    match (self.complex_val(a), self.complex_val(b)) {
                        (Some(x), Some(y)) => {
                            let (r, i) = c_pow(x, y);
                            Ok(self.alloc(PyObj::Complex(r, i)))
                        }
                        _ => Err(self.optype_err("**", a, b)),
                    }
                }
                _ => match (af, bf) {
                    // `0 ** <negative>` is a division by zero in CPython, not `inf`.
                    // As of 3.14 both int and float bases word it the same way.
                    (Some(x), Some(y)) if x == 0.0 && y < 0.0 => {
                        Err("ZeroDivisionError: zero to a negative power".into())
                    }
                    // A negative real base raised to a non-integer power yields a
                    // complex result in CPython (Rust's `powf` returns NaN).
                    (Some(x), Some(y)) if x < 0.0 && y.fract() != 0.0 => {
                        let (r, i) = c_pow((x, 0.0), (y, 0.0));
                        Ok(self.alloc(PyObj::Complex(r, i)))
                    }
                    (Some(x), Some(y)) => Ok(Value::Float(x.powf(y))),
                    _ => Err(self.optype_err("**", a, b)),
                },
            },
            binop::BITAND | binop::BITOR | binop::BITXOR => {
                // dict merge: `d1 | d2` → a new dict (right operand wins on key clash).
                if tag == binop::BITOR {
                    if let (Some(PyObj::Dict(x)), Some(PyObj::Dict(y))) = (self.get(a), self.get(b))
                    {
                        let mut out = x.clone();
                        for (k, (kv, vv)) in y.clone() {
                            dict_put(&mut out, k, kv, vv);
                        }
                        return Ok(self.new_dict(out));
                    }
                }
                // set operations (result type follows the left operand;
                // dict_keys/dict_items views participate as key-sets)
                if let (Some(x), Some(y)) = (self.setmap_of(a), self.setmap_of(b)) {
                    let mut out = IndexMap::new();
                    match tag {
                        binop::BITAND => {
                            for (k, v) in &x {
                                if y.contains_key(k) {
                                    out.insert(k.clone(), v.clone());
                                }
                            }
                        }
                        binop::BITOR => {
                            out = x.clone();
                            for (k, v) in &y {
                                out.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                        _ => {
                            for (k, v) in &x {
                                if !y.contains_key(k) {
                                    out.insert(k.clone(), v.clone());
                                }
                            }
                            for (k, v) in &y {
                                if !x.contains_key(k) {
                                    out.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                    let frozen = self.is_frozenset(a);
                    return Ok(self.new_setlike(out, frozen));
                }
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    let res = match tag {
                        binop::BITAND => x & y,
                        binop::BITOR => x | y,
                        _ => x ^ y,
                    };
                    // `bool op bool` stays `bool` (True & False → False).
                    if matches!(a, Value::Bool(_)) && matches!(b, Value::Bool(_)) {
                        use num_traits::Zero;
                        return Ok(Value::Bool(!res.is_zero()));
                    }
                    return Ok(self.norm_big(res));
                }
                Err(self.optype_err("bitop", a, b))
            }
            binop::SHL | binop::SHR => {
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    use num_bigint::Sign;
                    use num_traits::ToPrimitive;
                    if y.sign() == Sign::Minus {
                        return Err("ValueError: negative shift count".into());
                    }
                    let sh = match y.to_usize() {
                        Some(s) => s,
                        None => return Err("OverflowError: too many digits in integer".into()),
                    };
                    let res = if tag == binop::SHL { x << sh } else { x >> sh };
                    return Ok(self.norm_big(res));
                }
                Err(self.optype_err("shift", a, b))
            }
            binop::MATMUL => Err(self.optype_err("@", a, b)),
            _ => Err(type_error("unknown binop")),
        }
    }

    /// `~x` / unary `+x`.
    pub fn unary(&mut self, tag: i64, v: &Value) -> Result<Value, String> {
        // `~x` / unary `+x` on a CPython `Foreign` object (stdlib-ffi) routes
        // through the bridge (an `IntFlag`'s `~`, a `Decimal`'s unary `+`, …).
        #[cfg(feature = "stdlib-ffi")]
        if self.foreign_id(v).is_some() {
            let func = match tag {
                unop::INVERT => "invert",
                unop::POS => "pos",
                _ => return Err(type_error("unknown unary op")),
            };
            return crate::ffi::unary_op(self, func, v);
        }
        match tag {
            unop::INVERT => match self.big_val(v) {
                // `~x == -x - 1` (two's-complement), bignum-safe.
                Some(n) => Ok(self.norm_big(-(n + num_bigint::BigInt::from(1)))),
                None => Err(type_error(&format!(
                    "bad operand type for unary ~: '{}'",
                    self.type_name(v)
                ))),
            },
            unop::POS => match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => Ok(v.clone()),
                _ if self.num_val(v).is_some() => Ok(v.clone()),
                _ => Err(type_error(&format!(
                    "bad operand type for unary +: '{}'",
                    self.type_name(v)
                ))),
            },
            _ => Err(type_error("unknown unary op")),
        }
    }

    /// Minimal printf-style `%` formatting for `str % args`.
    /// `str % args` — CPython printf-style formatting. Supports the mapping form
    /// `'%(k)s' % {…}`, single-arg vs tuple positional args, conversions
    /// `d i u s r a f F e E g G x X o c %`, the flags `- + space 0 #`, field
    /// width and `.precision` (both as literals or `*` dynamic from the args).
    /// `str % args`. `premap` carries the dispatched `str()`/`repr()`/`ascii()`
    /// of any user instance or instance-bearing container among the format args,
    /// pre-resolved *outside* the host borrow (this method runs inside it and so
    /// cannot itself call back into `__str__`/`__repr__`). Keyed by heap id.
    pub fn str_format_percent(
        &mut self,
        fmt: &str,
        args: &Value,
        premap: &HashMap<u32, (String, String, String)>,
    ) -> Result<Value, String> {
        let is_mapping = matches!(self.get(args), Some(PyObj::Dict(_)));
        let arglist: Vec<Value> = if is_mapping {
            vec![]
        } else {
            match self.get(args) {
                Some(PyObj::Tuple(t)) => t.clone(),
                _ => vec![args.clone()],
            }
        };
        let chars: Vec<char> = fmt.chars().collect();
        let n = chars.len();
        let mut out = String::new();
        let mut ai = 0usize;
        let mut i = 0usize;
        while i < n {
            if chars[i] != '%' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            i += 1;
            if i >= n {
                return Err("ValueError: incomplete format".into());
            }
            if chars[i] == '%' {
                out.push('%');
                i += 1;
                continue;
            }
            // Mapping key `%(name)s`.
            let mut mapping_key: Option<String> = None;
            if chars[i] == '(' {
                i += 1;
                let mut key = String::new();
                let mut depth = 1;
                while i < n && depth > 0 {
                    match chars[i] {
                        '(' => {
                            depth += 1;
                            key.push('(');
                        }
                        ')' => {
                            depth -= 1;
                            if depth > 0 {
                                key.push(')');
                            }
                        }
                        c => key.push(c),
                    }
                    i += 1;
                }
                mapping_key = Some(key);
            }
            // Flags.
            let (mut f_minus, mut f_plus, mut f_space, mut f_zero, mut f_hash) =
                (false, false, false, false, false);
            while i < n {
                match chars[i] {
                    '-' => f_minus = true,
                    '+' => f_plus = true,
                    ' ' => f_space = true,
                    '0' => f_zero = true,
                    '#' => f_hash = true,
                    _ => break,
                }
                i += 1;
            }
            // Width (literal or `*`).
            let mut width: Option<usize> = None;
            if i < n && chars[i] == '*' {
                i += 1;
                let w = self.next_arg_int(&arglist, &mut ai);
                if w < 0 {
                    f_minus = true;
                    width = Some((-w) as usize);
                } else {
                    width = Some(w as usize);
                }
            } else {
                let mut wd = String::new();
                while i < n && chars[i].is_ascii_digit() {
                    wd.push(chars[i]);
                    i += 1;
                }
                if !wd.is_empty() {
                    width = wd.parse().ok();
                }
            }
            // Precision (literal or `*`).
            let mut prec: Option<usize> = None;
            if i < n && chars[i] == '.' {
                i += 1;
                if i < n && chars[i] == '*' {
                    i += 1;
                    prec = Some(self.next_arg_int(&arglist, &mut ai).max(0) as usize);
                } else {
                    let mut pd = String::new();
                    while i < n && chars[i].is_ascii_digit() {
                        pd.push(chars[i]);
                        i += 1;
                    }
                    prec = Some(pd.parse().unwrap_or(0));
                }
            }
            // Length modifiers are accepted and ignored.
            while i < n && matches!(chars[i], 'h' | 'l' | 'L') {
                i += 1;
            }
            if i >= n {
                return Err("ValueError: incomplete format".into());
            }
            let conv = chars[i];
            i += 1;
            // Resolve the value for this conversion.
            let val = if let Some(key) = &mapping_key {
                let kv = self.new_str(key.clone());
                let k = self.to_key(&kv)?;
                let found = match self.get(args) {
                    Some(PyObj::Dict(d)) => d.get(&k).map(|(_, v)| v.clone()),
                    _ => return Err("TypeError: format requires a mapping".into()),
                };
                match found {
                    Some(v) => v,
                    None => return Err(self.key_error(&kv)),
                }
            } else {
                let v = arglist.get(ai).cloned().ok_or_else(|| {
                    "TypeError: not enough arguments for format string".to_string()
                })?;
                ai += 1;
                v
            };
            let core = self.format_conv(
                conv,
                &val,
                ConvFlags {
                    plus: f_plus,
                    space: f_space,
                    hash: f_hash,
                },
                prec,
                premap,
            )?;
            out.push_str(&pad_conv(
                &core,
                width,
                f_minus,
                f_zero,
                is_numeric_conv(conv),
            ));
        }
        if !is_mapping && ai < arglist.len() {
            return Err("TypeError: not all arguments converted during string formatting".into());
        }
        Ok(self.new_str(out))
    }

    /// `bytes % args` / `bytearray % args` — PEP 461 percent formatting. Mirrors
    /// `str_format_percent` but the template and result are raw bytes and the
    /// conversions follow bytes semantics: `%b`/`%s` take a bytes-like object,
    /// `%c` an int in `0..=256` or a length-1 bytes-like, `%a`/`%r` produce the
    /// ASCII repr, and the numeric conversions reuse `format_conv`. `is_ba`
    /// selects a `bytearray` result to match the receiver.
    pub fn bytes_format_percent(
        &mut self,
        fmt: &[u8],
        args: &Value,
        is_ba: bool,
        premap: &std::collections::HashMap<u32, Vec<u8>>,
    ) -> Result<Value, String> {
        let is_mapping = matches!(self.get(args), Some(PyObj::Dict(_)));
        let arglist: Vec<Value> = if is_mapping {
            vec![]
        } else {
            match self.get(args) {
                Some(PyObj::Tuple(t)) => t.clone(),
                _ => vec![args.clone()],
            }
        };
        let n = fmt.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let mut ai = 0usize;
        let mut i = 0usize;
        while i < n {
            if fmt[i] != b'%' {
                out.push(fmt[i]);
                i += 1;
                continue;
            }
            i += 1;
            if i >= n {
                return Err("ValueError: incomplete format".into());
            }
            if fmt[i] == b'%' {
                out.push(b'%');
                i += 1;
                continue;
            }
            // Mapping key `%(name)s` (the key is a bytes object).
            let mut mapping_key: Option<Vec<u8>> = None;
            if fmt[i] == b'(' {
                i += 1;
                let mut key = Vec::new();
                let mut depth = 1;
                while i < n && depth > 0 {
                    match fmt[i] {
                        b'(' => {
                            depth += 1;
                            key.push(b'(');
                        }
                        b')' => {
                            depth -= 1;
                            if depth > 0 {
                                key.push(b')');
                            }
                        }
                        c => key.push(c),
                    }
                    i += 1;
                }
                mapping_key = Some(key);
            }
            // Flags.
            let (mut f_minus, mut f_plus, mut f_space, mut f_zero, mut f_hash) =
                (false, false, false, false, false);
            while i < n {
                match fmt[i] {
                    b'-' => f_minus = true,
                    b'+' => f_plus = true,
                    b' ' => f_space = true,
                    b'0' => f_zero = true,
                    b'#' => f_hash = true,
                    _ => break,
                }
                i += 1;
            }
            // Width (literal or `*`).
            let mut width: Option<usize> = None;
            if i < n && fmt[i] == b'*' {
                i += 1;
                let w = self.next_arg_int(&arglist, &mut ai);
                if w < 0 {
                    f_minus = true;
                    width = Some((-w) as usize);
                } else {
                    width = Some(w as usize);
                }
            } else {
                let mut wd = String::new();
                while i < n && fmt[i].is_ascii_digit() {
                    wd.push(fmt[i] as char);
                    i += 1;
                }
                if !wd.is_empty() {
                    width = wd.parse().ok();
                }
            }
            // Precision (literal or `*`).
            let mut prec: Option<usize> = None;
            if i < n && fmt[i] == b'.' {
                i += 1;
                if i < n && fmt[i] == b'*' {
                    i += 1;
                    prec = Some(self.next_arg_int(&arglist, &mut ai).max(0) as usize);
                } else {
                    let mut pd = String::new();
                    while i < n && fmt[i].is_ascii_digit() {
                        pd.push(fmt[i] as char);
                        i += 1;
                    }
                    prec = Some(pd.parse().unwrap_or(0));
                }
            }
            // Length modifiers are accepted and ignored.
            while i < n && matches!(fmt[i], b'h' | b'l' | b'L') {
                i += 1;
            }
            if i >= n {
                return Err("ValueError: incomplete format".into());
            }
            let conv = fmt[i] as char;
            i += 1;
            // Resolve the value for this conversion.
            let val = if let Some(key) = &mapping_key {
                let kv = self.alloc(PyObj::Bytes(key.clone()));
                let k = self.to_key(&kv)?;
                let found = match self.get(args) {
                    Some(PyObj::Dict(d)) => d.get(&k).map(|(_, v)| v.clone()),
                    _ => return Err("TypeError: format requires a mapping".into()),
                };
                match found {
                    Some(v) => v,
                    None => return Err(self.key_error(&kv)),
                }
            } else {
                let v = arglist.get(ai).cloned().ok_or_else(|| {
                    "TypeError: not enough arguments for format string".to_string()
                })?;
                ai += 1;
                v
            };
            let (core, numeric): (Vec<u8>, bool) = match conv {
                'b' | 's' => {
                    let mut raw = match self.get(&val) {
                        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
                        // A user instance's `__bytes__` was pre-resolved outside
                        // the borrow into `premap`, keyed by heap id.
                        _ => match &val {
                            Value::Obj(id) if premap.contains_key(id) => premap[id].clone(),
                            _ => {
                                return Err(type_error(&format!(
                            "%b requires a bytes-like object, or an object that implements __bytes__, not '{}'",
                            self.type_name(&val)
                        )))
                            }
                        },
                    };
                    if let Some(p) = prec {
                        raw.truncate(p);
                    }
                    (raw, false)
                }
                'a' | 'r' => {
                    // Both force an ASCII rendering of the repr.
                    let mut s = ascii_of(&self.repr_of(&val));
                    if let Some(p) = prec {
                        s = s.chars().take(p).collect();
                    }
                    (s.into_bytes(), false)
                }
                'c' => {
                    if let Some(cp) = self.as_int(&val) {
                        if !(0..=255).contains(&cp) {
                            return Err(
                                "TypeError: %c requires an integer in range(256) or a single byte"
                                    .into(),
                            );
                        }
                        (vec![cp as u8], false)
                    } else {
                        let raw = match self.get(&val) {
                            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
                            _ => None,
                        };
                        match raw {
                            Some(b) if b.len() == 1 => (vec![b[0]], false),
                            Some(b) => return Err(format!(
                                "TypeError: %c requires an integer in range(256) or a single byte, not a bytes object of length {}",
                                b.len()
                            )),
                            None => return Err(type_error(&format!(
                                "%c requires an integer in range(256) or a single byte, not {}",
                                self.type_name(&val)
                            ))),
                        }
                    }
                }
                'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                    let s = self.format_conv(
                        conv,
                        &val,
                        ConvFlags {
                            plus: f_plus,
                            space: f_space,
                            hash: f_hash,
                        },
                        prec,
                        &HashMap::new(),
                    )?;
                    (s.into_bytes(), true)
                }
                other => {
                    return Err(format!(
                        "ValueError: unsupported format character '{other}'"
                    ))
                }
            };
            out.extend_from_slice(&pad_conv_bytes(&core, width, f_minus, f_zero, numeric));
        }
        if !is_mapping && ai < arglist.len() {
            return Err("TypeError: not all arguments converted during bytes formatting".into());
        }
        Ok(if is_ba {
            self.alloc(PyObj::Bytearray(out))
        } else {
            self.alloc(PyObj::Bytes(out))
        })
    }

    /// Pop the next positional arg as an i64 (for `*` width/precision).
    fn next_arg_int(&self, arglist: &[Value], ai: &mut usize) -> i64 {
        let v = arglist.get(*ai).cloned().unwrap_or(Value::Int(0));
        *ai += 1;
        self.as_int(&v).unwrap_or(0)
    }

    /// Render one `%`-conversion's core text (sign included, width padding not).
    fn format_conv(
        &mut self,
        conv: char,
        val: &Value,
        flags: ConvFlags,
        prec: Option<usize>,
        premap: &HashMap<u32, (String, String, String)>,
    ) -> Result<String, String> {
        let ConvFlags { plus, space, hash } = flags;
        let sign_str = |neg: bool| -> &'static str {
            if neg {
                "-"
            } else if plus {
                "+"
            } else if space {
                " "
            } else {
                ""
            }
        };
        match conv {
            's' | 'r' | 'a' => {
                // Prefer the pre-resolved dispatched value for a user instance /
                // instance-bearing container; fall back to the non-dispatching
                // host renderers for plain values.
                let mut s = match val {
                    Value::Obj(id) if premap.contains_key(id) => {
                        let (sr, rp, asc) = &premap[id];
                        match conv {
                            's' => sr.clone(),
                            'r' => rp.clone(),
                            _ => asc.clone(),
                        }
                    }
                    _ => match conv {
                        's' => self.str_of(val),
                        'r' => self.repr_of(val),
                        _ => ascii_of(&self.repr_of(val)),
                    },
                };
                if let Some(p) = prec {
                    s = s.chars().take(p).collect();
                }
                Ok(s)
            }
            'c' => {
                if let Some(cp) = self.as_int(val) {
                    let ch = char::from_u32(cp as u32)
                        .ok_or_else(|| "OverflowError: %c arg not in range".to_string())?;
                    Ok(ch.to_string())
                } else if let Some(s) = self.as_str(val) {
                    if s.chars().count() == 1 {
                        Ok(s)
                    } else {
                        Err("TypeError: %c requires int or char".into())
                    }
                } else {
                    Err("TypeError: %c requires int or char".into())
                }
            }
            'd' | 'i' | 'u' | 'x' | 'X' | 'o' => {
                use num_traits::Signed;
                // `%d/%i/%u` accept a float (truncated toward zero); `%x/%X/%o`
                // require an integer.
                let b = match self.big_val(val) {
                    Some(b) => b,
                    None if matches!(conv, 'd' | 'i' | 'u') => match self.num_val(val) {
                        Some(f) => num_bigint::BigInt::from(f.trunc() as i64),
                        None => {
                            return Err(type_error(&format!(
                                "%{conv} format: a real number is required, not {}",
                                self.type_name(val)
                            )))
                        }
                    },
                    None => {
                        return Err(type_error(&format!(
                            "%{conv} format: an integer is required, not {}",
                            self.type_name(val)
                        )))
                    }
                };
                let neg = b.is_negative();
                let abs = b.abs();
                let (mut digits, prefix) = match conv {
                    'x' => (abs.to_str_radix(16), if hash { "0x" } else { "" }),
                    'X' => (
                        abs.to_str_radix(16).to_uppercase(),
                        if hash { "0X" } else { "" },
                    ),
                    'o' => (abs.to_str_radix(8), if hash { "0o" } else { "" }),
                    _ => (abs.to_str_radix(10), ""),
                };
                if let Some(p) = prec {
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                }
                // Python (unlike C printf) keeps the `#` radix prefix even for a
                // zero value: `'%#x' % 0` → `0x0`, `'%#o' % 0` → `0o0`.
                Ok(format!("{}{}{}", sign_str(neg), prefix, digits))
            }
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                let x = self.num_val(val).ok_or_else(|| {
                    type_error(&format!(
                        "%{conv} format: a real number is required, not {}",
                        self.type_name(val)
                    ))
                })?;
                let neg = x.is_sign_negative();
                if x.is_nan() {
                    return Ok(format!(
                        "{}{}",
                        sign_str(false),
                        if conv.is_uppercase() { "NAN" } else { "nan" }
                    ));
                }
                if x.is_infinite() {
                    return Ok(format!(
                        "{}{}",
                        sign_str(neg),
                        if conv.is_uppercase() { "INF" } else { "inf" }
                    ));
                }
                let mag = x.abs();
                let core = match conv {
                    'f' | 'F' => format!("{:.*}", prec.unwrap_or(6), mag),
                    'e' => fmt_sci(mag, prec.unwrap_or(6), false),
                    'E' => fmt_sci(mag, prec.unwrap_or(6), true),
                    'g' => fmt_g(mag, prec.unwrap_or(6), false, hash),
                    _ => fmt_g(mag, prec.unwrap_or(6), true, hash),
                };
                Ok(format!("{}{}", sign_str(neg), core))
            }
            other => Err(format!(
                "ValueError: unsupported format character '{other}'"
            )),
        }
    }
}

/// Whether a `%`-conversion produces a number (eligible for `0`-fill / sign).
fn is_numeric_conv(c: char) -> bool {
    matches!(
        c,
        'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G'
    )
}

/// Pad a rendered conversion to `width`. Left-justify with `-`; else zero-fill
/// numeric conversions (keeping the sign/base prefix leading) when `zero`; else
/// right-justify with spaces.
fn pad_conv(core: &str, width: Option<usize>, minus: bool, zero: bool, numeric: bool) -> String {
    let w = match width {
        Some(w) => w,
        None => return core.to_string(),
    };
    let len = core.chars().count();
    if len >= w {
        return core.to_string();
    }
    let pad = w - len;
    if minus {
        format!("{core}{}", " ".repeat(pad))
    } else if zero && numeric {
        let (prefix, rest) = split_sign_prefix(core);
        format!("{prefix}{}{rest}", "0".repeat(pad))
    } else {
        format!("{}{core}", " ".repeat(pad))
    }
}

/// Byte-level [`pad_conv`] for `bytes`/`bytearray` `%`-formatting. Padding is
/// measured in bytes; numeric zero-fill lands after any sign/base prefix.
fn pad_conv_bytes(
    core: &[u8],
    width: Option<usize>,
    minus: bool,
    zero: bool,
    numeric: bool,
) -> Vec<u8> {
    let w = match width {
        Some(w) => w,
        None => return core.to_vec(),
    };
    if core.len() >= w {
        return core.to_vec();
    }
    let pad = w - core.len();
    if minus {
        let mut v = core.to_vec();
        v.extend(std::iter::repeat(b' ').take(pad));
        v
    } else if zero && numeric {
        let (prefix, rest) = split_sign_prefix_bytes(core);
        let mut v = prefix.to_vec();
        v.extend(std::iter::repeat(b'0').take(pad));
        v.extend_from_slice(rest);
        v
    } else {
        let mut v: Vec<u8> = std::iter::repeat(b' ').take(pad).collect();
        v.extend_from_slice(core);
        v
    }
}

/// Byte-level [`split_sign_prefix`]: split a leading sign and `0x`/`0X`/`0o`
/// base prefix off a rendered number.
fn split_sign_prefix_bytes(s: &[u8]) -> (&[u8], &[u8]) {
    let mut idx = 0;
    if let Some(&c) = s.first() {
        if c == b'+' || c == b'-' || c == b' ' {
            idx = 1;
        }
    }
    if s.len() >= idx + 2 && s[idx] == b'0' && matches!(s[idx + 1], b'x' | b'X' | b'o') {
        idx += 2;
    }
    (&s[..idx], &s[idx..])
}

/// Split a leading sign (`+ - space`) and numeric base prefix (`0x`/`0X`/`0o`)
/// off a rendered number, so `0`-fill lands after them.
fn split_sign_prefix(s: &str) -> (String, &str) {
    let mut idx = 0;
    let bytes: Vec<char> = s.chars().collect();
    let mut prefix = String::new();
    if let Some(&c) = bytes.first() {
        if c == '+' || c == '-' || c == ' ' {
            prefix.push(c);
            idx = 1;
        }
    }
    if bytes.len() >= idx + 2 && bytes[idx] == '0' && matches!(bytes[idx + 1], 'x' | 'X' | 'o') {
        prefix.push('0');
        prefix.push(bytes[idx + 1]);
        idx += 2;
    }
    let byte_off: usize = s.chars().take(idx).map(|c| c.len_utf8()).sum();
    (prefix, &s[byte_off..])
}

/// `%e` / `%E` scientific form with Python's exponent shape (`e[+-]NN`, ≥2 digits).
pub fn fmt_sci(x: f64, prec: usize, upper: bool) -> String {
    let s = format!("{:.*e}", prec, x);
    let (mant, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let exp_num: i32 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    format!(
        "{mant}{e}{}{:02}",
        if exp_num < 0 { '-' } else { '+' },
        exp_num.abs()
    )
}

/// `%g` / `%G`: choose `f` or `e` style by exponent, `precision` significant
/// digits (min 1), trailing zeros stripped unless the `#` flag is set.
pub fn fmt_g(x: f64, prec: usize, upper: bool, hash: bool) -> String {
    let p = prec.max(1);
    if x == 0.0 {
        return "0".to_string();
    }
    let exp: i32 = format!("{:e}", x)
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let mut s = fmt_sci(x, p - 1, upper);
        if !hash {
            s = strip_g_sci(&s);
        }
        s
    } else {
        let dec = (p as i32 - 1 - exp).max(0) as usize;
        let mut s = format!("{:.*}", dec, x);
        if !hash && s.contains('.') {
            s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        s
    }
}

/// Strip trailing zeros from the mantissa of a `%g` scientific result.
fn strip_g_sci(s: &str) -> String {
    match s.find(['e', 'E']) {
        Some(pos) => {
            let (mant, exp) = s.split_at(pos);
            let mant = if mant.contains('.') {
                mant.trim_end_matches('0').trim_end_matches('.')
            } else {
                mant
            };
            format!("{mant}{exp}")
        }
        None => s.to_string(),
    }
}

/// `%a` (ascii): non-ASCII code points in a repr escaped as `\xNN`/`\uNNNN`/`\UNNNNNNNN`.
pub fn ascii_of(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if c.is_ascii() {
            o.push(c);
        } else {
            let u = c as u32;
            if u <= 0xff {
                o.push_str(&format!("\\x{u:02x}"));
            } else if u <= 0xffff {
                o.push_str(&format!("\\u{u:04x}"));
            } else {
                o.push_str(&format!("\\U{u:08x}"));
            }
        }
    }
    o
}

// ── indexing / iteration / containment ───────────────────────────────────────

impl PyHost {
    /// The current bytes a `memoryview` exposes, read from its live backing
    /// `bytes`/`bytearray` object (so a view over a `bytearray` reflects
    /// mutations). Empty for a non-memoryview or a stale/out-of-bounds window.
    pub fn mv_bytes(&self, recv: &Value) -> Vec<u8> {
        if let Some(PyObj::Memoryview {
            obj, start, len, ..
        }) = self.get(recv)
        {
            let (start, len) = (*start, *len);
            if let Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) = self.get(obj) {
                return b
                    .get(start..start + len)
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
            }
        }
        Vec::new()
    }

    /// `recv[idx]`.
    pub fn get_item(&mut self, recv: &Value, idx: &Value) -> Result<Value, String> {
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::get_item(self, id, idx);
        }
        // Slice?
        if let Some(PyObj::Slice { lo, hi, step }) = self.get(idx) {
            let (lo, hi, step) = (lo.clone(), hi.clone(), step.clone());
            return self.get_slice(recv, &lo, &hi, &step);
        }
        match self.get(recv) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => {
                let is_tuple = matches!(self.get(recv), Some(PyObj::Tuple(_)));
                let n = l.len() as i64;
                let i = self.as_int(idx).ok_or_else(|| {
                    let ty = if is_tuple { "tuple" } else { "list" };
                    type_error(&format!(
                        "{ty} indices must be integers or slices, not {}",
                        self.type_name(idx)
                    ))
                })?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    let ty = if is_tuple { "tuple" } else { "list" };
                    return Err(format!("IndexError: {ty} index out of range"));
                }
                Ok(l[k as usize].clone())
            }
            Some(PyObj::Str(s)) => {
                let chars: Vec<char> = s.chars().collect();
                let n = chars.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("string indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: string index out of range".into());
                }
                let ch = chars[k as usize].to_string();
                Ok(self.new_str(ch))
            }
            Some(PyObj::Dict(d)) => {
                let key = self.to_key(idx)?;
                let found = d.get(&key).map(|(_, v)| v.clone());
                match found {
                    Some(v) => Ok(v),
                    None => Err(self.key_error(idx)),
                }
            }
            Some(PyObj::Range { start, step, .. }) => {
                let (start, step) = (*start, *step);
                let len = match self.get(recv) {
                    Some(PyObj::Range { start, stop, step }) => range_len(*start, *stop, *step),
                    _ => 0,
                };
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("range indices must be integers"))?;
                let k = if i < 0 { i + len } else { i };
                if k < 0 || k >= len {
                    return Err("IndexError: range object index out of range".into());
                }
                Ok(Value::Int(start + k * step))
            }
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => {
                // `bytes` reports the bare message; `bytearray` names the type.
                let is_ba = matches!(self.get(recv), Some(PyObj::Bytearray(_)));
                let n = b.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("byte indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err(if is_ba {
                        "IndexError: bytearray index out of range".into()
                    } else {
                        "IndexError: index out of range".into()
                    });
                }
                Ok(Value::Int(b[k as usize] as i64))
            }
            Some(PyObj::Deque { items, .. }) => {
                let n = items.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("deque indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: deque index out of range".into());
                }
                Ok(items[k as usize].clone())
            }
            Some(PyObj::Memoryview { .. }) => {
                let bytes = self.mv_bytes(recv);
                let n = bytes.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("memoryview: invalid slice key"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: index out of bounds on dimension 1".into());
                }
                Ok(Value::Int(bytes[k as usize] as i64))
            }
            _ => Err(type_error(&format!(
                "'{}' object is not subscriptable",
                self.type_name(recv)
            ))),
        }
    }

    fn get_slice(
        &mut self,
        recv: &Value,
        lo: &Value,
        hi: &Value,
        step: &Value,
    ) -> Result<Value, String> {
        let step = self.as_int(step).unwrap_or(1);
        if step == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        // Slicing a range yields a new range (never materializes).
        if let Some(PyObj::Range {
            start: rstart,
            stop: rstop,
            step: rstep,
        }) = self.get(recv)
        {
            let (rstart, rstep) = (*rstart, *rstep);
            let len = range_len(rstart, *rstop, rstep);
            let (i, j) = slice_bounds(lo, hi, step, len, self);
            return Ok(self.alloc(PyObj::Range {
                start: rstart + i * rstep,
                stop: rstart + j * rstep,
                step: rstep * step,
            }));
        }
        // Slicing a memoryview yields another memoryview. A contiguous
        // (`step == 1`) slice is a sub-view sharing the backing buffer; a
        // strided slice materializes a fresh read-only byte buffer to view.
        if let Some(PyObj::Memoryview {
            obj,
            start,
            len,
            readonly,
        }) = self.get(recv)
        {
            let (obj, start, readonly) = (obj.clone(), *start, *readonly);
            let n = *len as i64;
            let (mut i, stop) = slice_bounds(lo, hi, step, n, self);
            if step == 1 {
                let lo_i = i.clamp(0, n);
                let hi_i = stop.clamp(lo_i, n);
                return Ok(self.alloc(PyObj::Memoryview {
                    obj,
                    start: start + lo_i as usize,
                    len: (hi_i - lo_i) as usize,
                    readonly,
                }));
            }
            let src = self.mv_bytes(recv);
            let mut out = Vec::new();
            if step > 0 {
                while i < stop {
                    if i >= 0 && i < n {
                        out.push(src[i as usize]);
                    }
                    i += step;
                }
            } else {
                while i > stop {
                    if i >= 0 && i < n {
                        out.push(src[i as usize]);
                    }
                    i += step;
                }
            }
            let len = out.len();
            let buf = self.alloc(PyObj::Bytes(out));
            return Ok(self.alloc(PyObj::Memoryview {
                obj: buf,
                start: 0,
                len,
                readonly: true,
            }));
        }
        // Slicing bytes/bytearray yields a new buffer of the same type.
        if let Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) = self.get(recv) {
            let is_ba = matches!(self.get(recv), Some(PyObj::Bytearray(_)));
            let src = b.clone();
            let n = src.len() as i64;
            let (mut i, stop) = slice_bounds(lo, hi, step, n, self);
            let mut out = Vec::new();
            if step > 0 {
                while i < stop {
                    if i >= 0 && i < n {
                        out.push(src[i as usize]);
                    }
                    i += step;
                }
            } else {
                while i > stop {
                    if i >= 0 && i < n {
                        out.push(src[i as usize]);
                    }
                    i += step;
                }
            }
            return Ok(if is_ba {
                self.alloc(PyObj::Bytearray(out))
            } else {
                self.alloc(PyObj::Bytes(out))
            });
        }
        let is_str = matches!(self.get(recv), Some(PyObj::Str(_)));
        let items: Vec<Value> = match self.get(recv) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => l.clone(),
            Some(PyObj::Str(s)) => s.chars().map(|c| Value::Int(c as i64)).collect(),
            _ => return Err(type_error("object is not subscriptable")),
        };
        let n = items.len() as i64;
        let (mut i, stop) = slice_bounds(lo, hi, step, n, self);
        let mut out = Vec::new();
        if step > 0 {
            while i < stop {
                if i >= 0 && i < n {
                    out.push(items[i as usize].clone());
                }
                i += step;
            }
        } else {
            while i > stop {
                if i >= 0 && i < n {
                    out.push(items[i as usize].clone());
                }
                i += step;
            }
        }
        if is_str {
            if let Some(PyObj::Str(s)) = self.get(recv) {
                let chars: Vec<char> = s.chars().collect();
                let mut r = String::new();
                let (mut i2, stop2) = slice_bounds(lo, hi, step, n, self);
                if step > 0 {
                    while i2 < stop2 {
                        if i2 >= 0 && i2 < n {
                            r.push(chars[i2 as usize]);
                        }
                        i2 += step;
                    }
                } else {
                    while i2 > stop2 {
                        if i2 >= 0 && i2 < n {
                            r.push(chars[i2 as usize]);
                        }
                        i2 += step;
                    }
                }
                return Ok(self.new_str(r));
            }
        }
        // Tuple slices stay tuples.
        if matches!(self.get(recv), Some(PyObj::Tuple(_))) {
            Ok(self.new_tuple(out))
        } else {
            Ok(self.new_list(out))
        }
    }

    /// `recv[idx] = val`.
    pub fn set_item(&mut self, recv: &Value, idx: &Value, val: Value) -> Result<(), String> {
        match self.get(recv) {
            Some(PyObj::List(l)) => {
                let n = l.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("list indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: list assignment index out of range".into());
                }
                if let Some(PyObj::List(l)) = self.get_mut(recv) {
                    l[k as usize] = val;
                }
                Ok(())
            }
            Some(PyObj::Dict(_)) => {
                let key = self.to_key(idx)?;
                let kv = idx.clone();
                if let Some(PyObj::Dict(d)) = self.get_mut(recv) {
                    dict_put(d, key, kv, val);
                }
                Ok(())
            }
            // `ba[i] = int` — a single byte in `0..=256`.
            Some(PyObj::Bytearray(b)) => {
                let n = b.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("bytearray indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: bytearray index out of range".into());
                }
                let v = self
                    .as_int(&val)
                    .ok_or_else(|| type_error("an integer is required"))?;
                if !(0..=255).contains(&v) {
                    return Err("ValueError: byte must be in range(0, 256)".into());
                }
                if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                    b[k as usize] = v as u8;
                }
                Ok(())
            }
            _ => Err(type_error(&format!(
                "'{}' object does not support item assignment",
                self.type_name(recv)
            ))),
        }
    }

    pub fn del_item(&mut self, recv: &Value, idx: &Value) -> Result<(), String> {
        // Slice deletion: `del x[i:j]`, `del x[::k]`.
        if let Some(PyObj::Slice { lo, hi, step }) = self.get(idx) {
            let (lo, hi, step) = (lo.clone(), hi.clone(), step.clone());
            return self.del_slice(recv, &lo, &hi, &step);
        }
        match self.get(recv) {
            Some(PyObj::Dict(_)) => {
                let key = self.to_key(idx)?;
                let removed = match self.get_mut(recv) {
                    Some(PyObj::Dict(d)) => d.shift_remove(&key).is_some(),
                    _ => false,
                };
                if !removed {
                    return Err(self.key_error(idx));
                }
                Ok(())
            }
            Some(PyObj::List(l)) => {
                let n = l.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("list indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: list assignment index out of range".into());
                }
                if let Some(PyObj::List(l)) = self.get_mut(recv) {
                    l.remove(k as usize);
                }
                Ok(())
            }
            Some(PyObj::Bytearray(b)) => {
                let n = b.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("bytearray indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: bytearray index out of range".into());
                }
                if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                    b.remove(k as usize);
                }
                Ok(())
            }
            _ => Err(type_error("object doesn't support item deletion")),
        }
    }

    /// The concrete indices selected by `[lo:hi:step]` over a length-`n`
    /// sequence, in iteration order (mirrors CPython `PySlice_AdjustIndices`).
    fn slice_indices(&mut self, lo: &Value, hi: &Value, step: i64, n: i64) -> Vec<i64> {
        let (mut i, stop) = slice_bounds(lo, hi, step, n, self);
        let mut out = Vec::new();
        if step > 0 {
            while i < stop {
                if i >= 0 && i < n {
                    out.push(i);
                }
                i += step;
            }
        } else {
            while i > stop {
                if i >= 0 && i < n {
                    out.push(i);
                }
                i += step;
            }
        }
        out
    }

    /// `x[lo:hi:step] = repl` (lists only), with `repl` already materialized. A
    /// contiguous slice (step == 1) splices in any-length replacement; an
    /// extended slice (step ≠ 1) requires `repl.len()` to equal the selected count.
    pub fn set_slice_vals(
        &mut self,
        recv: &Value,
        idx: &Value,
        repl: Vec<Value>,
    ) -> Result<(), String> {
        let (lo, hi, step) = match self.get(idx) {
            Some(PyObj::Slice { lo, hi, step }) => (lo.clone(), hi.clone(), step.clone()),
            _ => return Err(type_error("expected a slice")),
        };
        let (lo, hi) = (&lo, &hi);
        let step = self.as_int(&step).unwrap_or(1);
        if step == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        // `ba[i:j] = bytes-like` — the replacement's items are ints in `0..=256`.
        if matches!(self.get(recv), Some(PyObj::Bytearray(_))) {
            return self.set_bytearray_slice(recv, lo, hi, step, repl);
        }
        let n = match self.get(recv) {
            Some(PyObj::List(l)) => l.len() as i64,
            _ => {
                return Err(type_error(&format!(
                    "'{}' object does not support item assignment",
                    self.type_name(recv)
                )))
            }
        };
        if step == 1 {
            // Contiguous splice over [start, stop).
            let (start, stop) = slice_bounds(lo, hi, 1, n, self);
            let (start, stop) = (
                start.clamp(0, n) as usize,
                stop.clamp(0, n).max(start) as usize,
            );
            if let Some(PyObj::List(l)) = self.get_mut(recv) {
                l.splice(start..stop, repl);
            }
            Ok(())
        } else {
            let indices = self.slice_indices(lo, hi, step, n);
            if indices.len() != repl.len() {
                return Err(format!(
                    "ValueError: attempt to assign sequence of size {} to extended slice of size {}",
                    repl.len(),
                    indices.len()
                ));
            }
            if let Some(PyObj::List(l)) = self.get_mut(recv) {
                for (idx, v) in indices.into_iter().zip(repl) {
                    l[idx as usize] = v;
                }
            }
            Ok(())
        }
    }

    /// `bytearray[lo:hi:step] = repl` — `repl` is the RHS iterable already
    /// materialized to a `Vec<Value>` of ints. A contiguous slice (step == 1)
    /// splices any-length; an extended slice needs an exact-length replacement.
    fn set_bytearray_slice(
        &mut self,
        recv: &Value,
        lo: &Value,
        hi: &Value,
        step: i64,
        repl: Vec<Value>,
    ) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(repl.len());
        for v in &repl {
            let n = self
                .as_int(v)
                .ok_or_else(|| type_error("an integer is required"))?;
            if !(0..=255).contains(&n) {
                return Err("ValueError: byte must be in range(0, 256)".into());
            }
            bytes.push(n as u8);
        }
        let n = match self.get(recv) {
            Some(PyObj::Bytearray(b)) => b.len() as i64,
            _ => return Err(type_error("expected a bytearray")),
        };
        if step == 1 {
            let (start, stop) = slice_bounds(lo, hi, 1, n, self);
            let (start, stop) = (
                start.clamp(0, n) as usize,
                stop.clamp(0, n).max(start) as usize,
            );
            if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                b.splice(start..stop, bytes);
            }
            Ok(())
        } else {
            let indices = self.slice_indices(lo, hi, step, n);
            if indices.len() != bytes.len() {
                return Err(format!(
                    "ValueError: attempt to assign bytes of size {} to extended slice of size {}",
                    bytes.len(),
                    indices.len()
                ));
            }
            if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                for (idx, v) in indices.into_iter().zip(bytes) {
                    b[idx as usize] = v;
                }
            }
            Ok(())
        }
    }

    /// `del x[lo:hi:step]` (lists and bytearrays).
    fn del_slice(
        &mut self,
        recv: &Value,
        lo: &Value,
        hi: &Value,
        step: &Value,
    ) -> Result<(), String> {
        let step = self.as_int(step).unwrap_or(1);
        if step == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        let n = match self.get(recv) {
            Some(PyObj::List(l)) => l.len() as i64,
            Some(PyObj::Bytearray(b)) => b.len() as i64,
            _ => {
                return Err(type_error(&format!(
                    "'{}' object doesn't support item deletion",
                    self.type_name(recv)
                )))
            }
        };
        let mut indices = self.slice_indices(lo, hi, step, n);
        indices.sort_unstable();
        indices.dedup();
        // Remove from highest index down so earlier removals don't shift.
        match self.get_mut(recv) {
            Some(PyObj::List(l)) => {
                for i in indices.into_iter().rev() {
                    if (i as usize) < l.len() {
                        l.remove(i as usize);
                    }
                }
            }
            Some(PyObj::Bytearray(b)) => {
                for i in indices.into_iter().rev() {
                    if (i as usize) < b.len() {
                        b.remove(i as usize);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Materialize an iterable into a Vec of values (for `for`, comprehensions,
    /// `list()`, unpacking, …).
    pub fn iter_items(&mut self, v: &Value) -> Result<Vec<Value>, String> {
        // Iterating a file yields its remaining lines (each keeping its `\n`).
        // Read first (drops the immutable borrow) so `new_str` can borrow `&mut`.
        let file_id = match self.get(v) {
            Some(PyObj::File { id }) => Some(*id),
            _ => None,
        };
        if let Some(id) = file_id {
            let lines = self.io_read_lines(id)?;
            return Ok(lines.into_iter().map(|l| self.new_str(l)).collect());
        }
        // A dict view materializes its live elements (allocating item tuples).
        if let Some(items) = self.view_items(v) {
            return Ok(items);
        }
        // A CPython iterable (stdlib-ffi) is drained through its own iterator.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(v) {
            let it = crate::ffi::make_iter(self, id)?;
            let mut out = Vec::new();
            while let Some(x) = self.iter_next(&it)? {
                out.push(x);
            }
            return Ok(out);
        }
        match self.get(v) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Ok(l.clone()),
            Some(PyObj::Deque { items, .. }) => Ok(items.iter().cloned().collect()),
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => {
                Ok(b.iter().map(|&x| Value::Int(x as i64)).collect())
            }
            Some(PyObj::Memoryview { .. }) => Ok(self
                .mv_bytes(v)
                .iter()
                .map(|&x| Value::Int(x as i64))
                .collect()),
            Some(PyObj::Str(s)) => {
                let chars: Vec<Value> = s
                    .chars()
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|s| self.new_str(s))
                    .collect();
                Ok(chars)
            }
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => Ok(self.set_ordered_values(s)),
            Some(PyObj::Dict(d)) => Ok(d.values().map(|(k, _)| k.clone()).collect()),
            Some(PyObj::Range { start, stop, step }) => {
                let (start, stop, step) = (*start, *stop, *step);
                let mut out = Vec::new();
                let mut c = start;
                if step > 0 {
                    while c < stop {
                        out.push(Value::Int(c));
                        c += step;
                    }
                } else if step < 0 {
                    while c > stop {
                        out.push(Value::Int(c));
                        c += step;
                    }
                }
                Ok(out)
            }
            Some(PyObj::Iter(_)) => {
                let mut out = Vec::new();
                while let Some(x) = self.iter_next(v)? {
                    out.push(x);
                }
                Ok(out)
            }
            _ => {
                // Instance with __iter__/__next__ handled by caller; generators later.
                Err(type_error(&format!(
                    "'{}' object is not iterable",
                    self.type_name(v)
                )))
            }
        }
    }

    /// Build an iterator object over `v`.
    pub fn make_iter(&mut self, v: &Value) -> Result<Value, String> {
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(v) {
            return crate::ffi::make_iter(self, id);
        }
        // A dict view snapshots its live elements at iterator creation.
        if let Some(items) = self.view_items(v) {
            return Ok(self.alloc(PyObj::Iter(IterState::Seq { items, idx: 0 })));
        }
        let state = match self.get(v) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => IterState::Seq {
                items: l.clone(),
                idx: 0,
            },
            Some(PyObj::Str(s)) => IterState::Seq {
                items: s
                    .chars()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(PyObj::Str)
                    .map(|o| self.alloc(o))
                    .collect(),
                idx: 0,
            },
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => IterState::Seq {
                items: self.set_ordered_values(s),
                idx: 0,
            },
            Some(PyObj::Dict(d)) => IterState::DictKeys {
                keys: d.values().map(|(k, _)| k.clone()).collect(),
                idx: 0,
            },
            Some(PyObj::Range { start, stop, step }) => IterState::RangeIter {
                cur: *start,
                stop: *stop,
                step: *step,
            },
            Some(PyObj::Iter(_))
            | Some(PyObj::Generator { .. })
            | Some(PyObj::Zip { .. })
            | Some(PyObj::MapObj { .. })
            | Some(PyObj::FilterObj { .. })
            | Some(PyObj::EnumerateObj { .. })
            | Some(PyObj::CallIter { .. }) => return Ok(v.clone()),
            _ => {
                let items = self.iter_items(v)?;
                IterState::Seq { items, idx: 0 }
            }
        };
        Ok(self.alloc(PyObj::Iter(state)))
    }

    /// Advance an iterator; `None` on exhaustion.
    pub fn iter_next(&mut self, it: &Value) -> Result<Option<Value>, String> {
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(it) {
            return crate::ffi::iter_next(self, id);
        }
        let out = match self.get_mut(it) {
            Some(PyObj::Iter(IterState::Seq { items, idx })) => {
                if *idx < items.len() {
                    let v = items[*idx].clone();
                    *idx += 1;
                    Some(v)
                } else {
                    None
                }
            }
            Some(PyObj::Iter(IterState::DictKeys { keys, idx })) => {
                if *idx < keys.len() {
                    let v = keys[*idx].clone();
                    *idx += 1;
                    Some(v)
                } else {
                    None
                }
            }
            Some(PyObj::Iter(IterState::RangeIter { cur, stop, step })) => {
                let go = if *step > 0 {
                    *cur < *stop
                } else {
                    *cur > *stop
                };
                if go {
                    let v = *cur;
                    *cur += *step;
                    Some(Value::Int(v))
                } else {
                    None
                }
            }
            _ => return Err(type_error("not an iterator")),
        };
        Ok(out)
    }

    /// `item in container`.
    pub fn contains(&mut self, item: &Value, container: &Value) -> Result<bool, String> {
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(container) {
            return crate::ffi::contains(self, id, item);
        }
        // A dict view: membership over its live elements. A keys view can test
        // membership by direct key lookup (O(1)); values/items compare linearly.
        if let Some(PyObj::DictView { dict, kind }) = self.get(container) {
            let (dict, kind) = (dict.clone(), *kind);
            if kind == 0 {
                let key = self.to_key(item)?;
                return Ok(matches!(self.get(&dict), Some(PyObj::Dict(d)) if d.contains_key(&key)));
            }
            let items = self.view_items(container).unwrap_or_default();
            return Ok(items.iter().any(|x| self.equal(x, item)));
        }
        match self.get(container) {
            Some(PyObj::Str(s)) => {
                let needle = self
                    .as_str(item)
                    .ok_or_else(|| type_error("'in <string>' requires string as left operand"))?;
                Ok(s.contains(&needle))
            }
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => {
                let l = l.clone();
                Ok(l.iter().any(|x| self.equal(x, item)))
            }
            Some(PyObj::Dict(d)) => {
                let key = self.to_key(item)?;
                Ok(d.contains_key(&key))
            }
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => {
                let key = self.to_key(item)?;
                Ok(s.contains_key(&key))
            }
            // `int in bytes` tests byte-value membership; a bytes-like `in bytes`
            // is a substring search (`b'i' in b'hi'` → True).
            Some(PyObj::Bytes(hay)) | Some(PyObj::Bytearray(hay)) => {
                let hay = hay.clone();
                if let Some(n) = self.as_int(item) {
                    if !(0..=255).contains(&n) {
                        return Err("ValueError: byte must be in range(0, 256)".into());
                    }
                    return Ok(hay.contains(&(n as u8)));
                }
                let needle = match self.get(item) {
                    Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
                    _ => {
                        return Err(type_error(&format!(
                            "a bytes-like object is required, or an integer, not '{}'",
                            self.type_name(item)
                        )))
                    }
                };
                if needle.is_empty() {
                    return Ok(true);
                }
                Ok(hay.windows(needle.len()).any(|w| w == needle.as_slice()))
            }
            // `int in memoryview` tests byte-value membership over the view.
            Some(PyObj::Memoryview { .. }) => {
                let hay = self.mv_bytes(container);
                match self.as_int(item) {
                    Some(n) if (0..=255).contains(&n) => Ok(hay.contains(&(n as u8))),
                    _ => Ok(false),
                }
            }
            Some(PyObj::Range { start, stop, step }) => {
                let (start, stop, step) = (*start, *stop, *step);
                // O(1) membership: an integer in the arithmetic progression and
                // within the half-open bounds. Non-integers are never members.
                let x = match item {
                    Value::Int(n) => *n,
                    Value::Bool(b) => *b as i64,
                    // An integral float equals its integer value (`2.0 in range(5)`);
                    // a fractional float can never be a member.
                    Value::Float(f)
                        if f.fract() == 0.0
                            && f.is_finite()
                            && *f >= i64::MIN as f64
                            && *f <= i64::MAX as f64 =>
                    {
                        *f as i64
                    }
                    _ => return Ok(false),
                };
                let in_bounds = if step > 0 {
                    x >= start && x < stop
                } else {
                    x <= start && x > stop
                };
                Ok(in_bounds && (x - start).rem_euclid(step.abs()) == 0)
            }
            _ => {
                let items = self.iter_items(container)?;
                Ok(items.iter().any(|x| self.equal(x, item)))
            }
        }
    }
}

/// Resolve the (start, stop) integer bounds of a slice given optional endpoints.
/// Mirrors CPython's `PySlice_AdjustIndices`: negative endpoints are relative to
/// the end, and the clamping bounds differ by step direction (a negative step
/// clamps into `[-1, n-1]`, a positive step into `[0, n]`).
impl PyHost {
    /// `slice.indices(n)` support: the clamped `(start, stop)` for `[lo:hi:step]`
    /// over a length-`n` sequence (mirrors CPython `PySlice_AdjustIndices`). The
    /// caller supplies already-int-coerced bounds (`__index__` resolved).
    pub fn slice_adjust(&self, lo: &Value, hi: &Value, step: i64, n: i64) -> (i64, i64) {
        slice_bounds(lo, hi, step, n, self)
    }
}

fn slice_bounds(lo: &Value, hi: &Value, step: i64, n: i64, h: &PyHost) -> (i64, i64) {
    let lower = if step < 0 { -1 } else { 0 };
    let upper = if step < 0 { n - 1 } else { n };
    let adjust = |x: i64| -> i64 {
        let x = if x < 0 { x + n } else { x };
        x.clamp(lower, upper)
    };
    let start = match h.as_int(lo) {
        Some(x) => adjust(x),
        None => {
            if step < 0 {
                n - 1
            } else {
                0
            }
        }
    };
    let stop = match h.as_int(hi) {
        Some(x) => adjust(x),
        None => {
            if step < 0 {
                -1
            } else {
                n
            }
        }
    };
    (start, stop)
}

// ── attributes ───────────────────────────────────────────────────────────────

impl PyHost {
    /// The method resolution order for a class (this class first), computed by
    /// C3 linearization — the same algorithm CPython uses, so cooperative
    /// `super()` across diamond inheritance visits every base exactly once in the
    /// correct order. (`object` is implicit and omitted, since no methods live on
    /// it in the class registry.)
    /// Whether `class` (a user class name) derives from a builtin exception type
    /// — i.e. its MRO reaches a name that `is_exception_class` recognizes.
    pub fn class_is_exception(&self, class: &str) -> bool {
        self.mro_of(class)
            .iter()
            .any(|c| crate::builtins::is_exception_class(c))
    }

    pub fn mro_of(&self, class: &str) -> Vec<String> {
        let bases: Vec<String> = self
            .classes
            .get(class)
            .map(|cd| cd.bases.clone())
            .unwrap_or_default();
        if bases.is_empty() {
            return vec![class.to_string()];
        }
        let mut seqs: Vec<Vec<String>> = bases.iter().map(|b| self.mro_of(b)).collect();
        seqs.push(bases);
        let mut result = vec![class.to_string()];
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                break;
            }
            // A valid next head appears at the front of some sequence and never
            // in the tail of any sequence.
            let head = seqs.iter().find_map(|s| {
                let h = &s[0];
                let in_tail = seqs.iter().any(|t| t.len() > 1 && t[1..].contains(h));
                if in_tail {
                    None
                } else {
                    Some(h.clone())
                }
            });
            let head = match head {
                Some(h) => h,
                // Inconsistent hierarchy (CPython raises); degrade gracefully.
                None => break,
            };
            result.push(head.clone());
            for s in &mut seqs {
                if s.first() == Some(&head) {
                    s.remove(0);
                }
            }
        }
        result
    }

    /// Look up a name in a class's MRO namespace.
    pub fn class_lookup(&self, class: &str, name: &str) -> Option<Value> {
        for c in self.mro_of(class) {
            if let Some(cd) = self.classes.get(&c) {
                if let Some(v) = cd.ns.get(name) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// `recv.name`.
    pub fn get_attr(&mut self, recv: &Value, name: &str) -> Result<Value, String> {
        // A CPython object (stdlib-ffi) resolves attributes on the CPython side.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::get_attr(self, id, name);
        }
        // namedtuple field access: a tagged tuple resolves `.field` to its index,
        // and `._fields` to the field-name tuple.
        if let Value::Obj(i) = recv {
            if let Some(fields) = self.nt_meta.get(i).map(|m| m.fields.clone()) {
                if name == "_fields" {
                    let items: Vec<Value> = fields.iter().map(|f| self.new_str(f.clone())).collect();
                    return Ok(self.new_tuple(items));
                }
                if let Some(idx) = fields.iter().position(|f| f == name) {
                    if let Some(PyObj::Tuple(items)) = self.get(recv) {
                        if let Some(v) = items.get(idx) {
                            return Ok(v.clone());
                        }
                    }
                }
            }
        }
        // namedtuple TYPE object: `Point._fields`.
        if name == "_fields" {
            if let Some(PyObj::NamedTupleType { fields, .. }) = self.get(recv) {
                let fields = fields.clone();
                let items: Vec<Value> = fields.iter().map(|f| self.new_str(f.clone())).collect();
                return Ok(self.new_tuple(items));
            }
        }
        // Native-shadowed module: fast-path the native namespace, else defer to
        // the real CPython module over the FFI bridge. Resolved before the
        // borrowing match below because the fallback needs `&mut self`.
        let module_lookup = match self.get(recv) {
            Some(PyObj::Module { ns, name: mname }) => Some((ns.get(name).cloned(), mname.clone())),
            _ => None,
        };
        if let Some((hit, mname)) = module_lookup {
            return match hit {
                Some(v) => Ok(v),
                None => match module_ffi_fallback(self, &mname, name) {
                    Some(r) => r,
                    None => Err(format!(
                        "AttributeError: module '{mname}' has no attribute '{name}'"
                    )),
                },
            };
        }
        match self.get(recv) {
            Some(PyObj::Instance(inst)) => {
                let class = inst.class.clone();
                let inst_dict = inst.dict.clone();
                if let Some(v) = self.inst_attr(&inst_dict, name) {
                    return Ok(v);
                }
                // Exception chaining links live in a side table, not the
                // instance dict (a user exception is a plain `Instance`). Only
                // exception instances expose these dunders.
                // A user exception instance always exposes `.args` (empty tuple
                // if no construction path stored one).
                if name == "args" && self.class_is_exception(&class) {
                    return Ok(self.alloc(PyObj::Tuple(vec![])));
                }
                if (name == "__cause__" || name == "__context__" || name == "__suppress_context__")
                    && self.class_is_exception(&class)
                {
                    return Ok(match name {
                        "__cause__" => self.exc_link(recv).0,
                        "__context__" => self.exc_link(recv).1,
                        _ => Value::Bool(!matches!(self.exc_link(recv).0, Value::Undef)),
                    });
                }
                // Instance introspection: `__class__` and `__dict__`.
                if name == "__class__" {
                    return Ok(self.alloc(PyObj::Class(class)));
                }
                if name == "__dict__" {
                    // A fully-slotted instance has no `__dict__`.
                    if self.slots_of(&class).is_some() {
                        return Err(format!(
                            "AttributeError: '{class}' object has no attribute '__dict__'"
                        ));
                    }
                    // Hand back the instance's live dict by handle: identity is
                    // stable and mutations through it write through to attrs.
                    return Ok(inst_dict);
                }
                if let Some(v) = self.class_lookup(&class, name) {
                    match self.get(&v) {
                        // Bind plain functions to the instance.
                        Some(PyObj::Func(_)) => {
                            return Ok(self.alloc(PyObj::BoundMethod {
                                recv: recv.clone(),
                                func: v,
                            }));
                        }
                        // staticmethod: hand back the raw function.
                        Some(PyObj::StaticMethod(inner)) => return Ok(inner.clone()),
                        // classmethod: bind the class as `cls`.
                        Some(PyObj::ClassMethod(inner)) => {
                            let inner = inner.clone();
                            let cls = self.alloc(PyObj::Class(class.clone()));
                            return Ok(self.alloc(PyObj::BoundMethod {
                                recv: cls,
                                func: inner,
                            }));
                        }
                        _ => return Ok(v),
                    }
                }
                Err(format!(
                    "AttributeError: '{}' object has no attribute '{}'",
                    class, name
                ))
            }
            Some(PyObj::Class(cname)) => {
                let cname = cname.clone();
                if name == "__name__" {
                    return Ok(self.new_str(cname));
                }
                if name == "__qualname__" {
                    // The dotted path for a nested class (`A.B`); a top-level
                    // class has none recorded, so its qualname is its name.
                    let q = self
                        .classes
                        .get(&cname)
                        .map(|c| c.qualname.clone())
                        .filter(|q| !q.is_empty())
                        .unwrap_or_else(|| cname.clone());
                    return Ok(self.new_str(q));
                }
                // `cls.__class__` is the metaclass (`type(cls)`): a user metaclass
                // becomes a `Class`, otherwise the builtin `type`.
                if name == "__class__" {
                    let meta = self
                        .classes
                        .get(&cname)
                        .map(|c| c.metaclass.clone())
                        .unwrap_or_else(|| "type".into());
                    return Ok(if self.classes.contains_key(&meta) {
                        self.alloc(PyObj::Class(meta))
                    } else {
                        self.alloc(PyObj::Builtin(meta))
                    });
                }
                // Class introspection: `__mro__`, `__bases__`, `__dict__`.
                if name == "__mro__" {
                    let mut mro: Vec<Value> = self
                        .mro_of(&cname)
                        .into_iter()
                        .map(|c| self.alloc(PyObj::Class(c)))
                        .collect();
                    // `object` is the implicit tail of every MRO.
                    mro.push(self.alloc(PyObj::Builtin("object".into())));
                    return Ok(self.new_tuple(mro));
                }
                if name == "__bases__" {
                    let bases: Vec<String> = self
                        .classes
                        .get(&cname)
                        .map(|cd| cd.bases.clone())
                        .unwrap_or_default();
                    let vals: Vec<Value> = if bases.is_empty() {
                        vec![self.alloc(PyObj::Builtin("object".into()))]
                    } else {
                        bases
                            .into_iter()
                            .map(|b| self.alloc(PyObj::Class(b)))
                            .collect()
                    };
                    return Ok(self.new_tuple(vals));
                }
                if name == "__dict__" {
                    let ns = self
                        .classes
                        .get(&cname)
                        .map(|cd| cd.ns.clone())
                        .unwrap_or_default();
                    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                    for (k, val) in ns {
                        let kv = self.new_str(k.clone());
                        d.insert(PKey::Str(k), (kv, val));
                    }
                    return Ok(self.new_dict(d));
                }
                if let Some(v) = self.class_lookup(&cname, name) {
                    match self.get(&v) {
                        Some(PyObj::StaticMethod(inner)) => return Ok(inner.clone()),
                        Some(PyObj::ClassMethod(inner)) => {
                            let inner = inner.clone();
                            let cls = self.alloc(PyObj::Class(cname.clone()));
                            return Ok(self.alloc(PyObj::BoundMethod {
                                recv: cls,
                                func: inner,
                            }));
                        }
                        _ => return Ok(v),
                    }
                }
                // Fall back to the metaclass (`type(cls)`): an attribute defined on
                // the metaclass is visible through the class (`cls._registry`).
                let meta = self
                    .classes
                    .get(&cname)
                    .map(|c| c.metaclass.clone())
                    .unwrap_or_else(|| "type".into());
                if meta != "type" {
                    if let Some(v) = self.class_lookup(&meta, name) {
                        // A metaclass *method* binds the class as its receiver.
                        if matches!(self.get(&v), Some(PyObj::Func(_))) {
                            let cls = self.alloc(PyObj::Class(cname.clone()));
                            return Ok(self.alloc(PyObj::BoundMethod { recv: cls, func: v }));
                        }
                        return Ok(v);
                    }
                }
                Err(format!(
                    "AttributeError: type object '{cname}' has no attribute '{name}'"
                ))
            }
            // Modules are fully resolved up-front (see the block before this
            // match) so the FFI fallback can take `&mut self`; unreachable here.
            Some(PyObj::Module { ns, name: mname }) => {
                let mname = mname.clone();
                match ns.get(name) {
                    Some(v) => Ok(v.clone()),
                    None => Err(format!(
                        "AttributeError: module '{mname}' has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Exception { class, args }) => {
                if name == "args" {
                    let a = args.clone();
                    return Ok(self.new_tuple(a));
                }
                // `StopIteration.value` / `StopAsyncIteration.value` — the first
                // arg (the generator's `return` value), or `None`.
                if name == "value" && (class == "StopIteration" || class == "StopAsyncIteration") {
                    return Ok(args.first().cloned().unwrap_or(Value::Undef));
                }
                // `SystemExit.code` — the first arg, the whole tuple for 2+ args,
                // or `None` when constructed with no arguments.
                if name == "code" && class == "SystemExit" {
                    return Ok(match args.len() {
                        0 => Value::Undef,
                        1 => args[0].clone(),
                        _ => {
                            let a = args.clone();
                            self.new_tuple(a)
                        }
                    });
                }
                if name == "__cause__" {
                    return Ok(self.exc_link(recv).0);
                }
                if name == "__context__" {
                    return Ok(self.exc_link(recv).1);
                }
                if name == "__suppress_context__" {
                    // True iff an explicit cause was set (`raise X from Y`).
                    let has_cause = !matches!(self.exc_link(recv).0, Value::Undef);
                    return Ok(Value::Bool(has_cause));
                }
                let class = class.clone();
                Err(format!(
                    "AttributeError: '{class}' object has no attribute '{name}'"
                ))
            }
            Some(PyObj::Super { owner, instance }) => {
                let owner = owner.clone();
                let instance = instance.clone();
                let inst_class = match self.get(&instance) {
                    Some(PyObj::Instance(i)) => i.class.clone(),
                    _ => owner.clone(),
                };
                match super_lookup(self, &owner, &inst_class, name) {
                    Some((v, _)) => {
                        // Bind a found method to the original instance.
                        if matches!(self.get(&v), Some(PyObj::Func(_))) {
                            Ok(self.alloc(PyObj::BoundMethod {
                                recv: instance,
                                func: v,
                            }))
                        } else {
                            Ok(v)
                        }
                    }
                    None => Err(format!(
                        "AttributeError: 'super' object has no attribute '{name}'"
                    )),
                }
            }
            // Function introspection dunders. `C.m` yields the raw `Func`; a
            // bound `inst.m` delegates to the same underlying function.
            Some(PyObj::Func(fv))
                if matches!(
                    name,
                    "__name__" | "__qualname__" | "__module__" | "__defaults__"
                ) =>
            {
                let (def_id, defaults) = (fv.def_id, fv.defaults.clone());
                self.func_dunder(name, def_id, &defaults)
            }
            Some(PyObj::BoundMethod { func, recv })
                if matches!(
                    name,
                    "__name__" | "__qualname__" | "__module__" | "__defaults__"
                ) =>
            {
                let func = func.clone();
                let recv = recv.clone();
                match self.get(&func) {
                    Some(PyObj::Func(fv)) => {
                        let (def_id, defaults) = (fv.def_id, fv.defaults.clone());
                        self.func_dunder(name, def_id, &defaults)
                    }
                    // A bound builtin method (`[].append`): `func` is the method
                    // name. `__name__` is the bare name, `__qualname__` is
                    // `<type>.<name>`; `__module__`/`__defaults__` are `None`.
                    Some(PyObj::Builtin(bn)) => {
                        let bare = bn.rsplit('.').next().unwrap_or(bn).to_string();
                        match name {
                            "__name__" => Ok(self.new_str(bare)),
                            "__qualname__" => {
                                let tn = self.type_name(&recv);
                                Ok(self.new_str(format!("{tn}.{bare}")))
                            }
                            _ => Ok(Value::Undef),
                        }
                    }
                    _ => Err(format!(
                        "AttributeError: 'method' object has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Builtin(n)) if name == "__name__" => {
                // `type(x).__name__` — the builtin/type object's name.
                let n = n.clone();
                Ok(self.new_str(n))
            }
            // `dict.fromkeys` — a classmethod on the `dict` type, reached as an
            // attribute of the `dict` builtin. Returns a callable builtin.
            Some(PyObj::Builtin(n)) if n == "dict" && name == "fromkeys" => {
                Ok(self.alloc(PyObj::Builtin("dict.fromkeys".into())))
            }
            // `str.maketrans` — a static method on the `str` type.
            Some(PyObj::Builtin(n)) if n == "str" && name == "maketrans" => {
                Ok(self.alloc(PyObj::Builtin("str.maketrans".into())))
            }
            // `bytes.fromhex` / `bytearray.fromhex` — classmethods on the type.
            Some(PyObj::Builtin(n)) if (n == "bytes" || n == "bytearray") && name == "fromhex" => {
                Ok(self.alloc(PyObj::Builtin(format!("{n}.fromhex"))))
            }
            // `int.from_bytes` — a classmethod on the `int` type object.
            Some(PyObj::Builtin(n)) if n == "int" && name == "from_bytes" => {
                Ok(self.alloc(PyObj::Builtin("int.from_bytes".into())))
            }
            // `float.fromhex` — a classmethod on the `float` type object.
            Some(PyObj::Builtin(n)) if n == "float" && name == "fromhex" => {
                Ok(self.alloc(PyObj::Builtin("float.fromhex".into())))
            }
            // `bytes.maketrans` / `bytearray.maketrans` — static methods on the type.
            Some(PyObj::Builtin(n))
                if (n == "bytes" || n == "bytearray") && name == "maketrans" =>
            {
                Ok(self.alloc(PyObj::Builtin(format!("{n}.maketrans"))))
            }
            // Unbound instance method reached via a builtin type object
            // (`str.lower`, `list.append`, `dict.get`): a callable that takes the
            // receiver as its first argument (CPython's unbound method). Gated by
            // `type_has_method`, so a non-method name falls through to
            // AttributeError below.
            Some(PyObj::Builtin(n)) if crate::builtins::type_has_method(n, name) => {
                Ok(self.alloc(PyObj::Builtin(format!("{n}.{name}"))))
            }
            // `memoryview` read-only descriptor attributes. A faithful 1-D
            // unsigned-byte view: `format 'B'`, `itemsize 1`, `ndim 1`,
            // contiguous. `obj` is the backing object; `nbytes`/`shape` derive
            // from the window length.
            Some(PyObj::Memoryview {
                obj, len, readonly, ..
            }) if matches!(
                name,
                "obj"
                    | "nbytes"
                    | "format"
                    | "itemsize"
                    | "ndim"
                    | "readonly"
                    | "shape"
                    | "strides"
                    | "contiguous"
                    | "c_contiguous"
                    | "f_contiguous"
            ) =>
            {
                let (obj, len, readonly) = (obj.clone(), *len, *readonly);
                Ok(match name {
                    "obj" => obj,
                    "nbytes" => Value::Int(len as i64),
                    "format" => self.new_str("B"),
                    "itemsize" => Value::Int(1),
                    "ndim" => Value::Int(1),
                    "readonly" => Value::Bool(readonly),
                    "shape" => {
                        let n = Value::Int(len as i64);
                        self.new_tuple(vec![n])
                    }
                    "strides" => {
                        let one = Value::Int(1);
                        self.new_tuple(vec![one])
                    }
                    // Single-segment 1-D views are contiguous in every layout.
                    _ => Value::Bool(true),
                })
            }
            // `slice` read-only attributes: the RAW stored bound objects
            // (`slice(x).start is x`), `None` for an omitted bound.
            Some(PyObj::Slice { lo, hi, step }) if matches!(name, "start" | "stop" | "step") => {
                let v = match name {
                    "start" => lo,
                    "stop" => hi,
                    _ => step,
                };
                Ok(v.clone())
            }
            _ => {
                // Numeric `.real`/`.imag` (int/float/bool/bigint/complex are all
                // read-only descriptors in CPython).
                if name == "real" || name == "imag" {
                    if let Some(PyObj::Complex(r, i)) = self.get(recv) {
                        let (r, i) = (*r, *i);
                        return Ok(Value::Float(if name == "real" { r } else { i }));
                    }
                    if let Value::Int(_) | Value::Bool(_) = recv {
                        return Ok(if name == "real" {
                            recv.clone()
                        } else {
                            Value::Int(0)
                        });
                    }
                    if let Value::Float(f) = recv {
                        return Ok(Value::Float(if name == "real" { *f } else { 0.0 }));
                    }
                    if matches!(self.get(recv), Some(PyObj::BigInt(_))) {
                        return Ok(if name == "real" {
                            recv.clone()
                        } else {
                            Value::Int(0)
                        });
                    }
                }
                // Builtin type method: hand back a bound builtin method.
                let tn = self.type_name(recv);
                if crate::builtins::type_has_method(&tn, name) {
                    let b = self.alloc(PyObj::Builtin(name.to_string()));
                    return Ok(self.alloc(PyObj::BoundMethod {
                        recv: recv.clone(),
                        func: b,
                    }));
                }
                Err(format!(
                    "AttributeError: '{tn}' object has no attribute '{name}'"
                ))
            }
        }
    }

    /// Does `class` (via its MRO) define method `name`?
    pub fn class_has(&self, class: &str, name: &str) -> bool {
        self.class_lookup(class, name).is_some()
    }

    /// The sorted, de-duplicated attribute names `dir(v)` reports: for an
    /// instance, its live `__dict__` keys plus every name defined across its
    /// class MRO namespaces (`__slots__` members included); for a class, the
    /// names across its own MRO namespaces. Object-provided default dunders that
    /// pythonrs does not model are not enumerated.
    pub fn dir_names(&self, v: &Value) -> Vec<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        match self.get(v) {
            Some(PyObj::Instance(i)) => {
                let dict = i.dict.clone();
                let class = i.class.clone();
                for n in self.inst_attr_names(&dict) {
                    set.insert(n);
                }
                self.collect_class_dir(&class, &mut set);
            }
            Some(PyObj::Class(c)) => {
                let c = c.clone();
                self.collect_class_dir(&c, &mut set);
            }
            _ => {}
        }
        set.into_iter().collect()
    }

    /// Add every name defined across `class`'s MRO namespaces (and any
    /// `__slots__` members) to `set`.
    fn collect_class_dir(&self, class: &str, set: &mut BTreeSet<String>) {
        for c in self.mro_of(class) {
            if let Some(cd) = self.classes.get(&c) {
                for k in cd.ns.keys() {
                    set.insert(k.clone());
                }
            }
        }
        if let Some(slots) = self.slots_of(class) {
            for s in slots {
                set.insert(s);
            }
        }
    }

    /// The allowed attribute names for a `__slots__`-restricted instance, or
    /// `None` if the instance has a normal `__dict__` (some user class in its MRO
    /// omits `__slots__`). The returned set is the union of every class's slots.
    fn slots_of(&self, class: &str) -> Option<HashSet<String>> {
        let mut slots = HashSet::new();
        let mut any = false;
        for c in self.mro_of(class) {
            let cd = match self.classes.get(&c) {
                Some(cd) => cd,
                None => continue, // builtin base (e.g. `object`) — implicit, skip
            };
            // A user class without `__slots__` gives the instance a `__dict__`.
            let v = cd.ns.get("__slots__")?;
            any = true;
            match self.get(v) {
                Some(PyObj::List(items)) | Some(PyObj::Tuple(items)) => {
                    for it in items {
                        if let Some(s) = self.as_str(it) {
                            slots.insert(s);
                        }
                    }
                }
                Some(PyObj::Str(s)) => {
                    slots.insert(s.clone());
                }
                _ => {}
            }
        }
        if any {
            Some(slots)
        } else {
            None
        }
    }

    /// Plan reading `recv.name`, honoring the descriptor protocol (`property`
    /// and user `__get__` descriptors). See [`AttrGet`].
    pub fn plan_attr_get(&mut self, recv: &Value, name: &str) -> AttrGet {
        // `super().<name>` resolves along the MRO strictly after `owner`. If it
        // lands on a `property`, route through the out-of-borrow getter path so
        // `super().some_property` invokes its fget (methods/plain attrs fall
        // back to the in-borrow `get_attr` handling below via `Plain`).
        if let Some(PyObj::Super { owner, instance }) = self.get(recv) {
            let owner = owner.clone();
            let instance = instance.clone();
            let inst_class = match self.get(&instance) {
                Some(PyObj::Instance(i)) => i.class.clone(),
                _ => owner.clone(),
            };
            if let Some((v, found)) = super_lookup(self, &owner, &inst_class, name) {
                if let Some(PyObj::Property { fget, .. }) = self.get(&v) {
                    return AttrGet::Property {
                        fget: fget.clone(),
                        inst: instance,
                        owner: Some(found),
                    };
                }
            }
            return AttrGet::Plain;
        }
        // Class-level access `C.x`: a descriptor in the class MRO is invoked as
        // `desc.__get__(None, C)` (obj is `None`). `property`/method/staticmethod
        // fall through to the plain class-attribute read.
        if let Some(PyObj::Class(cname)) = self.get(recv) {
            let cname = cname.clone();
            if let Some(cls_attr) = self.class_lookup(&cname, name) {
                if let Some(PyObj::Instance(i)) = self.get(&cls_attr) {
                    let c = i.class.clone();
                    if self.class_has(&c, "__get__") {
                        return AttrGet::Descriptor {
                            desc: cls_attr,
                            inst: Value::Undef,
                            cls: recv.clone(),
                        };
                    }
                }
            }
            return AttrGet::Plain;
        }
        let (class, inst_dict) = match self.get(recv) {
            Some(PyObj::Instance(i)) => (i.class.clone(), i.dict.clone()),
            _ => return AttrGet::Plain,
        };
        let in_instdict = self.inst_has(&inst_dict, name);
        let cls_attr = match self.class_lookup(&class, name) {
            Some(v) => v,
            None => return AttrGet::Plain,
        };
        // `property` — a data descriptor: overrides the instance dict.
        if let Some(PyObj::Property { fget, .. }) = self.get(&cls_attr) {
            return AttrGet::Property {
                fget: fget.clone(),
                inst: recv.clone(),
                owner: method_owner(self, &class, name),
            };
        }
        // A user descriptor is an instance whose class defines `__get__`.
        let (has_get, is_data) = match self.get(&cls_attr) {
            Some(PyObj::Instance(i)) => {
                let c = i.class.clone();
                (
                    self.class_has(&c, "__get__"),
                    self.class_has(&c, "__set__") || self.class_has(&c, "__delete__"),
                )
            }
            _ => (false, false),
        };
        // Data descriptors override the instance dict; non-data descriptors only
        // fire when the name is absent from it.
        if has_get && (is_data || !in_instdict) {
            let cls = self.alloc(PyObj::Class(class));
            return AttrGet::Descriptor {
                desc: cls_attr,
                inst: recv.clone(),
                cls,
            };
        }
        AttrGet::Plain
    }

    /// Plan `recv.name = val`, honoring `property.fset` and user `__set__`
    /// data descriptors. See [`AttrSet`].
    pub fn plan_attr_set(&mut self, recv: &Value, name: &str, val: &Value) -> AttrSet {
        let class = match self.get(recv) {
            Some(PyObj::Instance(i)) => i.class.clone(),
            _ => return AttrSet::Plain,
        };
        let cls_attr = match self.class_lookup(&class, name) {
            Some(v) => v,
            None => return AttrSet::Plain,
        };
        if let Some(PyObj::Property { fset, .. }) = self.get(&cls_attr) {
            return AttrSet::Property {
                fset: fset.clone(),
                inst: recv.clone(),
                val: val.clone(),
                owner: method_owner(self, &class, name),
            };
        }
        let has_set = match self.get(&cls_attr) {
            Some(PyObj::Instance(i)) => {
                let c = i.class.clone();
                self.class_has(&c, "__set__")
            }
            _ => false,
        };
        if has_set {
            return AttrSet::Descriptor {
                desc: cls_attr,
                inst: recv.clone(),
                val: val.clone(),
            };
        }
        AttrSet::Plain
    }

    /// Plan `del recv.name`, honoring `property.fdel` and user data descriptors
    /// (`__delete__`). See [`AttrDel`]. Non-data descriptors (only `__get__`) do
    /// not intercept deletion — the name is removed from the instance dict.
    pub fn plan_attr_del(&mut self, recv: &Value, name: &str) -> AttrDel {
        let class = match self.get(recv) {
            Some(PyObj::Instance(i)) => i.class.clone(),
            _ => return AttrDel::Plain,
        };
        let cls_attr = match self.class_lookup(&class, name) {
            Some(v) => v,
            None => return AttrDel::Plain,
        };
        if let Some(PyObj::Property { fdel, .. }) = self.get(&cls_attr) {
            return AttrDel::Property {
                fdel: fdel.clone(),
                inst: recv.clone(),
                owner: method_owner(self, &class, name),
            };
        }
        // A data descriptor (defines `__set__` or `__delete__`) intercepts `del`.
        if let Some(PyObj::Instance(i)) = self.get(&cls_attr) {
            let c = i.class.clone();
            let has_delete = self.class_has(&c, "__delete__");
            if has_delete || self.class_has(&c, "__set__") {
                return AttrDel::Descriptor {
                    desc: cls_attr,
                    inst: recv.clone(),
                    has_delete,
                };
            }
        }
        AttrDel::Plain
    }

    /// `recv.name = val`.
    pub fn set_attr(&mut self, recv: &Value, name: &str, val: Value) -> Result<(), String> {
        // `__slots__` enforcement: a slotted instance rejects any attribute name
        // not declared in its slots.
        if let Some(PyObj::Instance(inst)) = self.get(recv) {
            let class = inst.class.clone();
            if let Some(slots) = self.slots_of(&class) {
                if !slots.contains(name) {
                    return Err(format!(
                        "AttributeError: '{class}' object has no attribute '{name}' and no \
                         __dict__ for setting new attributes"
                    ));
                }
            }
        }
        if let Some(PyObj::Instance(inst)) = self.get(recv) {
            let dict = inst.dict.clone();
            self.inst_attr_set(&dict, name, val);
            return Ok(());
        }
        match self.get_mut(recv) {
            Some(PyObj::Module { ns, .. }) => {
                ns.insert(name.to_string(), val);
                Ok(())
            }
            Some(PyObj::Class(cname)) => {
                let cname = cname.clone();
                if let Some(cd) = self.classes.get_mut(&cname) {
                    cd.ns.insert(name.to_string(), val);
                }
                Ok(())
            }
            _ => Err(type_error(&format!(
                "'{}' object attribute assignment unsupported",
                self.type_name(recv)
            ))),
        }
    }

    pub fn del_attr(&mut self, recv: &Value, name: &str) -> Result<(), String> {
        if let Some(PyObj::Instance(inst)) = self.get(recv) {
            let dict = inst.dict.clone();
            if self.inst_attr_del(&dict, name) {
                return Ok(());
            }
        }
        Err(format!(
            "AttributeError: '{}' object has no attribute '{name}'",
            self.type_name(recv)
        ))
    }

    /// Register a class built from a run class-body namespace.
    pub fn register_class(
        &mut self,
        name: &str,
        bases: Vec<String>,
        ns: IndexMap<String, Value>,
    ) -> Value {
        self.register_class_meta(name, bases, ns, "type")
    }

    /// Register a class whose metaclass (`type(cls)`) is `metaclass` — `"type"`
    /// for an ordinary class, a user metaclass name for `class A(metaclass=M)`.
    pub fn register_class_meta(
        &mut self,
        name: &str,
        bases: Vec<String>,
        ns: IndexMap<String, Value>,
        metaclass: &str,
    ) -> Value {
        let mro = {
            let mut out = vec![name.to_string()];
            for b in &bases {
                for m in self.mro_of(b) {
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
            }
            out
        };
        self.classes.insert(
            name.to_string(),
            ClassDef {
                name: name.to_string(),
                // Set by `build_class` once known; a bare registration (or an
                // older cache) leaves it empty, falling back to `name`.
                qualname: String::new(),
                bases,
                ns,
                mro,
                metaclass: metaclass.to_string(),
            },
        );
        self.alloc(PyObj::Class(name.to_string()))
    }
}

// ── call machinery (free functions: run user chunks, so hold no host borrow) ──

/// Invoke any callable value with positional + keyword arguments.
pub fn invoke(
    callable: &Value,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let obj = with_host(|h| h.get(callable).cloned());
    match obj {
        Some(PyObj::Builtin(name)) => crate::builtins::call_builtin_function(&name, args, kwargs),
        Some(PyObj::Func(fv)) => run_user_func(&fv, None, None, args, kwargs),
        // A staticmethod is just its wrapped function; a classmethod reached here
        // (without a bound class) still runs its wrapped function.
        Some(PyObj::StaticMethod(inner)) | Some(PyObj::ClassMethod(inner)) => {
            invoke(&inner, args, kwargs)
        }
        Some(PyObj::BoundMethod { recv, func }) => {
            let f = with_host(|h| h.get(&func).cloned());
            match f {
                Some(PyObj::Builtin(name)) => {
                    crate::builtins::call_type_method(&recv, &name, args, kwargs)
                }
                Some(PyObj::Func(fv)) => run_user_func(&fv, Some(recv), None, args, kwargs),
                _ => Err(type_error("bound method is not callable")),
            }
        }
        Some(PyObj::Class(name)) => instantiate(&name, args, kwargs),
        Some(PyObj::NamedTupleType { type_name, fields }) => {
            namedtuple_construct(&type_name, &fields, args, kwargs)
        }
        Some(PyObj::Partial {
            func,
            args: bound,
            kwargs: bkw,
        }) => {
            // Prepend bound positionals; bound kwargs first, call kwargs override.
            let mut all_args = bound;
            all_args.extend(args);
            let mut all_kw = bkw;
            for (k, v) in kwargs {
                if let Some(slot) = all_kw.iter_mut().find(|(kk, _)| *kk == k) {
                    slot.1 = v;
                } else {
                    all_kw.push((k, v));
                }
            }
            invoke(&func, all_args, all_kw)
        }
        Some(PyObj::LruCache { func, cache_id }) => lru_invoke(&func, cache_id, args, kwargs),
        // A user instance whose class defines `__call__` is callable.
        Some(PyObj::Instance(inst))
            if with_host(|h| h.class_lookup(&inst.class, "__call__").is_some()) =>
        {
            call_method(callable, "__call__", args, kwargs)
        }
        // A CPython callable (stdlib-ffi): call it on the CPython side. The bridge
        // drops the host borrow across the call so pythonrs callbacks can run.
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::call(id, args, kwargs),
        _ => Err(type_error(&format!(
            "'{}' object is not callable",
            with_host(|h| h.type_name(callable))
        ))),
    }
}

/// Marshal a Python call argument into a native fusevm `Value` for `rust { }`
/// FFI. Python strings ride as `Value::Obj(PyObj::Str)` heap handles, which
/// fusevm's marshaller cannot read (it calls `Value::to_str`, which returns
/// `"(obj:N)"` for a handle); rewrite them to a native `Value::Str`. Ints and
/// floats are already native `Value::Int`/`Value::Float`, so they pass through.
fn marshal_ffi_arg(v: &Value) -> Value {
    match v {
        Value::Obj(_) => match with_host(|h| h.as_str(v)) {
            Some(s) => Value::str(s),
            None => v.clone(),
        },
        _ => v.clone(),
    }
}

/// Resolve a bare name and call it (`f(args)`, `print(args)`).
pub fn call_named(
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // Inline Rust FFI: the `rust { ... }` desugar emits `__rust_compile(b64,
    // line)`; compile + register the block's exported functions, returning
    // Python `None` (`Value::Undef`).
    if name == "__rust_compile" {
        let b64 = args
            .first()
            .map(|v| with_host(|h| h.str_of(v)))
            .unwrap_or_default();
        return fusevm::ffi::compile_and_register(&b64).map(|_| Value::Undef);
    }
    if let Some(v) = with_host(|h| h.read_name(name)) {
        return invoke(&v, args, kwargs);
    }
    if with_host(|h| h.classes.contains_key(name)) {
        return instantiate(name, args, kwargs);
    }
    if crate::builtins::is_known_builtin(name) {
        return crate::builtins::call_builtin_function(name, args, kwargs);
    }
    // A `rust { ... }` block's exported functions are callable by bareword.
    // Reached only after user names/classes/builtins all miss, so Python code
    // always wins; the registry membership check keeps this off the hot path.
    if fusevm::ffi::is_registered(name) {
        let margs: Vec<Value> = args.iter().map(marshal_ffi_arg).collect();
        if let Some(r) = fusevm::ffi::try_call(name, &margs) {
            return r;
        }
    }
    Err(name_error(name))
}

// ── builtin-type subclassing (hybrid instances) ─────────────────────────────

/// Whether a builtin base type `base` provides `dunder` natively, so a subclass
/// instance responds to it without a user override. Covers the container /
/// value protocol guards (`len`, `[]`, iteration, `repr`, `hash`, numeric
/// coercion); arithmetic/comparison operators are handled by operand unwrapping
/// (see [`subclass_operand`]), not here.
pub fn base_provides(base: &str, dunder: &str) -> bool {
    match base {
        "list" => matches!(
            dunder,
            "__len__"
                | "__getitem__"
                | "__setitem__"
                | "__delitem__"
                | "__iter__"
                | "__contains__"
                | "__reversed__"
                | "__repr__"
                | "__str__"
        ),
        "tuple" => matches!(
            dunder,
            "__len__"
                | "__getitem__"
                | "__iter__"
                | "__contains__"
                | "__reversed__"
                | "__repr__"
                | "__str__"
                | "__hash__"
        ),
        "str" => matches!(
            dunder,
            "__len__"
                | "__getitem__"
                | "__iter__"
                | "__contains__"
                | "__reversed__"
                | "__repr__"
                | "__str__"
                | "__hash__"
        ),
        "dict" => matches!(
            dunder,
            "__len__"
                | "__getitem__"
                | "__setitem__"
                | "__delitem__"
                | "__iter__"
                | "__contains__"
                | "__repr__"
                | "__str__"
        ),
        "set" => matches!(
            dunder,
            "__len__" | "__iter__" | "__contains__" | "__repr__" | "__str__"
        ),
        "frozenset" => matches!(
            dunder,
            "__len__" | "__iter__" | "__contains__" | "__repr__" | "__str__" | "__hash__"
        ),
        "int" => matches!(
            dunder,
            "__repr__"
                | "__str__"
                | "__hash__"
                | "__bool__"
                | "__index__"
                | "__int__"
                | "__float__"
        ),
        "float" => matches!(
            dunder,
            "__repr__" | "__str__" | "__hash__" | "__bool__" | "__int__" | "__float__"
        ),
        _ => false,
    }
}

/// If `v` is a builtin-subclass instance whose user class does NOT override
/// `dunder`, and whose base provides `dunder`, return its native payload so the
/// caller runs the base operation on it. Otherwise `None`.
pub fn subclass_payload(v: &Value, dunder: &str) -> Option<Value> {
    with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
            let base = h.builtin_base_of(&i.class)?;
            if base_provides(base, dunder) && h.class_lookup(&i.class, dunder).is_none() {
                Some(i.payload.clone())
            } else {
                None
            }
        }
        _ => None,
    })
}

/// For an operand in an arithmetic/comparison operation: if `v` is a
/// builtin-subclass instance that does not override the operator `dunder`,
/// return its native payload (so the native operation runs and yields the base
/// type — `C(5) + 3 == 8`, a plain `int`). Otherwise return `v` unchanged.
pub fn subclass_operand(v: &Value, dunder: &str) -> Value {
    with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
            if h.builtin_base_of(&i.class).is_some() && h.class_lookup(&i.class, dunder).is_none() {
                i.payload.clone()
            } else {
                v.clone()
            }
        }
        _ => v.clone(),
    })
}

/// Run method/dunder `name` on a builtin-subclass instance by delegating to its
/// native payload. Container/value dunders route to the native heap ops; named
/// methods (`append`, `upper`, `keys`, …) route to [`call_type_method`]. `recv`
/// is the full instance (needed for a `dict` subclass `__missing__` hook).
fn base_dispatch(
    recv: &Value,
    payload: &Value,
    base: &str,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    match name {
        "__len__" => {
            let n = crate::builtins::py_len(payload)?;
            Ok(Value::Int(n as i64))
        }
        "__bool__" => Ok(Value::Bool(with_host(|h| h.truthy(payload)))),
        "__getitem__" => {
            let idx = args.into_iter().next().unwrap_or(Value::Undef);
            // A `dict` subclass with a `__missing__` hook: fire it on a miss.
            if base == "dict" {
                let missing = with_host(|h| match h.to_key(&idx) {
                    Ok(k) => matches!(h.get(payload), Some(PyObj::Dict(d)) if !d.contains_key(&k)),
                    Err(_) => false,
                });
                if missing {
                    let cls = with_host(|h| match h.get(recv) {
                        Some(PyObj::Instance(i)) => i.class.clone(),
                        _ => String::new(),
                    });
                    if with_host(|h| h.class_lookup(&cls, "__missing__").is_some()) {
                        return call_method(recv, "__missing__", vec![idx], vec![]);
                    }
                }
            }
            with_host(|h| h.get_item(payload, &idx))
        }
        "__setitem__" => {
            let mut it = args.into_iter();
            let idx = it.next().unwrap_or(Value::Undef);
            let val = it.next().unwrap_or(Value::Undef);
            with_host(|h| h.set_item(payload, &idx, val)).map(|_| Value::Undef)
        }
        "__delitem__" => {
            let idx = args.into_iter().next().unwrap_or(Value::Undef);
            with_host(|h| h.del_item(payload, &idx)).map(|_| Value::Undef)
        }
        "__contains__" => {
            let item = args.into_iter().next().unwrap_or(Value::Undef);
            Ok(Value::Bool(with_host(|h| h.contains(&item, payload))?))
        }
        "__iter__" => with_host(|h| h.make_iter(payload)),
        "__repr__" => Ok(with_host(|h| {
            let s = h.repr_of(payload);
            h.new_str(s)
        })),
        "__str__" => Ok(with_host(|h| {
            let s = h.str_of(payload);
            h.new_str(s)
        })),
        "__hash__" => {
            // Hash by the payload's value (the base type's `__hash__`).
            let k = with_host(|h| h.to_key(payload))?;
            Ok(Value::Int(crate::builtins::hash_key(&k)))
        }
        "__int__" | "__index__" => {
            let n = with_host(|h| h.as_int(payload));
            match n {
                Some(n) => Ok(Value::Int(n)),
                None => crate::builtins::call_type_method(payload, name, args, kwargs),
            }
        }
        "__float__" => {
            let f = with_host(|h| h.num_val(payload));
            match f {
                Some(f) => Ok(Value::Float(f)),
                None => crate::builtins::call_type_method(payload, name, args, kwargs),
            }
        }
        _ => crate::builtins::call_type_method(payload, name, args, kwargs),
    }
}

/// Allocate an instance of `cname` for a cooperative `super().__new__(cls, …)`.
/// When `cname` subclasses a builtin type, the extra arguments build the native
/// payload (`class C(int): __new__ -> super().__new__(cls, v*2)`); otherwise a
/// bare instance (the `object.__new__` default).
fn new_subclass_or_bare(cname: &str, extra: &[Value]) -> Result<Value, String> {
    if let Some(base) = with_host(|h| h.builtin_base_of(cname)) {
        let payload = crate::builtins::call_builtin_function(base, extra.to_vec(), vec![])?;
        return Ok(with_host(|h| {
            h.new_instance_payload(cname.to_string(), payload)
        }));
    }
    Ok(with_host(|h| {
        h.new_instance(cname.to_string(), IndexMap::new())
    }))
}

/// `super().__init__(*args, **kwargs)` inside a builtin-type subclass: populate
/// the instance's native payload from the constructor arguments. For a mutable
/// base the payload's storage is replaced with a freshly-built base value; for
/// an immutable base the value was fixed at `__new__`, so this is a no-op.
fn base_super_init(
    base: &str,
    payload: &Value,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    match base {
        "list" | "dict" | "set" => {
            let built = crate::builtins::call_builtin_function(base, args, kwargs)?;
            with_host(|h| {
                if let Some(o) = h.get(&built).cloned() {
                    if let Some(slot) = h.get_mut(payload) {
                        *slot = o;
                    }
                }
            });
            Ok(Value::Undef)
        }
        // Immutable base: the value is set by `__new__`; `__init__` is a no-op.
        _ => Ok(Value::Undef),
    }
}

/// `recv.name(args)`.
pub fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let obj = with_host(|h| h.get(recv).cloned());
    match obj {
        Some(PyObj::Instance(inst)) => {
            // instance attribute that is callable?
            if let Some(v) = with_host(|h| h.inst_attr(&inst.dict, name)) {
                return invoke(&v, args, kwargs);
            }
            let class = inst.class.clone();
            if let Some(f) = with_host(|h| h.class_lookup(&class, name)) {
                let fobj = with_host(|h| h.get(&f).cloned());
                match fobj {
                    Some(PyObj::Func(fv)) => {
                        let owner = with_host(|h| method_owner(h, &class, name));
                        return run_user_func(&fv, Some(recv.clone()), owner, args, kwargs);
                    }
                    // `@staticmethod`: no implicit first argument.
                    Some(PyObj::StaticMethod(inner)) => return invoke(&inner, args, kwargs),
                    // `@classmethod`: bind the instance's class as `cls`.
                    Some(PyObj::ClassMethod(inner)) => {
                        let cls = with_host(|h| h.alloc(PyObj::Class(class.clone())));
                        let mut a = Vec::with_capacity(args.len() + 1);
                        a.push(cls);
                        a.extend(args);
                        return invoke(&inner, a, kwargs);
                    }
                    _ => return invoke(&f, args, kwargs),
                }
            }
            // Builtin-subclass instance: inherited methods / protocol dunders
            // delegate to the native payload (`stack.append(x)`, `u.upper()`,
            // `d.keys()`, and the `__len__`/`__getitem__`/… protocol).
            if !matches!(inst.payload, Value::Undef) {
                if let Some(base) = with_host(|h| h.builtin_base_of(&class)) {
                    return base_dispatch(recv, &inst.payload, base, name, args, kwargs);
                }
            }
            Err(format!(
                "AttributeError: '{class}' object has no attribute '{name}'"
            ))
        }
        Some(PyObj::Class(cname)) => {
            if let Some(f) = with_host(|h| h.class_lookup(&cname, name)) {
                let fobj = with_host(|h| h.get(&f).cloned());
                match fobj {
                    Some(PyObj::Func(fv)) => {
                        // Class.method(...) — no implicit self binding.
                        return run_user_func(&fv, None, Some(cname.clone()), args, kwargs);
                    }
                    Some(PyObj::StaticMethod(inner)) => return invoke(&inner, args, kwargs),
                    Some(PyObj::ClassMethod(inner)) => {
                        let cls = with_host(|h| h.alloc(PyObj::Class(cname.clone())));
                        let mut a = Vec::with_capacity(args.len() + 1);
                        a.push(cls);
                        a.extend(args);
                        return invoke(&inner, a, kwargs);
                    }
                    _ => return invoke(&f, args, kwargs),
                }
            }
            // A method defined on the metaclass is callable on the class, bound to
            // the class as its receiver (`cls`): `A.meta_method()`.
            let meta = with_host(|h| {
                h.classes
                    .get(&cname)
                    .map(|c| c.metaclass.clone())
                    .unwrap_or_else(|| "type".into())
            });
            if meta != "type" {
                if let Some(f) = with_host(|h| h.class_lookup(&meta, name)) {
                    if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&f).cloned()) {
                        let clsobj = with_host(|h| h.alloc(PyObj::Class(cname.clone())));
                        let owner = with_host(|h| method_owner(h, &meta, name));
                        return run_user_func(&fv, Some(clsobj), owner, args, kwargs);
                    }
                }
            }
            Err(format!(
                "AttributeError: type object '{cname}' has no attribute '{name}'"
            ))
        }
        Some(PyObj::Module { ns, name: mname }) => match ns.get(name).cloned() {
            Some(v) => invoke(&v, args, kwargs),
            // Native-shadowed module miss (`math.isqrt(…)`): resolve the symbol
            // from the real CPython module over the FFI bridge, then call it.
            None => match with_host(|h| module_ffi_fallback(h, &mname, name)) {
                Some(Ok(f)) => invoke(&f, args, kwargs),
                Some(Err(e)) => Err(e),
                None => Err(format!(
                    "AttributeError: module '{mname}' has no attribute '{name}'"
                )),
            },
        },
        Some(PyObj::Super { owner, instance }) => {
            let inst_class = match with_host(|h| h.get(&instance).cloned()) {
                Some(PyObj::Instance(i)) => i.class,
                _ => owner.clone(),
            };
            match with_host(|h| super_lookup(h, &owner, &inst_class, name)) {
                Some((f, found)) => {
                    let fobj = with_host(|h| h.get(&f).cloned());
                    if let Some(PyObj::Func(fv)) = fobj {
                        return run_user_func(&fv, Some(instance), Some(found), args, kwargs);
                    }
                    invoke(&f, args, kwargs)
                }
                // A `super().<m>(...)` inside a builtin-type subclass reaches the
                // builtin base: `super().__init__(it)` fills the native payload,
                // `super().append(x)` / `super().upper()` run the base method.
                None if with_host(|h| h.builtin_base_of(&inst_class).is_some())
                    && !matches!(
                        with_host(|h| match h.get(&instance) {
                            Some(PyObj::Instance(i)) => i.payload.clone(),
                            _ => Value::Undef,
                        }),
                        Value::Undef
                    ) =>
                {
                    let base = with_host(|h| h.builtin_base_of(&inst_class)).unwrap();
                    let payload = with_host(|h| match h.get(&instance) {
                        Some(PyObj::Instance(i)) => i.payload.clone(),
                        _ => Value::Undef,
                    });
                    if name == "__init__" {
                        return base_super_init(base, &payload, args, kwargs);
                    }
                    if name == "__new__" {
                        return Ok(instance.clone());
                    }
                    base_dispatch(&instance, &payload, base, name, args, kwargs)
                }
                // A metaclass's cooperative `super().<m>(...)` falls through to
                // the builtin `type`'s implementation.
                None if with_host(|h| class_inherits_type(h, &owner)) => {
                    match name {
                        // `type.__new__(mcls, name, bases, ns)` builds the class.
                        "__new__" if args.len() >= 4 => {
                            let mcls_name = with_host(|h| match h.get(&args[0]) {
                                Some(PyObj::Class(n)) => n.clone(),
                                _ => owner.clone(),
                            });
                            crate::builtins::type_new_meta(&args[1], &args[2], &args[3], &mcls_name)
                        }
                        // `type.__init__` is a no-op.
                        "__init__" => Ok(Value::Undef),
                        // `type.__call__(cls, *args)` — instantiate `cls` normally
                        // (skipping the metaclass `__call__`, avoiding recursion).
                        "__call__" => {
                            let cls_name = with_host(|h| match h.get(&instance) {
                                Some(PyObj::Class(n)) => Some(n.clone()),
                                _ => None,
                            });
                            match cls_name {
                                Some(c) => instantiate_plain(&c, args, kwargs),
                                None => {
                                    Err(type_error("super().__call__: receiver is not a class"))
                                }
                            }
                        }
                        _ => Err(format!(
                            "AttributeError: 'super' object has no attribute '{name}'"
                        )),
                    }
                }
                // A cooperative `super().__new__(cls)` at the top of a normal MRO
                // falls through to `object.__new__`: allocate a bare instance.
                None if name == "__new__" => {
                    let cls = args.first().cloned().unwrap_or(Value::Undef);
                    match with_host(|h| h.get(&cls).cloned()) {
                        Some(PyObj::Class(cname)) => Ok(new_subclass_or_bare(&cname, &args[1..])?),
                        _ => Err(type_error("object.__new__(X): X is not a type object")),
                    }
                }
                // `super().__init__(*args)` — for an exception instance this is
                // `BaseException.__init__`, which sets `self.args = args`;
                // otherwise the `object.__init__` no-op default.
                None if name == "__init__" => {
                    if with_host(|h| h.class_is_exception(&inst_class)) {
                        with_host(|h| {
                            let t = h.alloc(PyObj::Tuple(args.clone()));
                            let _ = h.set_attr(&instance, "args", t);
                        });
                    }
                    Ok(Value::Undef)
                }
                // `super().__init_subclass__()` bottoms out at `object`'s no-op
                // default (PEP 487): a cooperative chain reaching the top returns
                // `None`. (`object` has no `__set_name__`, so that still errors.)
                None if name == "__init_subclass__" => Ok(Value::Undef),
                None => Err(format!(
                    "AttributeError: 'super' object has no attribute '{name}'"
                )),
            }
        }
        // `object.__new__(cls)` — allocate a bare instance of `cls` (the default
        // `__new__`, reached from a user `__new__` override).
        Some(PyObj::Builtin(bname)) if bname == "object" && name == "__new__" => {
            let cls = args.first().cloned().unwrap_or(Value::Undef);
            match with_host(|h| h.get(&cls).cloned()) {
                Some(PyObj::Class(cname)) => {
                    Ok(with_host(|h| h.new_instance(cname, IndexMap::new())))
                }
                _ => Err(type_error("object.__new__(X): X is not a type object")),
            }
        }
        // `object.__getattribute__/__setattr__/__delattr__(self, ...)` — the
        // default attribute protocol, reached when a user override cooperates via
        // `object.__dunder__(self, ...)`. These run the RAW lookup/store so they
        // never re-enter the user override (which would recurse forever).
        Some(PyObj::Builtin(bname))
            if bname == "object"
                && matches!(name, "__getattribute__" | "__setattr__" | "__delattr__") =>
        {
            let selfv = args.first().cloned().unwrap_or(Value::Undef);
            let attr = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            match name {
                "__getattribute__" => crate::builtins::raw_getattr(&selfv, &attr),
                "__setattr__" => {
                    let v = args.get(2).cloned().unwrap_or(Value::Undef);
                    crate::builtins::raw_setattr(&selfv, &attr, v).map(|_| Value::Undef)
                }
                _ => crate::builtins::raw_delattr(&selfv, &attr).map(|_| Value::Undef),
            }
        }
        // `foreign.method(...)` (stdlib-ffi) — dispatch on the CPython side.
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::call_method(id, name, args, kwargs),
        // A method fetched from a builtin *type* object (`dict.fromkeys(...)`):
        // resolve the attribute (a callable builtin) then invoke it.
        Some(PyObj::Builtin(_)) => match with_host(|h| h.get_attr(recv, name)) {
            Ok(f) => invoke(&f, args, kwargs),
            Err(_) => crate::builtins::call_type_method(recv, name, args, kwargs),
        },
        _ => crate::builtins::call_type_method(recv, name, args, kwargs),
    }
}

/// If `cls` is a user class whose metaclass overrides `name` (used for
/// `__instancecheck__` / `__subclasscheck__`), invoke the override bound to the
/// class and return its result. `None` means "no override" — the caller falls
/// back to the structural check. Ordinary classes (metaclass `type`) and any
/// non-class value return `None`.
pub fn metaclass_hook(cls: &Value, name: &str, arg: Value) -> Option<Result<Value, String>> {
    let cname = match with_host(|h| h.get(cls).cloned()) {
        Some(PyObj::Class(n)) => n,
        _ => return None,
    };
    let meta = with_host(|h| {
        h.classes
            .get(&cname)
            .map(|c| c.metaclass.clone())
            .unwrap_or_else(|| "type".into())
    });
    if meta == "type" || !with_host(|h| h.class_lookup(&meta, name).is_some()) {
        return None;
    }
    Some(call_method(cls, name, vec![arg], vec![]))
}

/// If `cls` is a user class defining `__class_getitem__` (an implicit
/// classmethod), invoke it with the class and `item`, returning the result.
/// `None` means the class has no such hook — the caller reports the normal
/// "not subscriptable" error.
pub fn class_getitem(cls: &Value, item: Value) -> Option<Result<Value, String>> {
    let cname = match with_host(|h| h.get(cls).cloned()) {
        Some(PyObj::Class(n)) => n,
        _ => return None,
    };
    let f = with_host(|h| h.class_lookup(&cname, "__class_getitem__"))?;
    // Implicit classmethod: bind the class as the leading `cls` argument whether
    // the body was wrapped with `@classmethod` or written bare.
    let inner = match with_host(|h| h.get(&f).cloned()) {
        Some(PyObj::ClassMethod(inner)) => inner,
        _ => f,
    };
    Some(invoke(&inner, vec![cls.clone(), item], vec![]))
}

/// Resolve `name` for a `super` proxy: search the MRO of `inst_class` strictly
/// AFTER `owner`, returning the found `(func_value, defining_class)`.
fn super_lookup(h: &PyHost, owner: &str, inst_class: &str, name: &str) -> Option<(Value, String)> {
    let mro = h.mro_of(inst_class);
    let start = mro.iter().position(|c| c == owner).map(|i| i + 1)?;
    for c in &mro[start..] {
        if let Some(cd) = h.classes.get(c) {
            if let Some(v) = cd.ns.get(name) {
                return Some((v.clone(), c.clone()));
            }
        }
    }
    None
}

fn method_owner(h: &PyHost, class: &str, name: &str) -> Option<String> {
    for c in h.mro_of(class) {
        if let Some(cd) = h.classes.get(&c) {
            if cd.ns.contains_key(name) {
                return Some(c);
            }
        }
    }
    None
}

/// Construct an instance of `class` and run its `__init__`.
pub fn instantiate(
    class: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // Builtin exception classes construct exception objects.
    if crate::builtins::is_exception_class(class) && !with_host(|h| h.classes.contains_key(class)) {
        return Ok(with_host(|h| {
            h.alloc(PyObj::Exception {
                class: class.to_string(),
                args,
            })
        }));
    }
    // If `class`'s metaclass defines `__call__`, it controls instantiation:
    // `A(...)` dispatches to `type(A).__call__(A, ...)`.
    let meta = with_host(|h| h.classes.get(class).map(|c| c.metaclass.clone()));
    if let Some(m) = &meta {
        if m != "type" {
            if let Some(f) = with_host(|h| h.class_lookup(m, "__call__")) {
                if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&f).cloned()) {
                    let clsobj = with_host(|h| h.alloc(PyObj::Class(class.to_string())));
                    let owner = with_host(|h| method_owner(h, m, "__call__"));
                    return run_user_func(&fv, Some(clsobj), owner, args, kwargs);
                }
            }
        }
    }
    instantiate_plain(class, args, kwargs)
}

/// The default `type.__call__`: build a class instance via `__new__`/`__init__`
/// (or a metaclass's class object), *without* consulting a metaclass `__call__`.
/// Reached directly and from a metaclass's `super().__call__(...)`.
pub fn instantiate_plain(
    class: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // Instantiating a metaclass builds a *class* object (not an instance):
    // `M(name, bases, ns)` runs `M.__new__` / `M.__init__`.
    if with_host(|h| class_inherits_type(h, class)) {
        return metaclass_instantiate(class, args, kwargs);
    }
    // `__new__` (if the class overrides it) creates the instance; it is an
    // implicit staticmethod, so `cls` is passed as the first argument. `cls` is
    // also installed as the frame `self` so a zero-arg `super().__new__(cls)`
    // resolves. Otherwise a bare instance is allocated (default `object.__new__`).
    let inst = if let Some(newf) = with_host(|h| h.class_lookup(class, "__new__")) {
        let newf = match with_host(|h| h.get(&newf).cloned()) {
            Some(PyObj::StaticMethod(inner)) => inner,
            _ => newf,
        };
        let clsobj = with_host(|h| h.alloc(PyObj::Class(class.to_string())));
        if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&newf).cloned()) {
            let owner = with_host(|h| method_owner(h, class, "__new__"));
            run_user_func(&fv, Some(clsobj), owner, args.clone(), kwargs.clone())?
        } else {
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(clsobj);
            a.extend(args.clone());
            invoke(&newf, a, kwargs.clone())?
        }
    } else if let Some(base) = with_host(|h| h.builtin_base_of(class)) {
        // Subclass of a builtin type (`class Stack(list)`, `class C(int)`): the
        // default `__new__`/`__init__` initialize the inherited native payload.
        // An immutable base (int/float/str/tuple/frozenset) is fixed at
        // `__new__` from the constructor args. A mutable base (list/dict/set)
        // is filled from the args unless the subclass defines `__init__` (which
        // controls filling, typically via `super().__init__(...)`), in which
        // case it starts empty.
        let immutable = matches!(base, "int" | "float" | "str" | "tuple" | "frozenset");
        let has_user_init = with_host(|h| {
            matches!(
                h.class_lookup(class, "__init__")
                    .and_then(|f| h.get(&f).cloned()),
                Some(PyObj::Func(_))
            )
        });
        let payload = if immutable || !has_user_init {
            crate::builtins::call_builtin_function(base, args.clone(), kwargs.clone())?
        } else {
            crate::builtins::call_builtin_function(base, vec![], vec![])?
        };
        with_host(|h| h.new_instance_payload(class.to_string(), payload))
    } else {
        with_host(|h| {
            let mut attrs = IndexMap::new();
            // `BaseException.__new__(cls, *args)` seeds `self.args` with the
            // constructor's positional args (overridable by `__init__`/super).
            if h.class_is_exception(class) {
                let t = h.alloc(PyObj::Tuple(args.clone()));
                attrs.insert("args".to_string(), t);
            }
            h.new_instance(class.to_string(), attrs)
        })
    };
    // `__init__` runs only when `__new__` returned an instance of `class` (or a
    // subclass) — matching CPython's `type.__call__`.
    let init_ok = with_host(|h| match h.get(&inst) {
        Some(PyObj::Instance(i)) => h.mro_of(&i.class).iter().any(|c| c == class),
        _ => false,
    });
    if init_ok {
        if let Some(f) = with_host(|h| h.class_lookup(class, "__init__")) {
            let fobj = with_host(|h| h.get(&f).cloned());
            if let Some(PyObj::Func(fv)) = fobj {
                let owner = with_host(|h| method_owner(h, class, "__init__"));
                run_user_func(&fv, Some(inst.clone()), owner, args, kwargs)?;
            }
        }
    }
    Ok(inst)
}

/// Instantiate a metaclass `meta` — i.e. build a new class object from
/// `(name, bases, namespace)`. Runs `meta.__new__` (or the default `type.__new__`)
/// then `meta.__init__(cls, name, bases, ns)`, mirroring `type.__call__`.
fn metaclass_instantiate(
    meta: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // __new__ produces the class object.
    let newcls = if let Some(newf) = with_host(|h| h.class_lookup(meta, "__new__")) {
        let newf = match with_host(|h| h.get(&newf).cloned()) {
            Some(PyObj::StaticMethod(inner)) => inner,
            _ => newf,
        };
        let metaobj = with_host(|h| h.alloc(PyObj::Class(meta.to_string())));
        if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&newf).cloned()) {
            let owner = with_host(|h| method_owner(h, meta, "__new__"));
            run_user_func(&fv, Some(metaobj), owner, args.clone(), kwargs.clone())?
        } else {
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(metaobj);
            a.extend(args.clone());
            invoke(&newf, a, kwargs.clone())?
        }
    } else if args.len() >= 3 {
        // Default `type.__new__(meta, name, bases, ns)`.
        crate::builtins::type_new_meta(&args[0], &args[1], &args[2], meta)?
    } else {
        return Err(type_error("type() takes 1 or 3 arguments"));
    };
    // __init__(cls, name, bases, ns) — only if `meta` defines one and the class
    // was actually produced.
    let is_class = with_host(|h| matches!(h.get(&newcls), Some(PyObj::Class(_))));
    if is_class {
        if let Some(f) = with_host(|h| h.class_lookup(meta, "__init__")) {
            if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&f).cloned()) {
                let owner = with_host(|h| method_owner(h, meta, "__init__"));
                run_user_func(&fv, Some(newcls.clone()), owner, args, kwargs)?;
            }
        }
    }
    Ok(newcls)
}

/// Execute a user function/closure body on a fresh frame.
pub fn run_user_func(
    fv: &FuncVal,
    self_opt: Option<Value>,
    owner_opt: Option<String>,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let def = with_host(|h| h.funcs[fv.def_id].clone());
    let self_val = self_opt.or_else(|| fv.bound.clone());
    let mut pos = args;
    if let Some(s) = &self_val {
        pos.insert(0, s.clone());
    }
    let env = new_env(fv.env.clone());
    bind_params(&env, &def, &fv.defaults, &fv.kwonly_defaults, pos, kwargs)?;
    let owner = owner_opt.or_else(|| fv.owner.clone());
    // `async def`: calling it returns a coroutine object (or, if the body
    // contains `yield`, an async generator); the body runs only when the event
    // loop drives it (CPython does not execute it eagerly).
    if def.is_async {
        if def.is_generator {
            return Ok(make_async_generator(
                def.chunk.clone(),
                env,
                self_val,
                owner,
                def.name.clone(),
                def.locals.clone(),
            ));
        }
        return Ok(make_coroutine(
            def.chunk.clone(),
            env,
            self_val,
            owner,
            def.name.clone(),
            def.locals.clone(),
        ));
    }
    // Generator function: build a suspended coroutine over the already-bound
    // frame; nothing of the body runs until the first `next`/iteration.
    if def.is_generator {
        return Ok(make_generator(
            def.chunk.clone(),
            env,
            self_val,
            owner,
            def.name.clone(),
            def.locals.clone(),
        ));
    }
    with_host(|h| {
        h.frames.push(Frame {
            env,
            globals_decl: HashSet::new(),
            nonlocals_decl: HashSet::new(),
            locals_set: def.locals.iter().cloned().collect(),
            is_class_body: false,
            self_obj: self_val,
            owner,
            name: def.name.clone(),
            line: 0,
        })
    });
    let r = run_chunk_on(def.chunk.clone());
    let sig = with_host(|h| {
        if r.is_err() {
            h.push_tb_frame();
        }
        h.frames.pop();
        h.signal.take()
    });
    match r {
        Err(e) => Err(e),
        Ok(_) => Ok(match sig {
            Some(Signal::Return(v)) => v,
            _ => Value::Undef,
        }),
    }
}

/// Join argument names the way CPython's `format_missing`/`too_many_positional`
/// helpers do: `'a'`, `'a' and 'b'`, `'a', 'b', and 'c'` (Oxford comma at 3+).
fn join_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => format!("'{}'", names[0]),
        2 => format!("'{}' and '{}'", names[0], names[1]),
        n => {
            let head: Vec<String> = names[..n - 1].iter().map(|s| format!("'{s}'")).collect();
            format!("{}, and '{}'", head.join(", "), names[n - 1])
        }
    }
}

/// Bind positional + keyword arguments into a fresh call environment.
///
/// The check order mirrors CPython's argument binder (Python/ceval.c
/// `initialize_locals`) so error messages surface in the same precedence:
/// keyword collisions (multiple-values) and invalid keywords fire before a
/// too-many-positional error, which in turn fires before missing-argument
/// errors. Deviating from this order changes which `TypeError` a caller sees.
fn bind_params(
    env: &Env,
    def: &FuncDef,
    defaults: &[Value],
    kwonly_defaults: &[Value],
    pos: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<(), String> {
    let np = def.params.len();
    let ndef = def.ndefaults;
    let posonly = def.posonly.min(np);
    // A named `*args` (`Some(non-empty)`) soaks up extra positionals; a bare `*`
    // (`Some("")`, keyword-only marker) does not — extras are an error there.
    let has_vararg = def.star.as_deref().is_some_and(|s| !s.is_empty());
    let mut vars: IndexMap<String, Value> = IndexMap::new();
    let mut star_items = Vec::new();
    let npos = pos.len();

    // 1. Place positional args into their slots; keep the overflow aside.
    for (i, val) in pos.into_iter().enumerate() {
        if i < np {
            vars.insert(def.params[i].clone(), val);
        } else {
            star_items.push(val);
        }
    }

    // 2. Bind keyword args in call order. A keyword naming an already-filled
    //    positional slot is `multiple values`; positional-only names and unknown
    //    names defer to the leftover bucket (posonly/unexpected/`**kwargs`).
    let kwonly_given = kwargs
        .iter()
        .filter(|(k, _)| def.kwonly.contains(k))
        .count();
    let mut leftover: Vec<(String, Value)> = Vec::new();
    for (k, v) in kwargs {
        if let Some(idx) = def.params.iter().position(|p| p == &k) {
            if idx < posonly {
                leftover.push((k, v));
            } else if vars.contains_key(&k) {
                return Err(type_error(&format!(
                    "{}() got multiple values for argument '{}'",
                    def.name, k
                )));
            } else {
                vars.insert(k, v);
            }
        } else if def.kwonly.contains(&k) {
            vars.insert(k, v);
        } else {
            leftover.push((k, v));
        }
    }

    // 3. Reject invalid leftovers unless a `**kwargs` absorbs them. CPython
    //    reports positional-only-as-keyword before a plain unexpected keyword.
    if def.kwargs.is_none() && !leftover.is_empty() {
        let bad_posonly: Vec<String> = def.params[..posonly]
            .iter()
            .filter(|p| leftover.iter().any(|(k, _)| k == *p))
            .cloned()
            .collect();
        if !bad_posonly.is_empty() {
            return Err(type_error(&format!(
                "{}() got some positional-only arguments passed as keyword arguments: '{}'",
                def.name,
                bad_posonly.join(", ")
            )));
        }
        return Err(type_error(&format!(
            "{}() got an unexpected keyword argument '{}'",
            def.name, leftover[0].0
        )));
    }

    // 4. Too many positionals (no `*args` to catch them).
    if npos > np && !has_vararg {
        return Err(type_error(&format!(
            "{}() {}",
            def.name,
            too_many_positional(np, ndef, npos, kwonly_given)
        )));
    }

    // 5. Fill defaults for unbound positional slots; collect the still-missing.
    let mut missing: Vec<String> = Vec::new();
    for i in 0..np {
        if !vars.contains_key(&def.params[i]) {
            if i >= np - ndef {
                vars.insert(def.params[i].clone(), defaults[i - (np - ndef)].clone());
            } else {
                missing.push(def.params[i].clone());
            }
        }
    }
    if !missing.is_empty() {
        let plural = if missing.len() == 1 { "" } else { "s" };
        return Err(type_error(&format!(
            "{}() missing {} required positional argument{}: {}",
            def.name,
            missing.len(),
            plural,
            join_names(&missing)
        )));
    }

    // 6. Bind the `*args` tuple (bare `*` has no name to bind).
    if has_vararg {
        let name = def.star.clone().unwrap_or_default();
        let t = with_host(|h| h.new_tuple(star_items));
        vars.insert(name, t);
    }

    // 7. Fill keyword-only defaults; collect the still-missing required ones.
    //    `kwonly_defaults` holds only the defaulted kwonly params, in kwonly
    //    order; walk it with a separate cursor as we pass each optional param.
    let mut kwdef_cursor = 0usize;
    let mut missing_kw: Vec<String> = Vec::new();
    for (j, name) in def.kwonly.iter().enumerate() {
        let required = def.kwonly_required.get(j).copied().unwrap_or(true);
        if vars.contains_key(name) {
            if !required {
                kwdef_cursor += 1;
            }
        } else if required {
            missing_kw.push(name.clone());
        } else {
            let d = kwonly_defaults
                .get(kwdef_cursor)
                .cloned()
                .unwrap_or(Value::Undef);
            vars.insert(name.clone(), d);
            kwdef_cursor += 1;
        }
    }
    if !missing_kw.is_empty() {
        let plural = if missing_kw.len() == 1 { "" } else { "s" };
        return Err(type_error(&format!(
            "{}() missing {} required keyword-only argument{}: {}",
            def.name,
            missing_kw.len(),
            plural,
            join_names(&missing_kw)
        )));
    }

    // 8. Route leftover keywords into `**kwargs` (order preserved).
    if let Some(kw) = &def.kwargs {
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
        for (k, v) in leftover {
            let kv = with_host(|h| h.new_str(k.clone()));
            d.insert(PKey::Str(k), (kv, v));
        }
        let dict = with_host(|h| h.new_dict(d));
        vars.insert(kw.clone(), dict);
    }

    env.borrow_mut().vars = vars;
    Ok(())
}

/// CPython's `too_many_positional` message tail (everything after `name()`):
/// `takes <n> positional arguments but <m> were given`, with the `from X to Y`
/// range form when the callable has positional defaults, and the extra
/// `(and K keyword-only arguments)` clause when keyword-only args were supplied.
fn too_many_positional(np: usize, ndef: usize, posgiven: usize, kwonly_given: usize) -> String {
    let takes = if ndef > 0 {
        format!("from {} to {} positional arguments", np - ndef, np)
    } else if np == 1 {
        "1 positional argument".to_string()
    } else {
        format!("{np} positional arguments")
    };
    let given = if kwonly_given > 0 {
        let ps = if posgiven == 1 { "" } else { "s" };
        let ks = if kwonly_given == 1 { "" } else { "s" };
        format!(
            "{posgiven} positional argument{ps} (and {kwonly_given} keyword-only argument{ks}) were given"
        )
    } else if posgiven == 1 {
        "1 was given".to_string()
    } else {
        format!("{posgiven} were given")
    };
    format!("takes {takes} but {given}")
}

// ── more host operations referenced from builtins ────────────────────────────

impl PyHost {
    /// Resolve a function introspection dunder to its value: `__name__` /
    /// `__qualname__` from the `FuncDef`, `__module__` is always `__main__` (the
    /// script module), and `__defaults__` is the positional-default tuple (or
    /// `None` when there are none), matching CPython.
    fn func_dunder(
        &mut self,
        name: &str,
        def_id: usize,
        defaults: &[Value],
    ) -> Result<Value, String> {
        match name {
            "__name__" => {
                let n = self.funcs[def_id].name.clone();
                Ok(self.new_str(n))
            }
            "__qualname__" => {
                let d = &self.funcs[def_id];
                let q = if d.qualname.is_empty() {
                    d.name.clone()
                } else {
                    d.qualname.clone()
                };
                Ok(self.new_str(q))
            }
            "__module__" => Ok(self.new_str("__main__".to_string())),
            // `__defaults__`: a tuple of the positional defaults, or `None`.
            _ => {
                if defaults.is_empty() {
                    Ok(Value::Undef)
                } else {
                    Ok(self.new_tuple(defaults.to_vec()))
                }
            }
        }
    }

    /// The environment a closure defined in the current frame captures. A class
    /// body is not a lexical scope for its methods (CPython): a function defined
    /// there captures the class body's PARENT env, so `class C: x=1; def m(self):
    /// return x` resolves `x` in the enclosing/module scope, not the class body.
    /// The class namespace stays reachable only via `self`/`C`, never by name.
    pub fn current_env_capture(&self) -> Env {
        let f = self.frame();
        if f.is_class_body {
            if let Some(parent) = f.env.borrow().parent.clone() {
                return parent;
            }
        }
        f.env.clone()
    }

    /// Build the `"Class: message"` display string for an exception's args.
    /// The `args` tuple stored on a user exception instance's dict — set by the
    /// builtin `BaseException.__new__`/`__init__`. Missing (or non-tuple) → empty.
    pub fn exc_instance_args(&self, dict: &Value) -> Vec<Value> {
        match self.inst_attr(dict, "args") {
            Some(v) => match self.get(&v) {
                Some(PyObj::Tuple(t)) => t.clone(),
                _ => vec![v.clone()],
            },
            None => Vec::new(),
        }
    }

    pub fn exc_message(&self, class: &str, args: &[Value]) -> String {
        if args.is_empty() {
            String::new()
        } else if args.len() == 1 {
            // `KeyError.__str__` returns `repr(args[0])`, so `KeyError('k')`
            // stringifies to `'k'` (and its uncaught line is `KeyError: 'k'`).
            if self.is_keyerror_str_class(class) {
                self.repr_of(&args[0])
            } else {
                self.str_of(&args[0])
            }
        } else {
            let inner: Vec<String> = args.iter().map(|a| self.repr_of(a)).collect();
            format!("({})", inner.join(", "))
        }
    }

    /// Whether `class` uses `KeyError`'s `__str__` (repr the single arg): the
    /// builtin `KeyError` or a user subclass that doesn't override `__str__`.
    fn is_keyerror_str_class(&self, class: &str) -> bool {
        if class == "KeyError" {
            return true;
        }
        self.classes.contains_key(class) && self.mro_of(class).iter().any(|c| c == "KeyError")
    }

    /// The terse `Class: message` (or bare `Class`) line an exception value
    /// would abort with. Used to decide whether the in-flight `h.exc` actually
    /// corresponds to a just-raised builtin error string, or is a stale
    /// still-being-handled exception that must not shadow the real one.
    pub fn exc_line_of(&self, exc: &Value) -> Option<String> {
        match self.get(exc) {
            Some(PyObj::Exception { class, args }) => {
                Some(join_exc(class, &self.exc_message(class, args)))
            }
            Some(PyObj::Instance(i)) if self.class_is_exception(&i.class) => {
                let a = self.exc_instance_args(&i.dict);
                Some(join_exc(&i.class, &self.exc_message(&i.class, &a)))
            }
            _ => None,
        }
    }

    /// Raise a `KeyError` for a missing `key`: build the exception object with
    /// the bare key as its single arg (so `.args`/`repr`/`__str__` match
    /// CPython), link its `__context__` to the exception currently being
    /// handled, install it as the in-flight exception, and return the terse
    /// `KeyError: <repr>` line to abort with.
    pub fn key_error(&mut self, key: &Value) -> String {
        let repr = self.repr_of(key);
        let context = self.exc.clone().unwrap_or(Value::Undef);
        let e = self.alloc(PyObj::Exception {
            class: "KeyError".to_string(),
            args: vec![key.clone()],
        });
        let ctx = match &context {
            Value::Obj(_) if e != context => context,
            _ => Value::Undef,
        };
        self.set_exc_link(&e, Value::Undef, ctx);
        self.exc = Some(e);
        format!("KeyError: {repr}")
    }
}

/// Run a class body function to populate its namespace, then register the class.
/// `meta_name` is the explicit `metaclass=` (a user class name) if any;
/// `class_kwargs` are the remaining class-header keywords forwarded to
/// `__init_subclass__`.
/// Run a class-body function on a fresh class frame and return the namespace it
/// binds (member/method names in definition order). Shared by the native
/// `build_class` and the foreign-base (CPython metaclass) path.
fn run_class_body(name: &str, body_func: &Value) -> Result<IndexMap<String, Value>, String> {
    let fv = match with_host(|h| h.get(body_func).cloned()) {
        Some(PyObj::Func(fv)) => fv,
        _ => return Err(type_error("internal: class body is not a function")),
    };
    let def = with_host(|h| h.funcs[fv.def_id].clone());
    let env = new_env(fv.env.clone());
    with_host(|h| {
        h.frames.push(Frame {
            env: env.clone(),
            globals_decl: HashSet::new(),
            nonlocals_decl: HashSet::new(),
            // A class body resolves names dynamically (LOAD_NAME), so an unbound
            // read is a `NameError`, not `UnboundLocalError` — leave this empty.
            locals_set: HashSet::new(),
            is_class_body: true,
            self_obj: None,
            owner: Some(name.to_string()),
            name: name.to_string(),
            line: 0,
        })
    });
    let r = run_chunk_on(def.chunk.clone());
    with_host(|h| {
        if r.is_err() {
            h.push_tb_frame();
        }
        h.frames.pop();
        h.signal.take();
    });
    r?;
    let vars = env.borrow().vars.clone();
    Ok(vars)
}

/// Create a class that has at least one foreign (CPython) base by delegating to
/// that base's metaclass over the FFI bridge (`class C(enum.Enum): …` →
/// `EnumType`). The class body runs on fusevm; its namespace is handed to
/// CPython's `types.new_class`, which fires `__prepare__` and the real metaclass.
/// The result is a `Foreign` class handle.
#[cfg(feature = "stdlib-ffi")]
pub fn build_class_foreign(
    name: &str,
    bases: Vec<Value>,
    body_func: &Value,
) -> Result<Value, String> {
    let ns = run_class_body(name, body_func)?;
    let mut members: Vec<(String, Value)> = ns.into_iter().collect();
    // CPython class creation always provides `__module__`/`__qualname__` in the
    // namespace; some metaclasses (typing.NamedTuple) index them directly and
    // KeyError without. Supply them if the body didn't.
    if !members.iter().any(|(k, _)| k == "__module__") {
        let m = with_host(|h| h.new_str("__main__".to_string()));
        members.push(("__module__".to_string(), m));
    }
    if !members.iter().any(|(k, _)| k == "__qualname__") {
        let q = with_host(|h| h.new_str(name.to_string()));
        members.push(("__qualname__".to_string(), q));
    }
    crate::ffi::build_foreign_class(name, &bases, &members)
}

pub fn build_class(
    name: &str,
    bases: Vec<String>,
    body_func: &Value,
    meta_name: Option<String>,
    class_kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let def = match with_host(|h| h.get(body_func).cloned()) {
        Some(PyObj::Func(fv)) => with_host(|h| h.funcs[fv.def_id].clone()),
        _ => return Err(type_error("internal: class body is not a function")),
    };
    let ns: IndexMap<String, Value> = run_class_body(name, body_func)?;
    // The effective metaclass: the explicit `metaclass=` if given, else the most
    // derived metaclass inherited from the bases (CPython rule). A user metaclass
    // constructs the class via `M(name, bases, namespace)` (firing `M.__new__`/
    // `M.__init__`, tagging `type(cls) is M`); an implicit `type` registers directly.
    let effective_meta = match meta_name {
        Some(m) if with_host(|h| h.classes.contains_key(&m)) => Some(m),
        _ => {
            let dm = with_host(|h| default_metaclass(h, &bases));
            (dm != "type").then_some(dm)
        }
    };
    let cls = match &effective_meta {
        Some(m) => metaclass_create(m, name, &bases, &ns)?,
        None => with_host(|h| h.register_class(name, bases, ns.clone())),
    };
    // Record the class's `__qualname__` (carried on the class-body `FuncDef`,
    // whose qualname was set to the class's dotted path at compile time).
    if !def.qualname.is_empty() {
        with_host(|h| {
            if let Some(cd) = h.classes.get_mut(name) {
                cd.qualname = def.qualname.clone();
            }
        });
    }
    // Descriptor protocol: fire `__set_name__(owner, name)` on every class-body
    // value whose type defines it (in definition order).
    for (attr_name, val) in &ns {
        let fires = with_host(|h| match h.get(val) {
            Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__set_name__").is_some(),
            _ => false,
        });
        if fires {
            let owner = with_host(|h| h.alloc(PyObj::Class(name.to_string())));
            let nm = with_host(|h| h.new_str(attr_name.clone()));
            call_method(val, "__set_name__", vec![owner, nm], vec![])?;
        }
    }
    // PEP 487: fire the parent's `__init_subclass__` (an implicit classmethod)
    // with the new class and the leftover class-header keywords. Resolved along
    // the MRO strictly after the new class (CPython's `super().__init_subclass__`).
    let hook = with_host(|h| {
        h.mro_of(name).into_iter().skip(1).find_map(|c| {
            h.classes
                .get(&c)
                .and_then(|cd| cd.ns.get("__init_subclass__").cloned())
                .map(|v| (v, c))
        })
    });
    match hook {
        Some((v, owner)) => {
            let inner = match with_host(|h| h.get(&v).cloned()) {
                Some(PyObj::ClassMethod(f)) => f,
                _ => v,
            };
            if let Some(PyObj::Func(fv)) = with_host(|h| h.get(&inner).cloned()) {
                let clsobj = with_host(|h| h.alloc(PyObj::Class(name.to_string())));
                run_user_func(&fv, Some(clsobj), Some(owner), vec![], class_kwargs)?;
            }
        }
        // Only `object.__init_subclass__` (the no-arg default) remains: extra
        // class keywords are an error, matching CPython.
        None if !class_kwargs.is_empty() => {
            return Err(type_error(&format!(
                "{name}.__init_subclass__() takes no keyword arguments"
            )));
        }
        None => {}
    }
    Ok(cls)
}

/// Construct a class through its metaclass: `M(name, (bases...), {ns...})`. This
/// runs `M`'s `__call__` (or the default `type.__call__` → `__new__`/`__init__`),
/// exactly like any `M(...)` call, and returns the new class object.
fn metaclass_create(
    meta: &str,
    name: &str,
    bases: &[String],
    ns: &IndexMap<String, Value>,
) -> Result<Value, String> {
    let name_v = with_host(|h| h.new_str(name.to_string()));
    let base_vals: Vec<Value> = with_host(|h| {
        bases
            .iter()
            .map(|b| h.alloc(PyObj::Class(b.clone())))
            .collect()
    });
    let bases_v = with_host(|h| h.new_tuple(base_vals));
    let ns_map: IndexMap<PKey, (Value, Value)> = with_host(|h| {
        ns.iter()
            .map(|(k, v)| {
                let kv = h.new_str(k.clone());
                (PKey::Str(k.clone()), (kv, v.clone()))
            })
            .collect()
    });
    let ns_v = with_host(|h| h.new_dict(ns_map));
    let meta_v = with_host(|h| h.alloc(PyObj::Class(meta.to_string())));
    invoke(&meta_v, vec![name_v, bases_v, ns_v], vec![])
}

/// The most-derived metaclass inherited from `bases` (CPython's rule for a class
/// with no explicit `metaclass=`): the metaclass that is a subclass of every
/// base's metaclass. `"type"` when no base carries a user metaclass.
fn default_metaclass(h: &PyHost, bases: &[String]) -> String {
    let mut winner = "type".to_string();
    for b in bases {
        let mb = h
            .classes
            .get(b)
            .map(|c| c.metaclass.clone())
            .unwrap_or_else(|| "type".into());
        if mb == winner {
            continue;
        }
        // Keep whichever metaclass derives from the other (more derived wins).
        if winner == "type" || class_is_subclass(h, &mb, &winner) {
            winner = mb;
        }
    }
    winner
}

/// Whether `sub` is `sup` or derives (transitively) from it.
fn class_is_subclass(h: &PyHost, sub: &str, sup: &str) -> bool {
    if sub == sup {
        return true;
    }
    match h.classes.get(sub) {
        Some(cd) => cd.bases.iter().any(|b| class_is_subclass(h, b, sup)),
        None => false,
    }
}

/// Whether `class` is a metaclass — i.e. it derives (transitively) from the
/// builtin `type`. A user metaclass is written `class M(type): ...`.
pub fn class_inherits_type(h: &PyHost, class: &str) -> bool {
    if class == "type" {
        return true;
    }
    match h.classes.get(class) {
        Some(cd) => cd
            .bases
            .iter()
            .any(|b| b == "type" || class_inherits_type(h, b)),
        None => false,
    }
}

/// Turn a raised value into an exception + the error string to abort with.
pub fn raise_value(exc: &Value) -> Result<String, String> {
    with_host(|h| {
        let obj = h.get(exc).cloned();
        match obj {
            Some(PyObj::Exception { class, args }) => {
                let msg = h.exc_message(&class, &args);
                h.exc = Some(exc.clone());
                Ok(join_exc(&class, &msg))
            }
            Some(PyObj::Builtin(name)) if crate::builtins::is_exception_class(&name) => {
                let e = h.alloc(PyObj::Exception {
                    class: name.clone(),
                    args: vec![],
                });
                h.exc = Some(e);
                Ok(name)
            }
            Some(PyObj::Class(name)) => {
                // Instantiate a user exception class with no args. An exception
                // class seeds `self.args = ()` (`BaseException.__new__`).
                let mut attrs = IndexMap::new();
                if h.class_is_exception(&name) {
                    let t = h.alloc(PyObj::Tuple(vec![]));
                    attrs.insert("args".to_string(), t);
                }
                let inst = h.new_instance(name.clone(), attrs);
                h.exc = Some(inst);
                Ok(name)
            }
            Some(PyObj::Instance(i)) => {
                let class = i.class.clone();
                // A user exception instance's uncaught line shows its message.
                let line = if h.class_is_exception(&class) {
                    let a = h.exc_instance_args(&i.dict);
                    join_exc(&class, &h.exc_message(&class, &a))
                } else {
                    class
                };
                h.exc = Some(exc.clone());
                Ok(line)
            }
            _ => Err(type_error("exceptions must derive from BaseException")),
        }
    })
}

fn join_exc(class: &str, msg: &str) -> String {
    if msg.is_empty() {
        class.to_string()
    } else {
        format!("{class}: {msg}")
    }
}

/// How an uncaught top-level exception ends the process.
pub enum TopExit {
    /// An uncaught `SystemExit`: exit with `code`, optionally after writing
    /// `message` (a non-int/non-None argument) to stderr.
    SystemExit { code: i32, message: Option<String> },
    /// Any other uncaught exception: print `traceback` to stderr, exit 1.
    Uncaught { traceback: String },
}

/// Classify the top-level error left on the host after a run. `err` is the run's
/// terse error string (e.g. `"ValueError: boom"`) used as the traceback's final
/// line. An uncaught `SystemExit` maps to CPython's exit-code rules; anything
/// else formats a `Traceback (most recent call last):` block.
pub fn classify_top_error(err: &str) -> TopExit {
    with_host(|h| {
        // Uncaught SystemExit (from `sys.exit` or `raise SystemExit`): CPython
        // prints no traceback and derives the exit status from the code.
        if let Some(Value::Obj(_)) = &h.exc {
            let exc = h.exc.clone().unwrap();
            if let Some(PyObj::Exception { class, args }) = h.get(&exc) {
                if class == "SystemExit" {
                    let args = args.clone();
                    return system_exit_outcome(h, &args);
                }
            }
        }
        TopExit::Uncaught {
            traceback: h.render_traceback(err),
        }
    })
}

/// Map a `SystemExit`'s args to an `(exit code, optional stderr message)`:
/// no args / `None` → 0; an int/bool → that value (masked to 8 bits by the OS);
/// a str or any other object → 1 with `str(arg)` on stderr.
fn system_exit_outcome(h: &mut PyHost, args: &[Value]) -> TopExit {
    let code = match args.len() {
        0 => Value::Undef,
        1 => args[0].clone(),
        _ => h.new_tuple(args.to_vec()),
    };
    match &code {
        Value::Undef => TopExit::SystemExit {
            code: 0,
            message: None,
        },
        Value::Bool(b) => TopExit::SystemExit {
            code: *b as i32,
            message: None,
        },
        Value::Int(n) => TopExit::SystemExit {
            code: *n as i32,
            message: None,
        },
        other => TopExit::SystemExit {
            code: 1,
            message: Some(format!("{}\n", h.str_of(other))),
        },
    }
}

impl PyHost {
    /// Render a CPython `Traceback (most recent call last):` block for an uncaught
    /// exception, ending with `err`. Frames run outermost (module) first; source
    /// lines are shown unless the program came from stdin. Caret markers are
    /// omitted (approximate for a first pass).
    pub fn render_traceback(&self, err: &str) -> String {
        let mut out = String::from("Traceback (most recent call last):\n");
        // Outermost = the module frame still on the stack; then the function
        // frames captured innermost-first as the exception unwound, reversed.
        let mut frames: Vec<(String, u32)> = Vec::new();
        if let Some(f) = self.frames.first() {
            frames.push((f.name.clone(), f.line));
        }
        for f in self.traceback.iter().rev() {
            frames.push(f.clone());
        }
        let src_lines: Vec<&str> = self.prog_source.lines().collect();
        for (name, line) in &frames {
            out.push_str(&format!(
                "  File \"{}\", line {}, in {}\n",
                self.tb_filename, line, name
            ));
            if self.tb_show_source && *line > 0 {
                if let Some(text) = src_lines.get((*line as usize).saturating_sub(1)) {
                    let stripped = text.trim();
                    if !stripped.is_empty() {
                        out.push_str(&format!("    {stripped}\n"));
                    }
                }
            }
        }
        out.push_str(err);
        out.push('\n');
        out
    }
}

// ── generators (stackful coroutines, same-thread via corosensei) ─────────────

impl PyHost {
    /// Swap the volatile execution context in one shot, returning the previous
    /// one — used to install a generator's context on resume and pull it back
    /// out on suspend/return, keeping caller and generator states isolated.
    fn install_gen_ctx(&mut self, mut c: GenContext) -> GenContext {
        std::mem::swap(&mut self.frames, &mut c.frames);
        std::mem::swap(&mut self.error, &mut c.error);
        std::mem::swap(&mut self.exc, &mut c.exc);
        std::mem::swap(&mut self.signal, &mut c.signal);
        c
    }
}

/// Build a suspended generator whose body is `chunk`, run in a frame with the
/// already-bound `env`. Nothing executes until the first `gen_resume`.
fn make_generator(
    chunk: Chunk,
    env: Env,
    self_val: Option<Value>,
    owner: Option<String>,
    func_name: String,
    locals: Vec<String>,
) -> Value {
    make_gen_kind(
        chunk,
        env,
        self_val,
        owner,
        GenKind::Generator,
        func_name,
        locals,
    )
}

/// Build a suspended `async def` coroutine object. Identical backing to a
/// generator (a stackful `corosensei` coroutine that suspends at each `await`),
/// but tagged `Coroutine` so `type().__name__` is `coroutine` and `repr` differs.
pub fn make_coroutine(
    chunk: Chunk,
    env: Env,
    self_val: Option<Value>,
    owner: Option<String>,
    func_name: String,
    locals: Vec<String>,
) -> Value {
    make_gen_kind(
        chunk,
        env,
        self_val,
        owner,
        GenKind::Coroutine,
        func_name,
        locals,
    )
}

/// Build a suspended async generator (`async def` containing `yield`). Its body
/// suspends both at `yield` (producing a value) and at `await` (yielding a Future
/// to the loop); the `awaiting` flag distinguishes the two for `__anext__`.
pub fn make_async_generator(
    chunk: Chunk,
    env: Env,
    self_val: Option<Value>,
    owner: Option<String>,
    func_name: String,
    locals: Vec<String>,
) -> Value {
    make_gen_kind(
        chunk,
        env,
        self_val,
        owner,
        GenKind::AsyncGen,
        func_name,
        locals,
    )
}

/// Whether `v` is an async generator object.
pub fn is_async_generator(v: &Value) -> bool {
    match with_host(|h| h.get(v).cloned()) {
        Some(PyObj::Generator { id }) => {
            with_host(|h| h.generators[id as usize].kind == GenKind::AsyncGen)
        }
        _ => false,
    }
}

/// Whether the running async generator's last suspension was an `await` (vs a
/// value-producing `yield`). Read by the `__anext__` driver right after resume.
pub fn cur_gen_awaiting(gen: &Value) -> bool {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].awaiting),
        _ => false,
    }
}

/// Queue the operation an `asend`/`athrow`/`aclose` awaitable will perform on its
/// next drive (see [`AGenOp`]).
pub fn set_agen_op(gen: &Value, op: AGenOp) {
    if let Some(PyObj::Generator { id }) = with_host(|h| h.get(gen).cloned()) {
        with_host(|h| h.generators[id as usize].agen_op = Some(op));
    }
}

/// Take (and clear) the pending async-generator op; `None` means a plain
/// `__anext__` step (`Send(None)`).
pub fn take_agen_op(gen: &Value) -> Option<AGenOp> {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].agen_op.take()),
        _ => None,
    }
}

/// Emit CPython's `RuntimeWarning: coroutine '<name>' was never awaited` (to
/// stderr) for every coroutine object that was created but never driven — i.e.
/// never `await`ed, `create_task`'d, or run. Called at program end (best-effort;
/// CPython emits at GC time, we emit once at teardown).
pub fn warn_unawaited_coroutines() {
    let names: Vec<String> = with_host(|h| {
        h.generators
            .iter()
            .filter(|g| g.kind == GenKind::Coroutine && !g.started && !g.done)
            .map(|g| g.func_name.clone())
            .collect()
    });
    for name in names {
        eprintln!("RuntimeWarning: coroutine '{name}' was never awaited");
    }
}

fn make_gen_kind(
    chunk: Chunk,
    env: Env,
    self_val: Option<Value>,
    owner: Option<String>,
    kind: GenKind,
    func_name: String,
    locals: Vec<String>,
) -> Value {
    let frame = Frame {
        env,
        globals_decl: HashSet::new(),
        nonlocals_decl: HashSet::new(),
        locals_set: locals.into_iter().collect(),
        is_class_body: false,
        self_obj: self_val,
        owner: owner.clone(),
        name: owner.unwrap_or_else(|| "<genexpr>".to_string()),
        line: 0,
    };
    let id = with_host(|h| {
        let id = h.generators.len() as u32;
        h.generators.push(GenCell {
            kind,
            coro: None,
            yielder: std::ptr::null(),
            ctx: GenContext {
                frames: vec![frame],
                ..GenContext::default()
            },
            done: false,
            started: false,
            pending_throw: None,
            ret_value: Value::Undef,
            awaiting: false,
            agen_op: None,
            func_name,
        });
        id
    });
    let coro = corosensei::Coroutine::new(
        move |yielder: &corosensei::Yielder<Value, Value>, _first: Value| {
            // Same thread → publish the yielder so `yield` (deep inside the
            // body's VM) can reach it. Valid for the whole body lifetime.
            with_host(|h| h.generators[id as usize].yielder = yielder as *const _ as *const ());
            let r = run_chunk_on(chunk);
            // A `return X` inside the body leaves a `Return(X)` signal; capture X
            // as the generator's return value (→ `StopIteration.value`) then drop
            // the signal so the generator's exhaustion is clean.
            with_host(|h| {
                if let Some(Signal::Return(v)) = h.signal.take() {
                    h.generators[id as usize].ret_value = v;
                }
            });
            r.map(|_| Value::Undef)
        },
    );
    with_host(|h| h.generators[id as usize].coro = Some(coro));
    with_host(|h| h.alloc(PyObj::Generator { id }))
}

/// `yield v` — suspend the running generator, handing `v` to the resumer;
/// returns the value the next `gen_resume(x)` supplies (a sent value, or None).
pub fn gen_yield(v: Value) -> Result<Value, String> {
    gen_suspend(v, false)
}

/// Like [`gen_yield`], but marks the suspension as an `await` (used by the async
/// runtime so an async generator's `__anext__` driver can tell an awaited Future
/// from a produced value).
pub fn gen_yield_awaiting(v: Value) -> Result<Value, String> {
    gen_suspend(v, true)
}

fn gen_suspend(v: Value, awaiting: bool) -> Result<Value, String> {
    match gen_suspend_raw(v, awaiting)? {
        Resumed::Send(s) => Ok(s),
        // A `.throw()`/`.close()` queued an exception to raise at this yield point.
        // `raise_value` sets `h.exc` and returns the abort string; propagate it as
        // an error so the body's own `try/except` can catch it.
        Resumed::Throw(exc) => Err(raise_value(&exc).unwrap_or_else(|e| e)),
    }
}

/// How a suspended generator was resumed: a `.send()` value or an exception
/// injected via `.throw()`/`.close()`. Plain `yield` collapses `Throw` into an
/// `Err` (see [`gen_suspend`]); `yield from` inspects it to forward the exception
/// into the sub-iterator (PEP 380).
enum Resumed {
    Send(Value),
    Throw(Value),
}

/// Suspend the running generator at a `yield`, returning how it was resumed
/// (a sent value, or an injected exception) WITHOUT collapsing a throw into an
/// error. This is the shared core of `gen_suspend` and the `yield from` driver.
fn gen_suspend_raw(v: Value, awaiting: bool) -> Result<Resumed, String> {
    let id = match CUR_GEN.with(|c| c.get()) {
        Some(id) => id,
        None => return Err(type_error("'yield' outside a generator")),
    };
    with_host(|h| h.generators[id as usize].awaiting = awaiting);
    let yp = with_host(|h| h.generators[id as usize].yielder);
    // SAFETY: same-thread coroutine; the yielder lives for the whole body, and
    // we only reach here from inside that body (its stack is live).
    let yielder = unsafe { &*(yp as *const corosensei::Yielder<Value, Value>) };
    let sent = yielder.suspend(v);
    if let Some(exc) = with_host(|h| h.generators[id as usize].pending_throw.take()) {
        return Ok(Resumed::Throw(exc));
    }
    Ok(Resumed::Send(sent))
}

/// One outcome of advancing the sub-iterator during `yield from` delegation.
enum SubStep {
    /// The sub-iterator yielded a value to re-yield from the delegating generator.
    Yield(Value),
    /// The sub-iterator is exhausted; carries its return (`StopIteration.value`).
    Return(Value),
}

/// The exception's class name (`Exception`/`Instance`/`Builtin` forms), used to
/// recognize `GeneratorExit` during `yield from` close-forwarding.
fn exc_class_name(v: &Value) -> Option<String> {
    with_host(|h| match h.get(v) {
        Some(PyObj::Exception { class, .. }) => Some(class.clone()),
        Some(PyObj::Instance(i)) => Some(i.class.clone()),
        Some(PyObj::Builtin(n)) => Some(n.clone()),
        _ => None,
    })
}

/// A finished sub-generator's return value (its `StopIteration.value`), or `None`
/// for a non-generator delegate.
fn gen_ret_of(it: &Value) -> Value {
    match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.gen_return_value(id)),
        _ => Value::Undef,
    }
}

/// Advance the sub-iterator by sending `s` (`Undef` = `next()`). A generator
/// delegate takes `.send(s)`; a plain iterator only accepts `next()` and errors
/// on a non-`None` send (CPython's `AttributeError: … has no attribute 'send'`).
fn sub_send(it: &Value, is_gen: bool, s: Value) -> Result<SubStep, String> {
    if is_gen {
        match gen_resume(it, s)? {
            Some(v) => Ok(SubStep::Yield(v)),
            None => Ok(SubStep::Return(gen_ret_of(it))),
        }
    } else {
        if !matches!(s, Value::Undef) {
            let tn = with_host(|h| h.type_name(it));
            return Err(format!(
                "AttributeError: '{tn}' object has no attribute 'send'"
            ));
        }
        match iter_step(it)? {
            Some(v) => Ok(SubStep::Yield(v)),
            None => Ok(SubStep::Return(Value::Undef)),
        }
    }
}

/// Forward a `.throw(exc)` into the sub-iterator. A generator delegate takes
/// `.throw`; a plain iterator has none, so the exception is raised in the
/// delegating frame (PEP 380's `raise _e`).
fn sub_throw(it: &Value, is_gen: bool, exc: Value) -> Result<SubStep, String> {
    if is_gen {
        match gen_throw(it, exc)? {
            Some(v) => Ok(SubStep::Yield(v)),
            None => Ok(SubStep::Return(gen_ret_of(it))),
        }
    } else {
        Err(raise_value(&exc).unwrap_or_else(|e| e))
    }
}

/// Forward a `.close()` (GeneratorExit) into a sub-generator, swallowing whatever
/// it produces (the delegating generator re-raises the GeneratorExit itself).
fn sub_close(it: &Value, is_gen: bool) {
    if is_gen {
        let ge = with_host(|h| {
            h.alloc(PyObj::Exception {
                class: "GeneratorExit".into(),
                args: vec![],
            })
        });
        let _ = gen_throw(it, ge);
        with_host(|h| {
            h.error = None;
            h.exc = None;
        });
    }
}

/// `yield from <it>` delegation (PEP 380): drive the sub-iterator `it`,
/// re-yielding each of its values from the delegating generator and forwarding
/// `.send()` values, `.throw()` exceptions, and `.close()` (GeneratorExit) into
/// the sub-iterator. Returns the sub-iterator's return value (its
/// `StopIteration.value`) so `r = yield from sub()` binds correctly.
pub fn run_yield_from(it: Value) -> Result<Value, String> {
    let is_gen = with_host(|h| matches!(h.get(&it), Some(PyObj::Generator { .. })));
    // `_y = next(_i)` — the first advance is always a plain `next()`.
    let mut y = match sub_send(&it, is_gen, Value::Undef)? {
        SubStep::Yield(v) => v,
        SubStep::Return(r) => return Ok(r),
    };
    loop {
        match gen_suspend_raw(y, false)? {
            // `_s = yield _y` → `next(_i)` if None, else `_i.send(_s)`.
            Resumed::Send(s) => match sub_send(&it, is_gen, s)? {
                SubStep::Yield(v) => y = v,
                SubStep::Return(r) => return Ok(r),
            },
            Resumed::Throw(exc) => {
                // GeneratorExit → close the sub-iterator, then re-raise it here.
                if exc_class_name(&exc).as_deref() == Some("GeneratorExit") {
                    sub_close(&it, is_gen);
                    return Err(raise_value(&exc).unwrap_or_else(|e| e));
                }
                // Any other thrown exception → forward to `_i.throw`.
                match sub_throw(&it, is_gen, exc)? {
                    SubStep::Yield(v) => y = v,
                    SubStep::Return(r) => return Ok(r),
                }
            }
        }
    }
}

/// Whether a generator has been resumed at least once (a fresh generator only
/// accepts `send(None)`).
pub fn gen_started(gen: &Value) -> bool {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].started),
        _ => false,
    }
}

/// The value a finished coroutine/generator `return`ed (its `StopIteration`
/// value). `None` (`Undef`) for a fall-off-the-end return.
pub fn coro_return_value(gen: &Value) -> Value {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.gen_return_value(id)),
        _ => Value::Undef,
    }
}

/// Whether `v` is a coroutine object (from an `async def`).
pub fn is_coroutine(v: &Value) -> bool {
    match with_host(|h| h.get(v).cloned()) {
        Some(PyObj::Generator { id }) => {
            with_host(|h| h.generators[id as usize].kind == GenKind::Coroutine)
        }
        _ => false,
    }
}

/// The `StopIteration` object carrying a finished generator's return value (its
/// `.value`). Built when `send`/`next`/`__next__` exhaust the generator.
pub fn gen_stop_iteration(gen: &Value) -> Value {
    let ret = match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].ret_value.clone()),
        _ => Value::Undef,
    };
    let args = if matches!(ret, Value::Undef) {
        vec![]
    } else {
        vec![ret]
    };
    with_host(|h| {
        let e = h.alloc(PyObj::Exception {
            class: "StopIteration".into(),
            args,
        });
        h.exc = Some(e.clone());
        e
    });
    with_host(|h| h.exc.clone().unwrap())
}

/// `gen.throw(exc)` — queue `exc` to raise at the current yield point, then
/// resume. Returns the next yielded value, or `Ok(None)` if the throw propagated
/// out of the generator (its body did not catch it — the error is on `h`).
pub fn gen_throw(gen: &Value, exc: Value) -> Result<Option<Value>, String> {
    let id = match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => id,
        _ => return Err(type_error("not a generator")),
    };
    // Throwing into a not-yet-started or finished generator raises in the caller.
    let (started, done) = with_host(|h| {
        let g = &h.generators[id as usize];
        (g.started, g.done)
    });
    if !started || done {
        return Err(raise_value(&exc).unwrap_or_else(|e| e));
    }
    with_host(|h| h.generators[id as usize].pending_throw = Some(exc));
    gen_resume(gen, Value::Undef)
}

/// Resume a generator until its next `yield` or its body returns. Returns
/// `Ok(Some(v))` for a yielded value, `Ok(None)` when exhausted, `Err` if the
/// body raised. Preserves the shared host: the coroutine is taken out so the
/// body re-enters `with_host` freely, and the volatile context is swapped so the
/// caller's frames/signal survive the switch.
pub fn gen_resume(gen: &Value, send: Value) -> Result<Option<Value>, String> {
    let id = match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => id,
        _ => return Err(type_error("not a generator")),
    };
    if with_host(|h| h.generators[id as usize].done) {
        return Ok(None);
    }
    let mut coro = match with_host(|h| h.generators[id as usize].coro.take()) {
        Some(c) => c,
        None => return Err("ValueError: generator already executing".into()),
    };
    let gen_ctx = with_host(|h| std::mem::take(&mut h.generators[id as usize].ctx));
    let caller_ctx = with_host(|h| h.install_gen_ctx(gen_ctx));
    let prev = CUR_GEN.with(|c| c.replace(Some(id)));
    with_host(|h| h.generators[id as usize].started = true);

    let out = coro.resume(send); // no host borrow held; body drives its own VM

    CUR_GEN.with(|c| c.set(prev));
    let gen_ctx = with_host(|h| h.install_gen_ctx(caller_ctx));
    with_host(|h| {
        h.generators[id as usize].ctx = gen_ctx;
        h.generators[id as usize].coro = Some(coro);
    });

    match out {
        corosensei::CoroutineResult::Yield(y) => Ok(Some(y)),
        corosensei::CoroutineResult::Return(r) => {
            with_host(|h| h.generators[id as usize].done = true);
            match r {
                Ok(_) => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Materialize any iterable — including a generator — into a `Vec`. Unlike the
/// `&mut self` `iter_items`, this holds NO host borrow across a generator
/// resume, so it is safe for generator-typed operands.
pub fn iter_vec(v: &Value) -> Result<Vec<Value>, String> {
    if with_host(|h| matches!(h.get(v), Some(PyObj::Generator { .. }))) {
        let mut out = Vec::new();
        while let Some(x) = gen_resume(v, Value::Undef)? {
            out.push(x);
        }
        return Ok(out);
    }
    // Lazy composite iterators (`zip`/`map`/`filter`/`enumerate`) drain via
    // `iter_step` so their (possibly generator) sources are pulled lazily.
    if with_host(|h| {
        matches!(
            h.get(v),
            Some(PyObj::Zip { .. })
                | Some(PyObj::MapObj { .. })
                | Some(PyObj::FilterObj { .. })
                | Some(PyObj::EnumerateObj { .. })
                | Some(PyObj::CallIter { .. })
        )
    }) {
        let mut out = Vec::new();
        while let Some(x) = iter_step(v)? {
            out.push(x);
        }
        return Ok(out);
    }
    // A foreign (CPython) iterable drains via `iter_step` so its advance runs
    // with the host borrow released — a lazy stdlib iterator built over a
    // pythonrs callback (`list(itertools.starmap(pow, …))`) would otherwise
    // re-enter the host mid-borrow and panic.
    #[cfg(feature = "stdlib-ffi")]
    if with_host(|h| h.foreign_id(v)).is_some() {
        let it = with_host(|h| h.make_iter(v))?;
        let mut out = Vec::new();
        while let Some(x) = iter_step(&it)? {
            out.push(x);
        }
        return Ok(out);
    }
    // A user instance iterates via its `__iter__`/`__next__` (or `__getitem__`)
    // protocol — reached by `list()`/`tuple()`/`sum()`/… over custom iterables.
    if with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_)))) {
        return iter_instance_items(v);
    }
    with_host(|h| h.iter_items(v))
}

/// Materialize a user instance's iteration into a concrete vector: `__iter__`
/// then repeated `__next__` (draining a native iterator/generator if `__iter__`
/// returned one), else the old-style `__getitem__(0..)` sequence protocol.
pub fn iter_instance_items(v: &Value) -> Result<Vec<Value>, String> {
    // A builtin-type subclass without an `__iter__` override materializes its
    // native payload (`sorted(S([...]))`, `list(Stack(...))`).
    if let Some(payload) = subclass_payload(v, "__iter__") {
        return iter_vec(&payload);
    }
    let (has_iter, has_getitem) = with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => (
            h.class_lookup(&i.class, "__iter__").is_some(),
            h.class_lookup(&i.class, "__getitem__").is_some(),
        ),
        _ => (false, false),
    });
    if has_iter {
        let it = call_method(v, "__iter__", vec![], vec![])?;
        if with_host(|h| {
            matches!(
                h.get(&it),
                Some(PyObj::Iter(_)) | Some(PyObj::Generator { .. })
            )
        }) {
            return iter_vec(&it);
        }
        let mut items = Vec::new();
        loop {
            match call_method(&it, "__next__", vec![], vec![]) {
                Ok(x) => items.push(x),
                Err(e) if e.contains("StopIteration") => break,
                Err(e) => return Err(e),
            }
            if items.len() > 10_000_000 {
                break;
            }
        }
        Ok(items)
    } else if has_getitem {
        let mut items = Vec::new();
        let mut i: i64 = 0;
        loop {
            match call_method(v, "__getitem__", vec![Value::Int(i)], vec![]) {
                Ok(x) => items.push(x),
                Err(e) if e.contains("IndexError") || e.contains("StopIteration") => break,
                Err(e) => return Err(e),
            }
            i += 1;
            if items.len() > 10_000_000 {
                break;
            }
        }
        Ok(items)
    } else {
        Err(type_error(&format!(
            "'{}' object is not iterable",
            with_host(|h| h.type_name(v))
        )))
    }
}

/// Advance any iterator — including a generator or a lazy composite iterator
/// (`zip`/`map`/`filter`/`enumerate`) — by one step. Composite iterators pull
/// from their sources with the host borrow released, so an infinite source
/// (e.g. `itertools.count()`) never materializes.
pub fn iter_step(it: &Value) -> Result<Option<Value>, String> {
    match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::Generator { .. }) => gen_resume(it, Value::Undef),
        Some(PyObj::Zip { .. }) => zip_step(it),
        Some(PyObj::MapObj { .. }) => map_step(it),
        Some(PyObj::FilterObj { .. }) => filter_step(it),
        Some(PyObj::EnumerateObj { .. }) => enumerate_step(it),
        Some(PyObj::CallIter { .. }) => calliter_step(it),
        // A foreign (CPython) iterator advances with the host borrow released so
        // a lazy stdlib iterator running a pythonrs callback can re-enter.
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::iter_next_cb(id),
        _ => with_host(|h| h.iter_next(it)),
    }
}

/// One step of the two-argument `iter(callable, sentinel)`: call `func()` and
/// yield the result unless it equals `sentinel` (by `==`), which exhausts the
/// iterator. A CPython `callable_iterator` latches on the sentinel and stays
/// exhausted thereafter.
fn calliter_step(it: &Value) -> Result<Option<Value>, String> {
    let (func, sentinel, done) = match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::CallIter {
            func,
            sentinel,
            done,
        }) => (func, sentinel, done),
        _ => return Err(type_error("not an iterator")),
    };
    if done {
        return Ok(None);
    }
    let v = invoke(&func, vec![], vec![])?;
    if with_host(|h| h.equal(&v, &sentinel)) {
        with_host(|h| {
            if let Some(PyObj::CallIter { done, .. }) = h.get_mut(it) {
                *done = true;
            }
        });
        return Ok(None);
    }
    Ok(Some(v))
}

/// One step of a lazy `zip`: pull one item from each source iterator in order.
fn zip_step(it: &Value) -> Result<Option<Value>, String> {
    let (sources, strict, done) = match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::Zip {
            sources,
            strict,
            done,
        }) => (sources, strict, done),
        _ => return Err(type_error("not an iterator")),
    };
    if done {
        return Ok(None);
    }
    // `zip()` with no iterables is an immediately-exhausted iterator (CPython
    // yields nothing); without this guard the empty-tuple round would repeat
    // forever since no source can signal exhaustion.
    if sources.is_empty() {
        set_zip_done(it);
        return Ok(None);
    }
    let mut out: Vec<Value> = Vec::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        match iter_step(s)? {
            Some(v) => out.push(v),
            None => {
                set_zip_done(it);
                if strict {
                    // A real length mismatch raises; sources exhausting together
                    // (source 0 ends and no later source still yields) is a clean
                    // stop, not an error.
                    if let Some(e) = zip_strict_error(&sources, i) {
                        return Err(e);
                    }
                }
                return Ok(None);
            }
        }
    }
    Ok(Some(with_host(|h| h.new_tuple(out))))
}

fn set_zip_done(it: &Value) {
    with_host(|h| {
        if let Some(PyObj::Zip { done, .. }) = h.get_mut(it) {
            *done = true;
        }
    });
}

/// Build CPython's `zip(strict=True)` length-mismatch message. `i` is the index
/// (0-based) of the source that just exhausted mid-round. Returns `None` when
/// there is no mismatch (source 0 ended and every later source is also exhausted)
/// — that is a normal end-of-iteration, not an error.
fn zip_strict_error(sources: &[Value], i: usize) -> Option<String> {
    if i > 0 {
        // Sources 0..i were longer than source i.
        let than = if i == 1 {
            "argument 1".to_string()
        } else {
            format!("arguments 1-{i}")
        };
        return Some(format!(
            "ValueError: zip() argument {} is shorter than {than}",
            i + 1
        ));
    }
    // Source 0 exhausted first: find the first later source that still yields.
    for (j, s) in sources.iter().enumerate().skip(1) {
        if let Ok(Some(_)) = iter_step(s) {
            let than = if j == 1 {
                "argument 1".to_string()
            } else {
                format!("arguments 1-{j}")
            };
            return Some(format!(
                "ValueError: zip() argument {} is longer than {than}",
                j + 1
            ));
        }
    }
    // All sources exhausted together: clean stop, no error.
    None
}

/// One step of a lazy `map`: pull one item from each source, then apply `func`.
fn map_step(it: &Value) -> Result<Option<Value>, String> {
    let (func, sources, done) = match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::MapObj {
            func,
            sources,
            done,
        }) => (func, sources, done),
        _ => return Err(type_error("not an iterator")),
    };
    if done {
        return Ok(None);
    }
    let mut call_args: Vec<Value> = Vec::with_capacity(sources.len());
    for s in &sources {
        match iter_step(s)? {
            Some(v) => call_args.push(v),
            None => {
                with_host(|h| {
                    if let Some(PyObj::MapObj { done, .. }) = h.get_mut(it) {
                        *done = true;
                    }
                });
                return Ok(None);
            }
        }
    }
    Ok(Some(invoke(&func, call_args, vec![])?))
}

/// One step of a lazy `filter`: pull items until one satisfies the predicate.
fn filter_step(it: &Value) -> Result<Option<Value>, String> {
    let (func, source, done) = match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::FilterObj { func, source, done }) => (func, source, done),
        _ => return Err(type_error("not an iterator")),
    };
    if done {
        return Ok(None);
    }
    loop {
        match iter_step(&source)? {
            Some(v) => {
                let keep = if matches!(func, Value::Undef) {
                    with_host(|h| h.truthy(&v))
                } else {
                    let r = invoke(&func, vec![v.clone()], vec![])?;
                    with_host(|h| h.truthy(&r))
                };
                if keep {
                    return Ok(Some(v));
                }
            }
            None => {
                with_host(|h| {
                    if let Some(PyObj::FilterObj { done, .. }) = h.get_mut(it) {
                        *done = true;
                    }
                });
                return Ok(None);
            }
        }
    }
}

/// One step of a lazy `enumerate`: pull one item and pair it with the index.
fn enumerate_step(it: &Value) -> Result<Option<Value>, String> {
    let (source, idx, done) = match with_host(|h| h.get(it).cloned()) {
        Some(PyObj::EnumerateObj { source, next, done }) => (source, next, done),
        _ => return Err(type_error("not an iterator")),
    };
    if done {
        return Ok(None);
    }
    match iter_step(&source)? {
        Some(v) => {
            with_host(|h| {
                if let Some(PyObj::EnumerateObj { next, .. }) = h.get_mut(it) {
                    *next = idx + 1;
                }
            });
            Ok(Some(with_host(|h| h.new_tuple(vec![Value::Int(idx), v]))))
        }
        None => {
            with_host(|h| {
                if let Some(PyObj::EnumerateObj { done, .. }) = h.get_mut(it) {
                    *done = true;
                }
            });
            Ok(None)
        }
    }
}

/// Import a module by name. A small built-in set is supported; unknown modules
/// raise `ModuleNotFoundError`.
pub fn import_module(name: &str) -> Result<Value, String> {
    // Native stdlib modules under src/stdlib. Their `entries` return owned-String
    // keys (vs the `&str` keys of the inline arms below), so build the namespace
    // here and return before the `&str` match. These are pure-Python subsets
    // (e.g. the native `textwrap` covers only `width`, not the full keyword-
    // option surface), so with the FFI bridge on they are skipped in favor of
    // the real CPython modules; they serve only the `--no-default-features` build.
    #[cfg(not(feature = "stdlib-ffi"))]
    let stdlib_entries: Option<Vec<(String, Value)>> = match name {
        "textwrap" => Some(with_host(crate::stdlib::textwrap::entries)),
        "statistics" => Some(with_host(crate::stdlib::statistics::entries)),
        _ => None,
    };
    #[cfg(feature = "stdlib-ffi")]
    let stdlib_entries: Option<Vec<(String, Value)>> = None;
    if let Some(entries) = stdlib_entries {
        return Ok(with_host(|h| {
            let mut ns = IndexMap::new();
            for (k, v) in entries {
                ns.insert(k, v);
            }
            h.alloc(PyObj::Module {
                name: name.to_string(),
                ns,
            })
        }));
    }

    let entries: Vec<(&str, Value)> = match name {
        "math" => with_host(|h| {
            vec![
                ("pi", Value::Float(std::f64::consts::PI)),
                ("e", Value::Float(std::f64::consts::E)),
                ("tau", Value::Float(std::f64::consts::TAU)),
                ("inf", Value::Float(f64::INFINITY)),
                ("nan", Value::Float(f64::NAN)),
                ("sqrt", h.alloc(PyObj::Builtin("math.sqrt".into()))),
                ("floor", h.alloc(PyObj::Builtin("math.floor".into()))),
                ("ceil", h.alloc(PyObj::Builtin("math.ceil".into()))),
                ("fabs", h.alloc(PyObj::Builtin("math.fabs".into()))),
                ("pow", h.alloc(PyObj::Builtin("math.pow".into()))),
                ("log", h.alloc(PyObj::Builtin("math.log".into()))),
                ("sin", h.alloc(PyObj::Builtin("math.sin".into()))),
                ("cos", h.alloc(PyObj::Builtin("math.cos".into()))),
                ("gcd", h.alloc(PyObj::Builtin("math.gcd".into()))),
                (
                    "factorial",
                    h.alloc(PyObj::Builtin("math.factorial".into())),
                ),
            ]
        }),
        "sys" => with_host(|h| {
            // `sys.argv` mirrors the process arguments installed by `init_runtime`.
            let argv_strs = h.argv.clone();
            let argv_items: Vec<Value> = argv_strs.into_iter().map(|s| h.new_str(s)).collect();
            let argv = h.new_list(argv_items);
            // Standard streams are `File` handles over the fixed side-table slots.
            let stdout = h.alloc(PyObj::File { id: 0 });
            let stderr = h.alloc(PyObj::File { id: 1 });
            let stdin = h.alloc(PyObj::File { id: 2 });
            // `sys.version_info` — a `(major, minor, micro, releaselevel, serial)`
            // namedtuple matching the emulated CPython.
            let vi_vals = vec![
                Value::Int(PY_MAJOR),
                Value::Int(PY_MINOR),
                Value::Int(PY_MICRO),
                h.new_str("final"),
                Value::Int(0),
            ];
            let version_info = h.new_tuple(vi_vals);
            if let Value::Obj(i) = version_info {
                h.nt_meta.insert(
                    i,
                    NtMeta {
                        type_name: "sys.version_info".to_string(),
                        fields: ["major", "minor", "micro", "releaselevel", "serial"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    },
                );
            }
            // `sys.path`: the script directory (or "" for `-c`/stdin) first, as a
            // list — the shape scripts rely on, not CPython's full search path.
            let path0 = match &h.main_file {
                Some(f) => std::path::Path::new(f)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                None => String::new(),
            };
            let path0 = h.new_str(path0);
            let path = h.new_list(vec![path0]);
            let executable = h.new_str(
                std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            let modules = h.new_dict(IndexMap::new());
            let version = h.new_str(format!("{PY_MAJOR}.{PY_MINOR}.{PY_MICRO} (pythonrs)"));
            let platform = h.new_str(py_platform());
            vec![
                ("argv", argv),
                ("maxsize", Value::Int(i64::MAX)),
                ("version", version),
                ("version_info", version_info),
                ("platform", platform),
                ("path", path),
                ("modules", modules),
                ("executable", executable),
                ("stdout", stdout),
                ("stderr", stderr),
                ("stdin", stdin),
                ("exit", h.alloc(PyObj::Builtin("sys.exit".into()))),
                (
                    "getrecursionlimit",
                    h.alloc(PyObj::Builtin("sys.getrecursionlimit".into())),
                ),
                (
                    "setrecursionlimit",
                    h.alloc(PyObj::Builtin("sys.setrecursionlimit".into())),
                ),
            ]
        }),
        "asyncio" => with_host(|h| {
            vec![
                ("run", h.alloc(PyObj::Builtin("asyncio.run".into()))),
                ("sleep", h.alloc(PyObj::Builtin("asyncio.sleep".into()))),
                ("gather", h.alloc(PyObj::Builtin("asyncio.gather".into()))),
                (
                    "create_task",
                    h.alloc(PyObj::Builtin("asyncio.create_task".into())),
                ),
                (
                    "ensure_future",
                    h.alloc(PyObj::Builtin("asyncio.ensure_future".into())),
                ),
                (
                    "wait_for",
                    h.alloc(PyObj::Builtin("asyncio.wait_for".into())),
                ),
                ("wait", h.alloc(PyObj::Builtin("asyncio.wait".into()))),
                (
                    "as_completed",
                    h.alloc(PyObj::Builtin("asyncio.as_completed".into())),
                ),
                ("Event", h.alloc(PyObj::Builtin("asyncio.Event".into()))),
                ("Lock", h.alloc(PyObj::Builtin("asyncio.Lock".into()))),
                ("Queue", h.alloc(PyObj::Builtin("asyncio.Queue".into()))),
                (
                    "get_event_loop",
                    h.alloc(PyObj::Builtin("asyncio.get_event_loop".into())),
                ),
                (
                    "get_running_loop",
                    h.alloc(PyObj::Builtin("asyncio.get_running_loop".into())),
                ),
                (
                    "new_event_loop",
                    h.alloc(PyObj::Builtin("asyncio.new_event_loop".into())),
                ),
                ("Future", h.alloc(PyObj::Builtin("asyncio.Future".into()))),
                (
                    "TimeoutError",
                    h.alloc(PyObj::Builtin("TimeoutError".into())),
                ),
                (
                    "CancelledError",
                    h.alloc(PyObj::Builtin("CancelledError".into())),
                ),
                (
                    "InvalidStateError",
                    h.alloc(PyObj::Builtin("InvalidStateError".into())),
                ),
                ("QueueEmpty", h.alloc(PyObj::Builtin("QueueEmpty".into()))),
                ("QueueFull", h.alloc(PyObj::Builtin("QueueFull".into()))),
                ("FIRST_COMPLETED", h.new_str("FIRST_COMPLETED".to_string())),
                ("FIRST_EXCEPTION", h.new_str("FIRST_EXCEPTION".to_string())),
                ("ALL_COMPLETED", h.new_str("ALL_COMPLETED".to_string())),
            ]
        }),
        "collections" => with_host(|h| {
            vec![
                ("deque", h.alloc(PyObj::Builtin("collections.deque".into()))),
                (
                    "Counter",
                    h.alloc(PyObj::Builtin("collections.Counter".into())),
                ),
                (
                    "defaultdict",
                    h.alloc(PyObj::Builtin("collections.defaultdict".into())),
                ),
                (
                    "OrderedDict",
                    h.alloc(PyObj::Builtin("collections.OrderedDict".into())),
                ),
                (
                    "namedtuple",
                    h.alloc(PyObj::Builtin("collections.namedtuple".into())),
                ),
            ]
        }),
        _ => {
            // With the CPython stdlib bridge on, a module pythonrs doesn't provide
            // natively is imported from the real CPython stdlib (pure `.py` + C
            // accelerators) and returned as a `Foreign` module handle. `from x
            // import y`, submodules (`os.path`), and `sys.modules` all fall out of
            // CPython's own importer.
            #[cfg(feature = "stdlib-ffi")]
            {
                let id = crate::ffi::import(name)?;
                return Ok(with_host(|h| h.alloc(PyObj::Foreign(id))));
            }
            #[cfg(not(feature = "stdlib-ffi"))]
            return Err(format!("ModuleNotFoundError: No module named '{name}'"));
        }
    };
    Ok(with_host(|h| {
        let mut ns = IndexMap::new();
        for (k, v) in entries {
            ns.insert(k.to_string(), v);
        }
        h.alloc(PyObj::Module {
            name: name.to_string(),
            ns,
        })
    }))
}

// ── file / I/O side table (ported from rubylang's `IoCell`) ──────────────────

fn io_err(e: std::io::Error) -> String {
    format!("OSError: {e}")
}
fn closed_err() -> String {
    "ValueError: I/O operation on closed file.".into()
}
fn unsupported_read() -> String {
    "io.UnsupportedOperation: not readable".into()
}
fn unsupported_write() -> String {
    "io.UnsupportedOperation: not writable".into()
}

impl PyHost {
    /// Register an owned `std::fs::File` and hand back a fresh `File` handle.
    pub fn io_alloc_file(
        &mut self,
        file: std::fs::File,
        path: String,
        readable: bool,
        writable: bool,
    ) -> Value {
        let id = self.io_handles.len() as u32;
        self.io_handles.push(IoCell::File {
            file: Some(file),
            path,
            readable,
            writable,
        });
        self.alloc(PyObj::File { id })
    }

    /// Whether the file behind `id` is closed (standard streams never close).
    pub fn io_closed(&self, id: u32) -> bool {
        matches!(
            self.io_handles.get(id as usize),
            Some(IoCell::File { file: None, .. })
        )
    }

    /// The `repr` of a file handle.
    fn file_repr(&self, id: u32) -> String {
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) => {
                "<_io.TextIOWrapper name='<stdout>' mode='w' encoding='utf-8'>".into()
            }
            Some(IoCell::Stderr) => {
                "<_io.TextIOWrapper name='<stderr>' mode='w' encoding='utf-8'>".into()
            }
            Some(IoCell::Stdin) => {
                "<_io.TextIOWrapper name='<stdin>' mode='r' encoding='utf-8'>".into()
            }
            Some(IoCell::File {
                file,
                path,
                readable,
                writable,
            }) => {
                let mode = match (readable, writable) {
                    (true, true) => "r+",
                    (false, true) => "w",
                    _ => "r",
                };
                let closed = if file.is_none() { " (closed)" } else { "" };
                format!("<_io.TextIOWrapper name='{path}' mode='{mode}' encoding='utf-8'{closed}>")
            }
            None => "<_io.TextIOWrapper>".into(),
        }
    }

    /// `f.write(s)` for text — returns the number of characters written.
    /// The side-table id of a `File`/stream object, if `v` is one.
    pub fn file_id(&self, v: &Value) -> Option<u32> {
        match self.get(v) {
            Some(PyObj::File { id }) => Some(*id),
            _ => None,
        }
    }

    pub fn io_write(&mut self, id: u32, s: &str) -> Result<Value, String> {
        self.io_write_bytes(id, s.as_bytes())?;
        Ok(Value::Int(s.chars().count() as i64))
    }

    /// `f.write(...)` at the byte layer — returns the number of bytes written.
    pub fn io_write_bytes(&mut self, id: u32, bytes: &[u8]) -> Result<Value, String> {
        use std::io::Write;
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::Stdout) => {
                let mut o = std::io::stdout();
                o.write_all(bytes).and_then(|_| o.flush()).map_err(io_err)?;
            }
            Some(IoCell::Stderr) => {
                let mut o = std::io::stderr();
                o.write_all(bytes).and_then(|_| o.flush()).map_err(io_err)?;
            }
            Some(IoCell::Stdin) => return Err(unsupported_write()),
            Some(IoCell::File {
                file: Some(f),
                writable: true,
                ..
            }) => {
                // Flush immediately: the handle is buffered, and a `with` block's
                // `__exit__` does not yet close files, so an unflushed write would
                // be invisible to a read-after-write in the same process.
                f.write_all(bytes).and_then(|_| f.flush()).map_err(io_err)?;
            }
            Some(IoCell::File { file: Some(_), .. }) => return Err(unsupported_write()),
            Some(IoCell::File { file: None, .. }) => return Err(closed_err()),
            None => return Err(closed_err()),
        }
        Ok(Value::Int(bytes.len() as i64))
    }

    /// `f.read()` — the remaining contents as a string.
    pub fn io_read_all(&mut self, id: u32) -> Result<String, String> {
        use std::io::Read;
        let mut s = String::new();
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File {
                file: Some(f),
                readable: true,
                ..
            }) => {
                f.read_to_string(&mut s).map_err(io_err)?;
                Ok(s)
            }
            Some(IoCell::File { file: Some(_), .. }) => Err(unsupported_read()),
            Some(IoCell::File { file: None, .. }) => Err(closed_err()),
            Some(IoCell::Stdin) => {
                std::io::stdin().read_to_string(&mut s).map_err(io_err)?;
                Ok(s)
            }
            _ => Err(unsupported_read()),
        }
    }

    /// `f.readline()` — one line up to and including `\n` (or EOF); "" at EOF.
    pub fn io_readline(&mut self, id: u32) -> Result<String, String> {
        use std::io::Read;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let mut one = [0u8; 1];
            let n = match self.io_handles.get_mut(id as usize) {
                Some(IoCell::File {
                    file: Some(f),
                    readable: true,
                    ..
                }) => f.read(&mut one),
                Some(IoCell::File { file: Some(_), .. }) => return Err(unsupported_read()),
                Some(IoCell::File { file: None, .. }) => return Err(closed_err()),
                Some(IoCell::Stdin) => std::io::stdin().read(&mut one),
                _ => return Err(unsupported_read()),
            };
            match n {
                Ok(0) => break,
                Ok(_) => {
                    buf.push(one[0]);
                    if one[0] == b'\n' {
                        break;
                    }
                }
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// `f.readlines()` / iteration — the remaining lines, each keeping its `\n`.
    pub fn io_read_lines(&mut self, id: u32) -> Result<Vec<String>, String> {
        let all = self.io_read_all(id)?;
        Ok(all.split_inclusive('\n').map(|l| l.to_string()).collect())
    }

    /// `f.close()` — drop the file (idempotent; no-op for standard streams).
    pub fn io_close(&mut self, id: u32) {
        if let Some(IoCell::File { file, .. }) = self.io_handles.get_mut(id as usize) {
            *file = None;
        }
    }

    /// `f.flush()`.
    pub fn io_flush(&mut self, id: u32) -> Result<(), String> {
        use std::io::Write;
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::Stdout) => std::io::stdout().flush().map_err(io_err),
            Some(IoCell::Stderr) => std::io::stderr().flush().map_err(io_err),
            Some(IoCell::File { file: Some(f), .. }) => f.flush().map_err(io_err),
            _ => Ok(()),
        }
    }

    // ── lru_cache memo tables ────────────────────────────────────────────────
    fn lru_new(&mut self, maxsize: Option<usize>) -> u32 {
        let id = self.lru_caches.len() as u32;
        self.lru_caches.push(LruData {
            map: IndexMap::new(),
            order: VecDeque::new(),
            maxsize,
            hits: 0,
            misses: 0,
        });
        id
    }

    /// Look up `key`; on a hit, mark it most-recently-used and bump `hits`, else
    /// bump `misses`.
    fn lru_lookup(&mut self, cache_id: u32, key: &PKey) -> Option<Value> {
        let c = self.lru_caches.get_mut(cache_id as usize)?;
        if let Some(v) = c.map.get(key).cloned() {
            c.hits += 1;
            if let Some(pos) = c.order.iter().position(|k| k == key) {
                if let Some(k) = c.order.remove(pos) {
                    c.order.push_back(k);
                }
            }
            Some(v)
        } else {
            c.misses += 1;
            None
        }
    }

    /// Store `key -> val`, evicting the least-recently-used entry past `maxsize`.
    fn lru_store(&mut self, cache_id: u32, key: PKey, val: Value) {
        if let Some(c) = self.lru_caches.get_mut(cache_id as usize) {
            if c.map.insert(key.clone(), val).is_none() {
                c.order.push_back(key);
            }
            if let Some(max) = c.maxsize {
                while c.map.len() > max {
                    match c.order.pop_front() {
                        Some(old) => {
                            c.map.shift_remove(&old);
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// `(hits, misses, maxsize, currsize)` for `cache_info()`.
    fn lru_info(&self, cache_id: u32) -> (u64, u64, Option<usize>, usize) {
        match self.lru_caches.get(cache_id as usize) {
            Some(c) => (c.hits, c.misses, c.maxsize, c.map.len()),
            None => (0, 0, None, 0),
        }
    }

    /// `cache_clear()` — empty the memo and reset counters.
    fn lru_clear(&mut self, cache_id: u32) {
        if let Some(c) = self.lru_caches.get_mut(cache_id as usize) {
            c.map.clear();
            c.order.clear();
            c.hits = 0;
            c.misses = 0;
        }
    }
}

/// `open(path, mode)` — open a file and return a `File` handle value. The text
/// modes `r`/`w`/`a`/`x` and their `+` / `b` / `t` variants are supported; bytes
/// vs text is handled at the read/write layer, not here.
pub fn open_file(path: &str, mode: &str) -> Result<Value, String> {
    use std::fs::OpenOptions;
    let m: String = mode.chars().filter(|c| *c != 'b' && *c != 't').collect();
    let base = m.chars().next().unwrap_or('r');
    let plus = m.contains('+');
    let mut opts = OpenOptions::new();
    let (readable, writable) = match base {
        'r' => {
            opts.read(true);
            if plus {
                opts.write(true);
            }
            (true, plus)
        }
        'w' => {
            opts.write(true).create(true).truncate(true);
            if plus {
                opts.read(true);
            }
            (plus, true)
        }
        'a' => {
            opts.append(true).create(true);
            if plus {
                opts.read(true);
            }
            (plus, true)
        }
        'x' => {
            opts.write(true).create_new(true);
            if plus {
                opts.read(true);
            }
            (plus, true)
        }
        _ => return Err(format!("ValueError: invalid mode: '{mode}'")),
    };
    let f = opts.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("FileNotFoundError: [Errno 2] No such file or directory: '{path}'")
        }
        std::io::ErrorKind::AlreadyExists => {
            format!("FileExistsError: [Errno 17] File exists: '{path}'")
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("PermissionError: [Errno 13] Permission denied: '{path}'")
        }
        _ => format!("OSError: {e}: '{path}'"),
    })?;
    Ok(with_host(|h| {
        h.io_alloc_file(f, path.to_string(), readable, writable)
    }))
}

// ── collections constructors ─────────────────────────────────────────────────

/// Allocate a `collections.deque`.
pub fn alloc_deque(items: VecDeque<Value>, maxlen: Option<usize>) -> Value {
    with_host(|h| h.alloc(PyObj::Deque { items, maxlen }))
}

/// Allocate a tagged `dict` subclass (Counter / defaultdict / OrderedDict).
pub fn alloc_dict_subtype(
    pairs: IndexMap<PKey, (Value, Value)>,
    kind: DictKind,
    factory: Option<Value>,
) -> Value {
    with_host(|h| {
        let d = h.alloc(PyObj::Dict(pairs));
        if let Value::Obj(i) = d {
            h.dict_meta.insert(i, DictMeta { kind, factory });
        }
        d
    })
}

/// The `dict_meta` for a value, if it is a tagged `dict` subclass.
pub fn dict_meta_of(v: &Value) -> Option<DictMeta> {
    with_host(|h| match v {
        Value::Obj(i) => h.dict_meta.get(i).cloned(),
        _ => None,
    })
}

/// Build a `namedtuple` type object (`namedtuple(name, field_names)`).
pub fn make_namedtuple_type(name: &str, fields: Vec<String>) -> Value {
    with_host(|h| {
        h.alloc(PyObj::NamedTupleType {
            type_name: name.to_string(),
            fields,
        })
    })
}

/// Construct a `namedtuple` instance: a `PyObj::Tuple` tagged in `nt_meta`.
pub fn namedtuple_construct(
    type_name: &str,
    fields: &[String],
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    if args.len() > fields.len() {
        return Err(type_error(&format!(
            "{type_name}() takes {} positional arguments but {} were given",
            fields.len(),
            args.len()
        )));
    }
    let mut values: Vec<Option<Value>> = vec![None; fields.len()];
    for (i, a) in args.into_iter().enumerate() {
        values[i] = Some(a);
    }
    for (k, v) in kwargs {
        match fields.iter().position(|f| *f == k) {
            Some(i) => {
                if values[i].is_some() {
                    return Err(type_error(&format!(
                        "{type_name}() got multiple values for argument '{k}'"
                    )));
                }
                values[i] = Some(v);
            }
            None => {
                return Err(type_error(&format!(
                    "{type_name}() got an unexpected keyword argument '{k}'"
                )))
            }
        }
    }
    let mut items = Vec::with_capacity(fields.len());
    for (i, slot) in values.into_iter().enumerate() {
        match slot {
            Some(v) => items.push(v),
            None => {
                return Err(type_error(&format!(
                    "{type_name}() missing required argument: '{}'",
                    fields[i]
                )))
            }
        }
    }
    Ok(with_host(|h| {
        let tup = h.alloc(PyObj::Tuple(items));
        if let Value::Obj(idx) = tup {
            h.nt_meta.insert(
                idx,
                NtMeta {
                    type_name: type_name.to_string(),
                    fields: fields.to_vec(),
                },
            );
        }
        tup
    }))
}

// ── functools partial / lru_cache ────────────────────────────────────────────

/// Allocate a `functools.partial`.
pub fn make_partial(func: Value, args: Vec<Value>, kwargs: Vec<(String, Value)>) -> Value {
    with_host(|h| h.alloc(PyObj::Partial { func, args, kwargs }))
}

/// Allocate a `functools.lru_cache`-wrapped callable over `func`.
pub fn make_lru_cache(func: Value, maxsize: Option<usize>) -> Value {
    with_host(|h| {
        let cache_id = h.lru_new(maxsize);
        h.alloc(PyObj::LruCache { func, cache_id })
    })
}

/// `wrapped.cache_info()` — `(hits, misses, maxsize, currsize)` for the cache
/// behind an `LruCache` value. Returns `None` if `v` is not one.
pub fn lru_cache_info(v: &Value) -> Option<(u64, u64, Option<usize>, usize)> {
    let id = match with_host(|h| h.get(v).cloned()) {
        Some(PyObj::LruCache { cache_id, .. }) => cache_id,
        _ => return None,
    };
    Some(with_host(|h| h.lru_info(id)))
}

/// `wrapped.cache_clear()` for an `LruCache` value; `false` if `v` is not one.
pub fn lru_cache_clear(v: &Value) -> bool {
    match with_host(|h| h.get(v).cloned()) {
        Some(PyObj::LruCache { cache_id, .. }) => {
            with_host(|h| h.lru_clear(cache_id));
            true
        }
        _ => false,
    }
}

/// Call an lru-cached function: hash the positional args into a key, consult the
/// memo, compute + store on a miss. Only positional-arg calls with hashable args
/// are cached; any keyword arg or an unhashable arg bypasses the cache (matching
/// that such calls can't form a stable key).
fn lru_invoke(
    func: &Value,
    cache_id: u32,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let key = with_host(|h| {
        args.iter()
            .map(|a| h.to_key(a))
            .collect::<Result<Vec<PKey>, String>>()
            .map(PKey::Tuple)
    });
    let key = match (key, kwargs.is_empty()) {
        (Ok(k), true) => k,
        _ => return invoke(func, args, kwargs),
    };
    if let Some(v) = with_host(|h| h.lru_lookup(cache_id, &key)) {
        return Ok(v);
    }
    let result = invoke(func, args, kwargs)?;
    with_host(|h| h.lru_store(cache_id, key, result.clone()));
    Ok(result)
}

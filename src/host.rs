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

use fusevm::{Chunk, NumOp, VMResult, Value, VM};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
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

// ── heap objects ───────────────────────────────────────────────────────────

/// A key usable in a dict/set: Python hashes by value for the immutable types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    Tuple(Vec<PKey>),
}

/// A compiled function template: parameter shape + body chunk. Shared by every
/// closure created from the same `def`/`lambda`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FuncDef {
    pub name: String,
    /// Positional-or-keyword parameter names, in order.
    pub params: Vec<String>,
    /// How many trailing `params` have defaults.
    pub ndefaults: usize,
    pub star: Option<String>,
    pub kwonly: Vec<String>,
    /// Which kwonly params are required (no default).
    pub kwonly_required: Vec<bool>,
    pub kwargs: Option<String>,
    pub chunk: Chunk,
    /// True if the body contains a `yield` (a generator function).
    pub is_generator: bool,
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
    pub bases: Vec<String>,
    /// The class namespace populated by running the class body.
    pub ns: IndexMap<String, Value>,
    /// The C3-ish MRO (this class first), by name.
    pub mro: Vec<String>,
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

/// A user-defined class instance.
#[derive(Clone)]
pub struct Instance {
    pub class: String,
    pub attrs: IndexMap<String, Value>,
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
    /// A mutable byte string (`bytearray`). Held inline (a plain `Vec<u8>`),
    /// unlike the immutable [`PyObj::Bytes`].
    Bytearray(Vec<u8>),
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
    /// A `property`: invoke `fget(inst)`, or raise if `fget` is unset.
    Property { fget: Value, inst: Value },
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
    Property {
        fset: Value,
        inst: Value,
        val: Value,
    },
    /// A user data descriptor: call `desc.__set__(inst, val)`.
    Descriptor {
        desc: Value,
        inst: Value,
        val: Value,
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
    pub self_obj: Option<Value>,
    pub owner: Option<String>,
    /// Source line currently executing in this frame — updated by the DAP debug
    /// line hook (`--dap`); 0 outside debug mode.
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
}

/// One suspended generator. `coro` is `None` only while this generator is
/// actively running (taken out across `Coroutine::resume`); `ctx` holds its
/// volatile execution context (frames/signal/error/exc) while suspended.
struct GenCell {
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

/// Run `f` with mutable access to the thread-local host.
pub fn with_host<R>(f: impl FnOnce(&mut PyHost) -> R) -> R {
    HOST.with(|h| f(&mut h.borrow_mut()))
}

/// Reset the host to a clean slate (fresh module frame).
pub fn reset_host() {
    with_host(|h| *h = PyHost::new());
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
                self_obj: None,
                owner: None,
                line: 0,
            }],
            error: None,
            exc: None,
            signal: None,
            generators: Vec::new(),
            io_handles: vec![IoCell::Stdout, IoCell::Stderr, IoCell::Stdin],
            dict_meta: HashMap::new(),
            nt_meta: HashMap::new(),
            lru_caches: Vec::new(),
            exc_links: HashMap::new(),
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
    pub fn new_set(&mut self, items: IndexMap<PKey, Value>) -> Value {
        self.alloc(PyObj::Set(items))
    }

    pub fn as_str(&self, v: &Value) -> Option<String> {
        match self.get(v) {
            Some(PyObj::Str(s)) => Some(s.clone()),
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
    /// The call stack as (frame name, line) pairs, innermost first — for the DAP
    /// `stackTrace`. `owner` carries the function/class name where known.
    pub fn dbg_stack(&self) -> Vec<(String, u32)> {
        self.frames
            .iter()
            .rev()
            .map(|f| {
                let name = f.owner.clone().unwrap_or_else(|| "<module>".to_string());
                (name, f.line)
            })
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
        if self.frames.len() == 1 {
            self.globals.insert(name.to_string(), val);
        } else {
            self.cur_env()
                .borrow_mut()
                .vars
                .insert(name.to_string(), val);
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
pub fn type_error(msg: &str) -> String {
    format!("TypeError: {msg}")
}

// ── the fusevm run plumbing ──────────────────────────────────────────────────

thread_local! {
    static DEBUG_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable DAP debug execution.
pub fn set_debug_mode(on: bool) {
    DEBUG_MODE.with(|d| d.set(on));
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
                Some(PyObj::Range { .. }) => "range".into(),
                Some(PyObj::Slice { .. }) => "slice".into(),
                Some(PyObj::Func(_)) => "function".into(),
                Some(PyObj::Builtin(_)) => "builtin_function_or_method".into(),
                Some(PyObj::Class(_)) => "type".into(),
                Some(PyObj::Instance(i)) => i.class.clone(),
                Some(PyObj::BoundMethod { .. }) => "method".into(),
                Some(PyObj::Exception { class, .. }) => class.clone(),
                Some(PyObj::Iter(_)) => "iterator".into(),
                Some(PyObj::Zip { .. }) => "zip".into(),
                Some(PyObj::MapObj { .. }) => "map".into(),
                Some(PyObj::FilterObj { .. }) => "filter".into(),
                Some(PyObj::EnumerateObj { .. }) => "enumerate".into(),
                Some(PyObj::Module { .. }) => "module".into(),
                Some(PyObj::BigInt(_)) => "int".into(),
                Some(PyObj::Complex(..)) => "complex".into(),
                Some(PyObj::Generator { .. }) => "generator".into(),
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
                Some(PyObj::Deque { items, .. }) => !items.is_empty(),
                Some(PyObj::List(l)) => !l.is_empty(),
                Some(PyObj::Tuple(l)) => !l.is_empty(),
                Some(PyObj::Dict(d)) => !d.is_empty(),
                Some(PyObj::Set(s)) => !s.is_empty(),
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
                Some(PyObj::Bytes(b)) => format!("b{}", quote_bytes(b)),
                Some(PyObj::Instance(inst)) => format!("<{} object>", inst.class),
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
                // A builtin *type* name (`int`, `list`, …) reprs as `<class 'X'>`;
                // a builtin *function* (`len`, `print`) as `<built-in function X>`.
                Some(PyObj::Builtin(n)) => {
                    if crate::builtins::is_builtin_type_name(n) {
                        format!("<class '{n}'>")
                    } else {
                        format!("<built-in function {n}>")
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
                Some(PyObj::Generator { id }) => format!("<generator object at 0x{id:012x}>"),
                Some(PyObj::Bytearray(b)) => format!("bytearray(b{})", quote_bytes(b)),
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
                | Some(PyObj::Set(_)) => self.repr_of(v),
                None => "<object>".into(),
            },
            _ => "<object>".into(),
        }
    }

    fn exc_str(&self, class: &str, args: &[Value]) -> String {
        if args.is_empty() {
            String::new()
        } else if args.len() == 1 {
            self.str_of(&args[0])
        } else {
            let inner: Vec<String> = args.iter().map(|a| self.repr_of(a)).collect();
            let _ = class;
            format!("({})", inner.join(", "))
        }
    }

    /// `repr(v)`.
    pub fn repr_of(&self, v: &Value) -> String {
        match v {
            Value::Str(s) => quote_str(s),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => quote_str(s),
                Some(PyObj::List(l)) => {
                    let inner: Vec<String> = l.iter().map(|x| self.repr_of(x)).collect();
                    format!("[{}]", inner.join(", "))
                }
                Some(PyObj::Tuple(l)) => {
                    // A namedtuple instance reprs as `Type(field=value, …)`.
                    let nt = match v {
                        Value::Obj(i) => self.nt_meta.get(i),
                        _ => None,
                    };
                    if let Some(m) = nt {
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
                    }
                }
                Some(PyObj::Dict(d)) => {
                    let body: Vec<String> = d
                        .values()
                        .map(|(k, val)| format!("{}: {}", self.repr_of(k), self.repr_of(val)))
                        .collect();
                    let dict_repr = format!("{{{}}}", body.join(", "));
                    let meta = match v {
                        Value::Obj(i) => self.dict_meta.get(i),
                        _ => None,
                    };
                    match meta.map(|m| (m.kind, m.factory.clone())) {
                        Some((DictKind::Counter, _)) => format!("Counter({dict_repr})"),
                        Some((DictKind::DefaultDict, factory)) => {
                            let f = factory
                                .map(|fv| self.repr_of(&fv))
                                .unwrap_or_else(|| "None".into());
                            format!("defaultdict({f}, {dict_repr})")
                        }
                        Some((DictKind::OrderedDict, _)) => {
                            let pairs: Vec<String> = d
                                .values()
                                .map(|(k, val)| {
                                    format!("({}, {})", self.repr_of(k), self.repr_of(val))
                                })
                                .collect();
                            format!("OrderedDict([{}])", pairs.join(", "))
                        }
                        None => dict_repr,
                    }
                }
                Some(PyObj::Set(s)) => {
                    if s.is_empty() {
                        "set()".into()
                    } else {
                        let inner: Vec<String> = s.values().map(|x| self.repr_of(x)).collect();
                        format!("{{{}}}", inner.join(", "))
                    }
                }
                Some(PyObj::Exception { class, args }) => {
                    let inner: Vec<String> = args.iter().map(|a| self.repr_of(a)).collect();
                    format!("{class}({})", inner.join(", "))
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
            _ => "object",
        }
    }

    /// Structural equality (`==`).
    pub fn equal(&self, a: &Value, b: &Value) -> bool {
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
                    (Some(PyObj::Set(x)), Some(PyObj::Set(y))) => {
                        x.len() == y.len() && x.keys().all(|k| y.contains_key(k))
                    }
                    (Some(PyObj::Deque { items: x, .. }), Some(PyObj::Deque { items: y, .. })) => {
                        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.equal(p, q))
                    }
                    // bytes/bytearray compare equal by content (`b'a' == bytearray(b'a')`).
                    (Some(PyObj::Bytes(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytes(x)), Some(PyObj::Bytearray(y)))
                    | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytes(y)))
                    | (Some(PyObj::Bytearray(x)), Some(PyObj::Bytearray(y))) => x == y,
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
    fn num_val(&self, v: &Value) -> Option<f64> {
        match v {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(*b as i64 as f64),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::BigInt(b)) => Some(bigint_to_f64(b)),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_int(&self, v: &Value) -> Option<i64> {
        match v {
            Value::Int(n) => Some(*n),
            Value::Bool(b) => Some(*b as i64),
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
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::from("'");
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push('\'');
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
                    _ => Err(self.optype_err("+", a, b)),
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
                // set difference
                if let (Some(PyObj::Set(x)), Some(PyObj::Set(y))) = (self.get(a), self.get(b)) {
                    let mut out = x.clone();
                    for k in y.keys() {
                        out.shift_remove(k);
                    }
                    return Ok(self.new_set(out));
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
            _ => Ok(None),
        }
    }

    /// Comparison ops for non-native operands (`<`, `>`, `<=`, `>=`).
    pub fn compare(&mut self, op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
        use std::cmp::Ordering;
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
                            let d = br * br + bi * bi;
                            if d == 0.0 {
                                return Err("ZeroDivisionError: complex division by zero".into());
                            }
                            Ok(self.alloc(PyObj::Complex(
                                (ar * br + ai * bi) / d,
                                (ai * br - ar * bi) / d,
                            )))
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
                        return Err("ZeroDivisionError: integer division or modulo by zero".into());
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
                        return Err("ZeroDivisionError: integer division or modulo by zero".into());
                    }
                    return Ok(self.norm_big(bigint_floordiv(&x, &y)));
                }
                match (af, bf) {
                    (Some(_), Some(0.0)) => {
                        Err("ZeroDivisionError: float floor division by zero".into())
                    }
                    (Some(x), Some(y)) => Ok(Value::Float((x / y).floor())),
                    _ => Err(self.optype_err("//", a, b)),
                }
            }
            binop::MOD => {
                // str % formatting
                if let Some(PyObj::Str(fmt)) = self.get(a) {
                    let fmt = fmt.clone();
                    return self.str_format_percent(&fmt, b);
                }
                // Python `%` takes the sign of the divisor (floored remainder).
                if let (Some(x), Some(y)) = (ai, bi) {
                    if y == 0 {
                        return Err("ZeroDivisionError: integer division or modulo by zero".into());
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
                        return Err("ZeroDivisionError: integer division or modulo by zero".into());
                    }
                    return Ok(self.norm_big(bigint_mod(&x, &y)));
                }
                match (af, bf) {
                    (Some(_), Some(0.0)) => Err("ZeroDivisionError: float modulo".into()),
                    (Some(x), Some(y)) => Ok(Value::Float(x - (x / y).floor() * y)),
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
                    // Integer base and float base word the message differently.
                    (Some(x), Some(y)) if x == 0.0 && y < 0.0 => {
                        if ai.is_some() && bi.is_some() {
                            Err("ZeroDivisionError: zero to a negative power".into())
                        } else {
                            Err(
                                "ZeroDivisionError: 0.0 cannot be raised to a negative power"
                                    .into(),
                            )
                        }
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
                // set operations
                if let (Some(PyObj::Set(x)), Some(PyObj::Set(y))) = (self.get(a), self.get(b)) {
                    let (x, y) = (x.clone(), y.clone());
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
                    return Ok(self.new_set(out));
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
    fn str_format_percent(&mut self, fmt: &str, args: &Value) -> Result<Value, String> {
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
                match self.get(args) {
                    Some(PyObj::Dict(d)) => d
                        .get(&k)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| format!("KeyError: '{key}'"))?,
                    _ => return Err("TypeError: format requires a mapping".into()),
                }
            } else {
                let v = arglist.get(ai).cloned().ok_or_else(|| {
                    "TypeError: not enough arguments for format string".to_string()
                })?;
                ai += 1;
                v
            };
            let core = self.format_conv(conv, &val, f_plus, f_space, f_hash, prec)?;
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
        plus: bool,
        space: bool,
        hash: bool,
        prec: Option<usize>,
    ) -> Result<String, String> {
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
                let mut s = match conv {
                    's' => self.str_of(val),
                    'r' => self.repr_of(val),
                    _ => ascii_of(&self.repr_of(val)),
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
                let prefix = if abs == num_bigint::BigInt::from(0) {
                    ""
                } else {
                    prefix
                };
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
fn ascii_of(s: &str) -> String {
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
                let n = l.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: index out of range".into());
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
                match d.get(&key) {
                    Some((_, v)) => Ok(v.clone()),
                    None => Err(format!("KeyError: {}", self.repr_of(idx))),
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
                let n = b.len() as i64;
                let i = self
                    .as_int(idx)
                    .ok_or_else(|| type_error("byte indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: index out of range".into());
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
            _ => Err(type_error(&format!(
                "'{}' object does not support item assignment",
                self.type_name(recv)
            ))),
        }
    }

    pub fn del_item(&mut self, recv: &Value, idx: &Value) -> Result<(), String> {
        match self.get(recv) {
            Some(PyObj::Dict(_)) => {
                let key = self.to_key(idx)?;
                if let Some(PyObj::Dict(d)) = self.get_mut(recv) {
                    if d.shift_remove(&key).is_none() {
                        return Err(format!("KeyError: {}", self.repr_of(idx)));
                    }
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
            _ => Err(type_error("object doesn't support item deletion")),
        }
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
            Some(PyObj::Set(s)) => Ok(s.values().cloned().collect()),
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
            Some(PyObj::Set(s)) => IterState::Seq {
                items: s.values().cloned().collect(),
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
            | Some(PyObj::EnumerateObj { .. }) => return Ok(v.clone()),
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
            Some(PyObj::Set(s)) => {
                let key = self.to_key(item)?;
                Ok(s.contains_key(&key))
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
        // namedtuple field access: a tagged tuple resolves `.field` to its index.
        if let Value::Obj(i) = recv {
            let field_idx = self
                .nt_meta
                .get(i)
                .and_then(|m| m.fields.iter().position(|f| f == name));
            if let Some(idx) = field_idx {
                if let Some(PyObj::Tuple(items)) = self.get(recv) {
                    if let Some(v) = items.get(idx) {
                        return Ok(v.clone());
                    }
                }
            }
        }
        match self.get(recv) {
            Some(PyObj::Instance(inst)) => {
                if let Some(v) = inst.attrs.get(name) {
                    return Ok(v.clone());
                }
                // Exception chaining links live in a side table, not the
                // instance dict (a user exception is a plain `Instance`). Only
                // exception instances expose these dunders.
                if (name == "__cause__" || name == "__context__" || name == "__suppress_context__")
                    && self.class_is_exception(&inst.class)
                {
                    return Ok(match name {
                        "__cause__" => self.exc_link(recv).0,
                        "__context__" => self.exc_link(recv).1,
                        _ => Value::Bool(!matches!(self.exc_link(recv).0, Value::Undef)),
                    });
                }
                let class = inst.class.clone();
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
                    let attrs = match self.get(recv) {
                        Some(PyObj::Instance(i)) => i.attrs.clone(),
                        _ => IndexMap::new(),
                    };
                    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                    for (k, val) in attrs {
                        let kv = self.new_str(k.clone());
                        d.insert(PKey::Str(k), (kv, val));
                    }
                    return Ok(self.new_dict(d));
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
                if name == "__name__" || name == "__qualname__" {
                    return Ok(self.new_str(cname));
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
                Err(format!(
                    "AttributeError: type object '{cname}' has no attribute '{name}'"
                ))
            }
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
            Some(PyObj::Builtin(n)) if name == "__name__" => {
                // `type(x).__name__` — the builtin/type object's name.
                let n = n.clone();
                Ok(self.new_str(n))
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
    fn class_has(&self, class: &str, name: &str) -> bool {
        self.class_lookup(class, name).is_some()
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
        let (class, in_instdict) = match self.get(recv) {
            Some(PyObj::Instance(i)) => (i.class.clone(), i.attrs.contains_key(name)),
            _ => return AttrGet::Plain,
        };
        let cls_attr = match self.class_lookup(&class, name) {
            Some(v) => v,
            None => return AttrGet::Plain,
        };
        // `property` — a data descriptor: overrides the instance dict.
        if let Some(PyObj::Property { fget, .. }) = self.get(&cls_attr) {
            return AttrGet::Property {
                fget: fget.clone(),
                inst: recv.clone(),
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
        match self.get_mut(recv) {
            Some(PyObj::Instance(inst)) => {
                inst.attrs.insert(name.to_string(), val);
                Ok(())
            }
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
        if let Some(PyObj::Instance(inst)) = self.get_mut(recv) {
            if inst.attrs.shift_remove(name).is_some() {
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
                bases,
                ns,
                mro,
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

/// Resolve a bare name and call it (`f(args)`, `print(args)`).
pub fn call_named(
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    if let Some(v) = with_host(|h| h.read_name(name)) {
        return invoke(&v, args, kwargs);
    }
    if with_host(|h| h.classes.contains_key(name)) {
        return instantiate(name, args, kwargs);
    }
    if crate::builtins::is_known_builtin(name) {
        return crate::builtins::call_builtin_function(name, args, kwargs);
    }
    Err(name_error(name))
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
            if let Some(v) = inst.attrs.get(name).cloned() {
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
            Err(format!(
                "AttributeError: type object '{cname}' has no attribute '{name}'"
            ))
        }
        Some(PyObj::Module { ns, name: mname }) => match ns.get(name).cloned() {
            Some(v) => invoke(&v, args, kwargs),
            None => Err(format!(
                "AttributeError: module '{mname}' has no attribute '{name}'"
            )),
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
                Some(PyObj::Class(cname)) => Ok(with_host(|h| {
                    h.alloc(PyObj::Instance(Instance {
                        class: cname,
                        attrs: IndexMap::new(),
                    }))
                })),
                _ => Err(type_error("object.__new__(X): X is not a type object")),
            }
        }
        // `foreign.method(...)` (stdlib-ffi) — dispatch on the CPython side.
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::call_method(id, name, args, kwargs),
        _ => crate::builtins::call_type_method(recv, name, args, kwargs),
    }
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
    // `__new__` (if the class overrides it) creates the instance; it is an
    // implicit staticmethod, so `cls` is passed as the first argument. Otherwise
    // a bare instance is allocated (the default `object.__new__`).
    let inst = if let Some(newf) = with_host(|h| h.class_lookup(class, "__new__")) {
        let newf = match with_host(|h| h.get(&newf).cloned()) {
            Some(PyObj::StaticMethod(inner)) => inner,
            _ => newf,
        };
        let clsobj = with_host(|h| h.alloc(PyObj::Class(class.to_string())));
        let mut a = Vec::with_capacity(args.len() + 1);
        a.push(clsobj);
        a.extend(args.clone());
        invoke(&newf, a, kwargs.clone())?
    } else {
        with_host(|h| {
            h.alloc(PyObj::Instance(Instance {
                class: class.to_string(),
                attrs: IndexMap::new(),
            }))
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
    // Generator function: build a suspended coroutine over the already-bound
    // frame; nothing of the body runs until the first `next`/iteration.
    if def.is_generator {
        return Ok(make_generator(def.chunk.clone(), env, self_val, owner));
    }
    with_host(|h| {
        h.frames.push(Frame {
            env,
            globals_decl: HashSet::new(),
            nonlocals_decl: HashSet::new(),
            self_obj: self_val,
            owner,
            line: 0,
        })
    });
    let r = run_chunk_on(def.chunk.clone());
    let sig = with_host(|h| {
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

/// Bind positional + keyword arguments into a fresh call environment.
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
    let mut vars: IndexMap<String, Value> = IndexMap::new();
    let mut star_items = Vec::new();
    let npos = pos.len();
    for (i, val) in pos.into_iter().enumerate() {
        if i < np {
            vars.insert(def.params[i].clone(), val);
        } else if def.star.is_some() {
            star_items.push(val);
        } else {
            return Err(type_error(&format!(
                "{}() takes {} positional argument(s) but {} were given",
                def.name, np, npos
            )));
        }
    }
    let mut kwmap: IndexMap<String, Value> = IndexMap::new();
    for (k, v) in kwargs {
        kwmap.insert(k, v);
    }
    for i in 0..np {
        if !vars.contains_key(&def.params[i]) {
            if let Some(v) = kwmap.shift_remove(&def.params[i]) {
                vars.insert(def.params[i].clone(), v);
            } else if i >= np - ndef {
                let d = defaults[i - (np - ndef)].clone();
                vars.insert(def.params[i].clone(), d);
            } else {
                return Err(type_error(&format!(
                    "{}() missing required positional argument: '{}'",
                    def.name, def.params[i]
                )));
            }
        }
    }
    if let Some(star) = &def.star {
        if !star.is_empty() {
            let t = with_host(|h| h.new_tuple(star_items));
            vars.insert(star.clone(), t);
        }
    }
    // `kwonly_defaults` holds only the defaulted kwonly params, in kwonly order;
    // walk it with a separate cursor as we pass each optional param.
    let mut kwdef_cursor = 0usize;
    for (j, name) in def.kwonly.iter().enumerate() {
        let required = def.kwonly_required.get(j).copied().unwrap_or(true);
        if let Some(v) = kwmap.shift_remove(name) {
            vars.insert(name.clone(), v);
            if !required {
                kwdef_cursor += 1;
            }
        } else if required {
            return Err(type_error(&format!(
                "{}() missing required keyword-only argument: '{}'",
                def.name, name
            )));
        } else {
            // Apply the def-time default for this optional keyword-only param.
            let d = kwonly_defaults
                .get(kwdef_cursor)
                .cloned()
                .unwrap_or(Value::Undef);
            vars.insert(name.clone(), d);
            kwdef_cursor += 1;
        }
    }
    if let Some(kw) = &def.kwargs {
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
        for (k, v) in kwmap {
            let kv = with_host(|h| h.new_str(k.clone()));
            d.insert(PKey::Str(k), (kv, v));
        }
        let dict = with_host(|h| h.new_dict(d));
        vars.insert(kw.clone(), dict);
    } else if let Some((k, _)) = kwmap.iter().next() {
        return Err(type_error(&format!(
            "{}() got an unexpected keyword argument '{}'",
            def.name, k
        )));
    }
    env.borrow_mut().vars = vars;
    Ok(())
}

// ── more host operations referenced from builtins ────────────────────────────

impl PyHost {
    /// The current frame's environment, for a closure to capture.
    pub fn current_env_capture(&self) -> Env {
        self.frame().env.clone()
    }

    /// Build the `"Class: message"` display string for an exception's args.
    pub fn exc_message(&self, args: &[Value]) -> String {
        if args.is_empty() {
            String::new()
        } else if args.len() == 1 {
            self.str_of(&args[0])
        } else {
            let inner: Vec<String> = args.iter().map(|a| self.repr_of(a)).collect();
            format!("({})", inner.join(", "))
        }
    }
}

/// Run a class body function to populate its namespace, then register the class.
pub fn build_class(name: &str, bases: Vec<String>, body_func: &Value) -> Result<Value, String> {
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
            self_obj: None,
            owner: Some(name.to_string()),
            line: 0,
        })
    });
    let r = run_chunk_on(def.chunk.clone());
    with_host(|h| {
        h.frames.pop();
        h.signal.take();
    });
    r?;
    let ns: IndexMap<String, Value> = env.borrow().vars.clone();
    let cls = with_host(|h| h.register_class(name, bases, ns.clone()));
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
    Ok(cls)
}

/// Turn a raised value into an exception + the error string to abort with.
pub fn raise_value(exc: &Value) -> Result<String, String> {
    with_host(|h| {
        let obj = h.get(exc).cloned();
        match obj {
            Some(PyObj::Exception { class, args }) => {
                let msg = h.exc_message(&args);
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
                // Instantiate a user exception class with no args.
                let inst = h.alloc(PyObj::Instance(Instance {
                    class: name.clone(),
                    attrs: IndexMap::new(),
                }));
                h.exc = Some(inst);
                Ok(name)
            }
            Some(PyObj::Instance(i)) => {
                let class = i.class.clone();
                h.exc = Some(exc.clone());
                Ok(class)
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
fn make_generator(chunk: Chunk, env: Env, self_val: Option<Value>, owner: Option<String>) -> Value {
    let frame = Frame {
        env,
        globals_decl: HashSet::new(),
        nonlocals_decl: HashSet::new(),
        self_obj: self_val,
        owner,
        line: 0,
    };
    let id = with_host(|h| {
        let id = h.generators.len() as u32;
        h.generators.push(GenCell {
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
    let id = match CUR_GEN.with(|c| c.get()) {
        Some(id) => id,
        None => return Err(type_error("'yield' outside a generator")),
    };
    let yp = with_host(|h| h.generators[id as usize].yielder);
    // SAFETY: same-thread coroutine; the yielder lives for the whole body, and
    // we only reach here from inside that body (its stack is live).
    let yielder = unsafe { &*(yp as *const corosensei::Yielder<Value, Value>) };
    let sent = yielder.suspend(v);
    // A `.throw()`/`.close()` queued an exception to raise at this yield point.
    if let Some(exc) = with_host(|h| h.generators[id as usize].pending_throw.take()) {
        // `raise_value` sets `h.exc` and returns the abort string; propagate it as
        // an error so the body's own `try/except` can catch it.
        return Err(raise_value(&exc).unwrap_or_else(|e| e));
    }
    Ok(sent)
}

/// Whether a generator has been resumed at least once (a fresh generator only
/// accepts `send(None)`).
pub fn gen_started(gen: &Value) -> bool {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].started),
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
        )
    }) {
        let mut out = Vec::new();
        while let Some(x) = iter_step(v)? {
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
        _ => with_host(|h| h.iter_next(it)),
    }
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
    let mut out: Vec<Value> = Vec::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        match iter_step(s)? {
            Some(v) => out.push(v),
            None => {
                set_zip_done(it);
                if strict {
                    return Err(zip_strict_error(&sources, i));
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
/// (0-based) of the source that just exhausted mid-round.
fn zip_strict_error(sources: &[Value], i: usize) -> String {
    if i > 0 {
        // Sources 0..i were longer than source i.
        let than = if i == 1 {
            "argument 1".to_string()
        } else {
            format!("arguments 1-{i}")
        };
        return format!(
            "ValueError: zip() argument {} is shorter than {than}",
            i + 1
        );
    }
    // Source 0 exhausted first: find the first later source that still yields.
    for (j, s) in sources.iter().enumerate().skip(1) {
        if let Ok(Some(_)) = iter_step(s) {
            let than = if j == 1 {
                "argument 1".to_string()
            } else {
                format!("arguments 1-{j}")
            };
            return format!("ValueError: zip() argument {} is longer than {than}", j + 1);
        }
    }
    // All equal length after all — no error (shouldn't reach here).
    "ValueError: zip() argument is longer".to_string()
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
    // here and return before the `&str` match.
    let stdlib_entries: Option<Vec<(String, Value)>> = match name {
        "textwrap" => Some(with_host(crate::stdlib::textwrap::entries)),
        "statistics" => Some(with_host(crate::stdlib::statistics::entries)),
        _ => None,
    };
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
            let argv = h.new_list(vec![]);
            // Standard streams are `File` handles over the fixed side-table slots.
            let stdout = h.alloc(PyObj::File { id: 0 });
            let stderr = h.alloc(PyObj::File { id: 1 });
            let stdin = h.alloc(PyObj::File { id: 2 });
            vec![
                ("argv", argv),
                ("maxsize", Value::Int(i64::MAX)),
                ("version", h.new_str("3.12.0 (pythonrs)")),
                ("platform", h.new_str("pythonrs")),
                ("stdout", stdout),
                ("stderr", stderr),
                ("stdin", stdin),
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
fn namedtuple_construct(
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

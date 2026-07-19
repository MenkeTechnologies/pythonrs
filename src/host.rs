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
    Bool(bool),
    Int(i64),
    FloatBits(u64),
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
        }
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

    // ── heap allocation / accessors ──────────────────────────────────────
    pub fn alloc(&mut self, obj: PyObj) -> Value {
        self.heap.push(obj);
        Value::Obj((self.heap.len() - 1) as u32)
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

    pub fn new_str(&mut self, s: impl Into<String>) -> Value {
        self.alloc(PyObj::Str(s.into()))
    }
    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(PyObj::List(items))
    }
    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        self.alloc(PyObj::Tuple(items))
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
                Some(PyObj::Module { .. }) => "module".into(),
                Some(PyObj::BigInt(_)) => "int".into(),
                Some(PyObj::Complex(..)) => "complex".into(),
                Some(PyObj::Generator { .. }) => "generator".into(),
                Some(PyObj::File { .. }) => "TextIOWrapper".into(),
                Some(PyObj::Deque { .. }) => "deque".into(),
                Some(PyObj::NamedTupleType { .. }) => "type".into(),
                Some(PyObj::Partial { .. }) => "partial".into(),
                Some(PyObj::LruCache { .. }) => "functools._lru_cache_wrapper".into(),
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
                Some(PyObj::Instance(_)) => true, // __bool__/__len__ handled by caller
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
                Some(PyObj::Class(n)) => format!("<class '{n}'>"),
                Some(PyObj::Func(f)) => {
                    let name = self
                        .funcs
                        .get(f.def_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    format!("<function {name}>")
                }
                Some(PyObj::Builtin(n)) => format!("<built-in function {n}>"),
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
                _ => self.str_of(v),
            },
            _ => self.str_of(v),
        }
    }

    /// A hashable key for a dict/set. Returns an error for unhashable types.
    pub fn to_key(&self, v: &Value) -> Result<PKey, String> {
        Ok(match v {
            Value::Undef => PKey::None,
            Value::Bool(b) => PKey::Bool(*b),
            Value::Int(n) => PKey::Int(*n),
            Value::Float(f) => PKey::FloatBits(f.to_bits()),
            Value::Str(s) => PKey::Str((**s).clone()),
            Value::Obj(_) => match self.get(v) {
                Some(PyObj::Str(s)) => PKey::Str(s.clone()),
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
                    _ => match (a, b) {
                        (Value::Str(x), Value::Str(y)) => x == y,
                        _ => a == b,
                    },
                }
            }
        }
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
    if r == 0.0 {
        format!("{}j", fmt_float(i))
    } else {
        let sign = if i >= 0.0 { "+" } else { "-" };
        format!("({}{}{}j)", fmt_float(r), sign, fmt_float(i.abs()))
    }
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
                Err(self.optype_err("*", a, b))
            }
            Div => self.binop(binop::DIV, a, b),
            Mod => self.binop(binop::MOD, a, b),
            Pow => self.binop(binop::POW, a, b),
            Neg => {
                if let Some(x) = self.big_val(a) {
                    return Ok(self.norm_big(-x));
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

    fn norm_big(&mut self, b: num_bigint::BigInt) -> Value {
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
                _ => match (af, bf) {
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
                match (ai, bi) {
                    (Some(x), Some(y)) => Ok(Value::Int(match tag {
                        binop::BITAND => x & y,
                        binop::BITOR => x | y,
                        _ => x ^ y,
                    })),
                    _ => Err(self.optype_err("bitop", a, b)),
                }
            }
            binop::SHL | binop::SHR => match (ai, bi) {
                (Some(x), Some(y)) => Ok(Value::Int(if tag == binop::SHL {
                    x.wrapping_shl(y as u32)
                } else {
                    x >> y
                })),
                _ => Err(self.optype_err("shift", a, b)),
            },
            binop::MATMUL => Err(self.optype_err("@", a, b)),
            _ => Err(type_error("unknown binop")),
        }
    }

    /// `~x` / unary `+x`.
    pub fn unary(&mut self, tag: i64, v: &Value) -> Result<Value, String> {
        match tag {
            unop::INVERT => match self.as_int(v) {
                Some(n) => Ok(Value::Int(!n)),
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
                    d.insert(key, (kv, val));
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
            Some(PyObj::Iter(_)) | Some(PyObj::Generator { .. }) => return Ok(v.clone()),
            _ => {
                let items = self.iter_items(v)?;
                IterState::Seq { items, idx: 0 }
            }
        };
        Ok(self.alloc(PyObj::Iter(state)))
    }

    /// Advance an iterator; `None` on exhaustion.
    pub fn iter_next(&mut self, it: &Value) -> Result<Option<Value>, String> {
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
            Some(PyObj::Range { .. }) => {
                let items = self.iter_items(container)?;
                Ok(items.iter().any(|x| self.equal(x, item)))
            }
            _ => {
                let items = self.iter_items(container)?;
                Ok(items.iter().any(|x| self.equal(x, item)))
            }
        }
    }
}

/// Resolve the (start, stop) integer bounds of a slice given optional endpoints.
fn slice_bounds(lo: &Value, hi: &Value, step: i64, n: i64, h: &PyHost) -> (i64, i64) {
    let clamp = |x: i64| -> i64 {
        let k = if x < 0 { x + n } else { x };
        k.clamp(0, n)
    };
    let start = match h.as_int(lo) {
        Some(x) => {
            if x < 0 {
                (x + n).max(if step < 0 { -1 } else { 0 })
            } else {
                x.min(n)
            }
        }
        None => {
            if step < 0 {
                n - 1
            } else {
                0
            }
        }
    };
    let stop = match h.as_int(hi) {
        Some(x) => {
            if x < 0 {
                (x + n).max(if step < 0 { -1 } else { 0 })
            } else {
                x.min(n)
            }
        }
        None => {
            if step < 0 {
                -1
            } else {
                n
            }
        }
    };
    let _ = clamp;
    (start, stop)
}

// ── attributes ───────────────────────────────────────────────────────────────

impl PyHost {
    /// The method resolution order names for a class (this class first).
    pub fn mro_of(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![class.to_string()];
        while let Some(c) = stack.pop() {
            if out.contains(&c) {
                continue;
            }
            out.push(c.clone());
            if let Some(cd) = self.classes.get(&c) {
                for b in &cd.bases {
                    stack.push(b.clone());
                }
            }
        }
        out
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
                let class = inst.class.clone();
                if let Some(v) = self.class_lookup(&class, name) {
                    // Bind functions to the instance.
                    if matches!(self.get(&v), Some(PyObj::Func(_))) {
                        return Ok(self.alloc(PyObj::BoundMethod {
                            recv: recv.clone(),
                            func: v,
                        }));
                    }
                    return Ok(v);
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
                if let Some(v) = self.class_lookup(&cname, name) {
                    return Ok(v);
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
                let class = class.clone();
                Err(format!(
                    "AttributeError: '{class}' object has no attribute '{name}'"
                ))
            }
            Some(PyObj::Builtin(n)) if name == "__name__" => {
                // `type(x).__name__` — the builtin/type object's name.
                let n = n.clone();
                Ok(self.new_str(n))
            }
            _ => {
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

    /// `recv.name = val`.
    pub fn set_attr(&mut self, recv: &Value, name: &str, val: Value) -> Result<(), String> {
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
                if let Some(PyObj::Func(fv)) = fobj {
                    let owner = with_host(|h| method_owner(h, &class, name));
                    return run_user_func(&fv, Some(recv.clone()), owner, args, kwargs);
                }
                return invoke(&f, args, kwargs);
            }
            Err(format!(
                "AttributeError: '{class}' object has no attribute '{name}'"
            ))
        }
        Some(PyObj::Class(cname)) => {
            if let Some(f) = with_host(|h| h.class_lookup(&cname, name)) {
                let fobj = with_host(|h| h.get(&f).cloned());
                if let Some(PyObj::Func(fv)) = fobj {
                    // Class.method(...) — no implicit self binding.
                    return run_user_func(&fv, None, Some(cname.clone()), args, kwargs);
                }
                return invoke(&f, args, kwargs);
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
        _ => crate::builtins::call_type_method(recv, name, args, kwargs),
    }
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
    let inst = with_host(|h| {
        h.alloc(PyObj::Instance(Instance {
            class: class.to_string(),
            attrs: IndexMap::new(),
        }))
    });
    if let Some(f) = with_host(|h| h.class_lookup(class, "__init__")) {
        let fobj = with_host(|h| h.get(&f).cloned());
        if let Some(PyObj::Func(fv)) = fobj {
            let owner = with_host(|h| method_owner(h, class, "__init__"));
            run_user_func(&fv, Some(inst.clone()), owner, args, kwargs)?;
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
    bind_params(&env, &def, &fv.defaults, pos, kwargs)?;
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
    for (j, name) in def.kwonly.iter().enumerate() {
        if let Some(v) = kwmap.shift_remove(name) {
            vars.insert(name.clone(), v);
        } else if def.kwonly_required.get(j).copied().unwrap_or(true) {
            return Err(type_error(&format!(
                "{}() missing required keyword-only argument: '{}'",
                def.name, name
            )));
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
    Ok(with_host(|h| h.register_class(name, bases, ns)))
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
        });
        id
    });
    let coro = corosensei::Coroutine::new(
        move |yielder: &corosensei::Yielder<Value, Value>, _first: Value| {
            // Same thread → publish the yielder so `yield` (deep inside the
            // body's VM) can reach it. Valid for the whole body lifetime.
            with_host(|h| h.generators[id as usize].yielder = yielder as *const _ as *const ());
            let r = run_chunk_on(chunk);
            // A `return` inside the body leaves a Return signal; drop it so the
            // generator's StopIteration is clean.
            with_host(|h| {
                h.signal.take();
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
    Ok(yielder.suspend(v))
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
    with_host(|h| h.iter_items(v))
}

/// Advance any iterator — including a generator — by one step.
pub fn iter_step(it: &Value) -> Result<Option<Value>, String> {
    if with_host(|h| matches!(h.get(it), Some(PyObj::Generator { .. }))) {
        return gen_resume(it, Value::Undef);
    }
    with_host(|h| h.iter_next(it))
}

/// Import a module by name. A small built-in set is supported; unknown modules
/// raise `ModuleNotFoundError`.
pub fn import_module(name: &str) -> Result<Value, String> {
    // Native stdlib modules under src/stdlib. Their `entries` return owned-String
    // keys (vs the `&str` keys of the inline arms below), so build the namespace
    // here and return before the `&str` match.
    let stdlib_entries: Option<Vec<(String, Value)>> = match name {
        "itertools" => Some(with_host(crate::stdlib::itertools::entries)),
        "functools" => Some(with_host(crate::stdlib::functools::entries)),
        "json" => Some(with_host(crate::stdlib::json::entries)),
        "os" => Some(with_host(crate::stdlib::os::entries)),
        "random" => Some(with_host(crate::stdlib::random::entries)),
        "string" => Some(with_host(crate::stdlib::string::entries)),
        "re" => Some(with_host(crate::stdlib::re::entries)),
        "datetime" => Some(with_host(crate::stdlib::datetime::entries)),
        "heapq" => Some(with_host(crate::stdlib::heapq::entries)),
        "bisect" => Some(with_host(crate::stdlib::bisect::entries)),
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

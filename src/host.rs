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

use crate::ast::Span;
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
    pub const ELLIPSIS: u16 = 76; // [] -> the `Ellipsis` (`...`) singleton
    pub const IMPORT_STAR: u16 = 77; // [module] -> bind all public names (`from m import *`)
    pub const IMPORT_RELATIVE: u16 = 78; // [level, modpart, name] -> value bound by a relative `from . import`
    pub const TRY_ANNOTATION: u16 = 79; // [dict, key, thunk] -> set dict[key]=thunk(), forward-ref NameError skipped
    pub const IS_INT: u16 = 80; // [v] -> Bool: v is an `int` a native slot loop can hold (fixnum or bignum, not bool)
    pub const INTERPOLATION: u16 = 81; // [value, expression(str), conv(int), spec(str)] -> Interpolation
    pub const TEMPLATE: u16 = 82; // [segments(list of str|Interpolation)] -> Template
    pub const CHECK_BOUND: u16 = 83; // [name, value] -> value, or UnboundLocalError if unbound
    pub const UNBOUND: u16 = 84; // [] -> the never-assigned frame-slot marker
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
    /// A CPython `Foreign` object (stdlib-ffi) used as a dict/set key — an enum
    /// member, `Decimal`, `Fraction`, `datetime`, … `hash` is CPython's own
    /// `hash(obj)` (so value-equal objects share a bucket); `id` is the heap id of
    /// the object this key is *equal to* (its own, or a value-equal existing key it
    /// collapsed onto via `prepare_key` + `ffi::foreign_eq`), giving CPython value
    /// semantics rather than raw handle identity.
    Foreign {
        hash: i64,
        id: u32,
    },
    /// A type object (`PyObj::Class` or a builtin type/function) used as a key.
    /// Types are conceptual singletons by name, so they key by name — matching
    /// `is`/`==` on classes (`{int: 1}[int]`, `{C: 1}` for a user class `C`).
    Class(String),
    /// The `Ellipsis` (`...`) / `NotImplemented` singletons used as dict/set keys.
    /// Hashable by identity; the tag distinguishes the two (`Ellipsis` = 0).
    Singleton(u8),
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
    /// The docstring — the body's first statement when it is a bare string
    /// literal, else `None`. Surfaces as `func.__doc__`. `serde(default)` so
    /// bytecode cached before this field decodes to `None` (attribute present,
    /// content fills in on recompile).
    #[serde(default)]
    pub doc: Option<String>,
    /// Names this function closes over — referenced here and bound in an enclosing
    /// function scope (`co_freevars`; drives `func.__closure__`). Sorted.
    #[serde(default)]
    pub freevars: Vec<String>,
}

impl FuncDef {
    /// Clone everything EXCEPT the bytecode, leaving `chunk` empty.
    ///
    /// A call needs the signature, the name and the local/free-variable sets; it
    /// reads the body through the VM, which gets its own copy (usually a pooled
    /// one — see `run_chunk_cached`). `clone()` here would copy the whole `Chunk`
    /// per call, which on a recursive function is most of the runtime.
    pub fn clone_meta(&self) -> FuncDef {
        FuncDef {
            name: self.name.clone(),
            qualname: self.qualname.clone(),
            params: self.params.clone(),
            posonly: self.posonly,
            ndefaults: self.ndefaults,
            star: self.star.clone(),
            kwonly: self.kwonly.clone(),
            kwonly_required: self.kwonly_required.clone(),
            kwargs: self.kwargs.clone(),
            chunk: Chunk::default(),
            locals: self.locals.clone(),
            is_generator: self.is_generator,
            is_async: self.is_async,
            doc: self.doc.clone(),
            freevars: self.freevars.clone(),
        }
    }
}

/// A compiled lambda/comprehension body (same shape, unnamed).
pub type ProcDef = FuncDef;

/// A compiled `try`/`except`/`else`/`finally` block. Bodies are bare chunks run
/// in the *current* scope (so assignments persist), not fresh frames.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TryDef {
    pub body: Chunk,
    pub handlers: Vec<HandlerDef>,
    pub orelse: Option<Chunk>,
    pub finalbody: Option<Chunk>,
}

/// One compiled `except` / `except*` clause of a [`TryDef`].
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct HandlerDef {
    /// The exception type expression. `None` is a bare `except:`, which catches
    /// everything (`except*` always has one — CPython's grammar requires it).
    pub typ: Option<Chunk>,
    /// The `as name` binding, unbound again when the handler finishes.
    pub name: Option<String>,
    pub body: Chunk,
    /// `except*` (PEP 654): match against the caught exception GROUP rather than
    /// the exception itself, and run at most once with the matching subgroup.
    pub star: bool,
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
    pub ns: NameMap,
    /// The C3-ish MRO (this class first), by name.
    pub mro: Vec<String>,
    /// The metaclass name (`type(cls)`). `"type"` for an ordinary class; a user
    /// metaclass name for `class A(metaclass=M)`.
    pub metaclass: String,
    /// The `__module__` — the `__name__` of the module whose body defined the
    /// class. Empty defaults to `__main__` (a class built at the top level).
    pub module: String,
}

/// A live closure value.
#[derive(Clone)]
pub struct FuncVal {
    pub def_id: usize,
    /// The module (index into `PyHost::module_globals`) this function was defined
    /// in. Global-name resolution while the function runs targets this slot, so a
    /// vendored stdlib function sees its own module's names, not the importer's.
    pub module: usize,
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
    /// The `__annotations__` dict `{param|"return": annotation}`, built at def
    /// time. A heap [`PyObj::Dict`] handle (empty dict for an unannotated func).
    pub annotations: Value,
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

/// The `object`-level dunder slots that a type object exposes as
/// `wrapper_descriptor`s (`type(object.__init__)`). The comparison/hash/attr/
/// format slots CPython wraps; enough for introspection to classify them.
const OBJECT_SLOT_WRAPPERS: &[&str] = &[
    "__init__",
    "__str__",
    "__repr__",
    "__eq__",
    "__ne__",
    "__lt__",
    "__le__",
    "__gt__",
    "__ge__",
    "__hash__",
    "__delattr__",
    "__setattr__",
    "__getattribute__",
    "__format__",
    "__sizeof__",
    "__reduce__",
    "__reduce_ex__",
    "__dir__",
    "__init_subclass__",
    "__subclasshook__",
];

/// MT19937 — the Mersenne Twister behind CPython's `random`. Seeded via CPython's
/// `init_by_array`, so `random.seed(n); random.random()` matches CPython.
#[derive(Clone)]
pub struct MtState {
    mt: [u32; 624],
    index: usize,
}

impl Default for MtState {
    fn default() -> Self {
        let mut s = MtState {
            mt: [0; 624],
            index: 624,
        };
        s.init_by_array(&[19650218u32.wrapping_add(0)]); // placeholder; reseeded on use
        s
    }
}

impl MtState {
    fn init_genrand(&mut self, seed: u32) {
        self.mt[0] = seed;
        for i in 1..624 {
            self.mt[i] = 1812433253u32
                .wrapping_mul(self.mt[i - 1] ^ (self.mt[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        self.index = 624;
    }

    /// CPython's `init_by_array` (seeds from an array of 32-bit words).
    pub fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19650218);
        let (mut i, mut j) = (1usize, 0usize);
        let mut k = 624.max(key.len());
        while k > 0 {
            self.mt[i] = (self.mt[i]
                ^ ((self.mt[i - 1] ^ (self.mt[i - 1] >> 30)).wrapping_mul(1664525)))
            .wrapping_add(key[j])
            .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                self.mt[0] = self.mt[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = 623;
        while k > 0 {
            self.mt[i] = (self.mt[i]
                ^ ((self.mt[i - 1] ^ (self.mt[i - 1] >> 30)).wrapping_mul(1566083941)))
            .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                self.mt[0] = self.mt[623];
                i = 1;
            }
            k -= 1;
        }
        self.mt[0] = 0x80000000;
        self.index = 624;
    }

    pub fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for i in 0..624 {
                let y = (self.mt[i] & 0x80000000) | (self.mt[(i + 1) % 624] & 0x7fffffff);
                let mut next = self.mt[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    next ^= 0x9908b0df;
                }
                self.mt[i] = next;
            }
            self.index = 0;
        }
        let mut y = self.mt[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^= y >> 18;
        y
    }

    /// A random double in [0, 1) — CPython's `random_random` (53-bit).
    pub fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67108864.0 + b) * (1.0 / 9007199254740992.0)
    }

    /// The 625-element state (624 MT words + index), as CPython's `getstate`.
    pub fn state(&self) -> Vec<u32> {
        let mut v = self.mt.to_vec();
        v.push(self.index as u32);
        v
    }

    /// Restore from a 625-element state (`setstate`).
    pub fn set_state(&mut self, s: &[u32]) {
        if s.len() >= 625 {
            self.mt.copy_from_slice(&s[..624]);
            self.index = s[624] as usize;
        }
    }
}

/// The lazy `itertools` iterators. Each drives [`PyObj::ItertoolsIter`] through
/// its step function; the combinatoric ones (product/permutations/…) are built
/// eagerly instead.
#[derive(Clone, Copy, PartialEq)]
pub enum ItKind {
    Count,
    Repeat,
    Cycle,
    Chain,
    Accumulate,
    StarMap,
    Compress,
    DropWhile,
    TakeWhile,
    FilterFalse,
    ISlice,
    ZipLongest,
    Pairwise,
    Batched,
}

impl ItKind {
    fn type_name(self) -> &'static str {
        match self {
            ItKind::Count => "itertools.count",
            ItKind::Repeat => "itertools.repeat",
            ItKind::Cycle => "itertools.cycle",
            ItKind::Chain => "itertools.chain",
            ItKind::Accumulate => "itertools.accumulate",
            ItKind::StarMap => "itertools.starmap",
            ItKind::Compress => "itertools.compress",
            ItKind::DropWhile => "itertools.dropwhile",
            ItKind::TakeWhile => "itertools.takewhile",
            ItKind::FilterFalse => "itertools.filterfalse",
            ItKind::ISlice => "itertools.islice",
            ItKind::ZipLongest => "itertools.zip_longest",
            ItKind::Pairwise => "itertools.pairwise",
            ItKind::Batched => "itertools.batched",
        }
    }
}

/// The kind of a C-level attribute [`PyObj::Descriptor`], which fixes its
/// `type().__name__`. These are the descriptor types CPython's `types` module
/// derives so the stdlib can classify class members.
#[derive(Clone, Copy, PartialEq)]
pub enum DescKind {
    /// `type(object.__init__)` — an unbound slot wrapper on a type.
    WrapperDescriptor,
    /// `type(object().__str__)` — a slot wrapper bound to an instance.
    MethodWrapper,
    /// `type(dict.__dict__['fromkeys'])` — a C classmethod.
    ClassMethodDescriptor,
    /// `type(FunctionType.__code__)` — a computed (get/set) attribute.
    GetSetDescriptor,
    /// `type(FunctionType.__globals__)` — a C struct-member attribute.
    MemberDescriptor,
}

/// The attribute names of a `time.struct_time`, in order: the 9 sequence fields
/// (`tm_year … tm_isdst`) then the two attribute-only extras (`tm_gmtoff`,
/// `tm_zone`). Index/iteration cover only the first 9.
pub const STRUCT_TIME_FIELDS: &[&str] = &[
    "tm_year",
    "tm_mon",
    "tm_mday",
    "tm_hour",
    "tm_min",
    "tm_sec",
    "tm_wday",
    "tm_yday",
    "tm_isdst",
    "tm_gmtoff",
    "tm_zone",
];

/// Which `typing` type-parameter flavor a [`PyObj::TypeVarLike`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeVarKind {
    /// `TypeVar('T')` — a single type variable.
    TypeVar,
    /// `ParamSpec('P')` — a parameter specification.
    ParamSpec,
    /// `TypeVarTuple('Ts')` — a variadic type variable.
    TypeVarTuple,
}

impl TypeVarKind {
    /// The `type(...).__name__` CPython reports for this flavor.
    fn type_name(self) -> &'static str {
        match self {
            TypeVarKind::TypeVar => "TypeVar",
            TypeVarKind::ParamSpec => "ParamSpec",
            TypeVarKind::TypeVarTuple => "TypeVarTuple",
        }
    }
}

impl DescKind {
    pub fn type_name(self) -> &'static str {
        match self {
            DescKind::WrapperDescriptor => "wrapper_descriptor",
            DescKind::MethodWrapper => "method-wrapper",
            DescKind::ClassMethodDescriptor => "classmethod_descriptor",
            DescKind::GetSetDescriptor => "getset_descriptor",
            DescKind::MemberDescriptor => "member_descriptor",
        }
    }
}

/// How a value reads as a `Py_ssize_t`. See [`PyHost::index_fit`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexFit {
    /// An integer inside `Py_ssize_t`.
    Fits(i64),
    /// An integer outside `Py_ssize_t`; the flag is its sign (true = negative).
    TooLarge(bool),
    /// Not an integer at all.
    NotInt,
}

/// CPython's `PyNumber_AsSsize_t` overflow text, raised as `IndexError` from a
/// subscript and as `OverflowError` from a repetition or a length.
pub const INDEX_OVERFLOW: &str = "cannot fit 'int' into an index-sized integer";

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
    /// A `range` whose bounds do not fit `i64` (`range(1 << 1000)`). Kept separate
    /// so the common i64 `Range` stays a cheap scalar; all operations use bignum.
    BigRange {
        start: num_bigint::BigInt,
        stop: num_bigint::BigInt,
        step: num_bigint::BigInt,
    },
    Slice {
        lo: Value,
        hi: Value,
        step: Value,
    },
    /// A user function. Behind an `Rc` because dispatching a call reads the
    /// callable out of the heap by CLONING it — and a bare `FuncVal` clone copies
    /// both its default-value vectors, two heap allocations and two frees on
    /// every single Python call. The refcount bump costs nothing.
    Func(Rc<FuncVal>),
    /// A function's compiled code object (`func.__code__`), keyed to its
    /// `FuncDef`. Exposes the `co_*` introspection attributes the faithful stdlib
    /// reads (`types` derives `CodeType`, `inspect.signature`, `functools`,
    /// `dataclasses`). A native VM object — not a Python reimplementation.
    Code {
        def_id: usize,
    },
    /// A PEP 604 union type (`int | str`, `X | None`). `args` are the member
    /// type objects (flattened, deduped). `type()` is `types.UnionType`; used in
    /// annotations and `isinstance(x, int | str)`. Native VM object.
    Union {
        args: Vec<Value>,
    },
    /// A parameterized generic alias (`list[int]`, `WeakSet[T]`). `type()` is
    /// `types.GenericAlias`. Callable (constructs `origin`), forwards attribute
    /// access to `origin`, and substitutes `origin` as a base (`__mro_entries__`).
    /// Native so `list[int]` needs no import (types.py derives GenericAlias from
    /// `type(list[int])`, which would otherwise recurse into the loading module).
    GenericAlias {
        origin: Value,
        args: Vec<Value>,
    },
    /// A compiled `re` pattern (`re.compile(...)`). `id` indexes
    /// [`PyHost::regexes`]; `pattern` is the original source string (for
    /// `.pattern`), `flags` the `re` flag bits, `groups` the capture-group count.
    Pattern {
        id: usize,
        pattern: String,
        flags: i64,
        groups: usize,
    },
    /// A `re` match object. `text` is the searched string; `spans` holds the
    /// `(start, end)` byte range of each group (group 0 is the whole match), with
    /// `None` for a group that did not participate. `named` maps group names to
    /// their index. `pos`/`endpos` are the window the search ran in, as byte
    /// offsets like the spans — every one of these is converted to a codepoint
    /// index by [`crate::regexpr::char_index_of`] on the way out to Python, which
    /// counts `str` positions in characters and not in bytes.
    Match {
        text: String,
        spans: Vec<Option<(usize, usize)>>,
        named: Vec<(String, usize)>,
        pos: usize,
        endpos: usize,
    },
    /// A `time.struct_time` — the 9-field broken-down time sequence
    /// (`tm_year … tm_isdst`) plus the named-only `tm_gmtoff`/`tm_zone`. Indexes
    /// and iterates as a 9-tuple; the extra two are attribute-only. `fields` holds
    /// all 11 values in order.
    StructTime {
        fields: Vec<Value>,
    },
    /// A `typing` type parameter — `TypeVar`, `ParamSpec`, or `TypeVarTuple` — the
    /// C `_typing` primitives that `typing.py` builds on. `kind` selects which;
    /// `attrs` holds the dunder attributes (`__bound__`, `__constraints__`,
    /// `__covariant__`, `__default__`, …) that `typing.py` reads. Hashable by
    /// identity, usable as a generic argument, and `|`-combinable into a `Union`.
    TypeVarLike {
        kind: TypeVarKind,
        name: String,
        attrs: Value,
    },
    /// A `types.SimpleNamespace` — a mutable attribute bag (`sys.implementation`,
    /// argparse results). Attribute reads/writes go through `attrs`; `repr` is
    /// `namespace(k=v, …)`. Native VM object.
    Namespace {
        attrs: NameMap,
    },
    /// A read-only view of a mapping (`types.MappingProxyType`) — what a type's
    /// `__dict__` returns. Reads pass through to `dict`; there is no mutating API.
    MappingProxy {
        dict: Value,
    },
    /// One of CPython's C-level attribute descriptors, distinguished by `kind`.
    /// These exist so faithful introspection (`type(object.__init__)`,
    /// `type(dict.__dict__['fromkeys'])`, `type(FunctionType.__code__)`) yields
    /// the right distinct type object; `qual` is the `owner.name` display.
    Descriptor {
        kind: DescKind,
        qual: String,
    },
    /// An exception's `__traceback__` — a node in the traceback chain over the
    /// captured `(scope, line)` frames. `idx` is this node's position; `tb_next`
    /// advances toward the innermost frame.
    Traceback {
        frames: Vec<(String, u32)>,
        idx: usize,
    },
    /// A stack frame object (`tb.tb_frame`). Minimal: the scope name and current
    /// line, with `f_globals` the running module's globals.
    PyFrame {
        name: String,
        lineno: u32,
    },
    /// A frame's `f_code`. pythonrs has no CPython code objects, but the fields
    /// callers actually read off one — the defining file and the function name —
    /// are known: `logging.findCaller` walks frames comparing `co_filename`
    /// against its own, and `warnings` uses the same field to skip stdlib frames.
    FrameCode {
        name: String,
        lineno: u32,
    },
    /// A closure cell (`func.__closure__[i]`). Holds the current value of a free
    /// variable, read via `cell_contents`. `type()` is `cell`.
    Cell {
        value: Value,
    },
    /// A `_thread` lock (`allocate_lock()`/`RLock()`). `count` tracks nesting for
    /// a reentrant lock; a plain lock is 0/1. Functional under pythonrs's
    /// single-threaded user execution (the stdlib uses these for cache/IO guards).
    Lock {
        count: u32,
        reentrant: bool,
    },
    /// A compiled `struct.Struct`. Holds the format string; the spec is re-parsed
    /// per call, which is what `_struct`'s own cache amounts to at this scale.
    StructFmt(String),
    /// A `contextvars.ContextVar`. One interpreter thread here, so the "current
    /// context" is just the variable's own slot: `set` swaps it and hands back a
    /// `ContextToken` carrying the previous state for `reset`.
    ContextVar {
        name: String,
        default: Option<Box<Value>>,
        value: Option<Box<Value>>,
    },
    /// The token `ContextVar.set` returns — the variable it came from plus the
    /// value that was there (`None` = the variable was unset).
    ContextToken {
        var: Box<Value>,
        old: Option<Box<Value>>,
    },
    /// A `hashlib` hash object. The fed bytes are kept and hashed on demand,
    /// which is what makes `digest()` non-destructive and `copy()` a plain clone
    /// — CPython's objects can be read repeatedly and updated afterwards.
    Hasher {
        algo: crate::stdlib::pyhash::Algo,
        data: Vec<u8>,
        out_len: usize,
    },
    /// A `_csv` writer: the stream it emits to plus the dialect it emits under.
    CsvWriter {
        stream: Value,
        dialect: Box<crate::stdlib::pycsv::Dialect>,
    },
    /// A resolved `_csv` dialect, as `csv.get_dialect` returns.
    CsvDialect(Box<crate::stdlib::pycsv::Dialect>),
    /// A `_csv` reader. The rows are parsed up front; `line_num` still advances
    /// as they are consumed, because `csv.DictReader` reads it to report which
    /// input line a malformed row came from.
    CsvReader {
        rows: Vec<Value>,
        idx: usize,
        dialect: Box<crate::stdlib::pycsv::Dialect>,
    },
    /// The marker a frame slot holds before its local is first assigned.
    ///
    /// Slots are a plain `Vec<Value>`, and `Value::Undef` already means Python
    /// `None` — so a never-assigned slot had to be distinguishable from one
    /// holding `None`, or `def f(): print(x); x = 1` would print `None` instead
    /// of raising `UnboundLocalError`. The compiler emits a `CHECK_BOUND` only
    /// where it cannot prove the local is already bound.
    Unbound,
    /// A `contextvars.Context`. Each variable carries its own state here, so a
    /// context is a marker for "the current context" rather than a snapshot — but
    /// it has to be an OBJECT rather than a type marker, because `threading` runs
    /// every thread body through `self._context.run(self.run)` and that is a
    /// bound-method call on a value.
    ContextObj,
    /// A lazy `itertools` iterator. `sources` are pre-made input iterators, `func`
    /// an optional predicate/binop, `nums` integer state (count start/step,
    /// islice bounds, cursor), `buf` a value buffer (cycle's seen items,
    /// accumulate's running total), `flag`/`done` latches.
    ItertoolsIter {
        kind: ItKind,
        sources: Vec<Value>,
        func: Value,
        nums: Vec<i64>,
        buf: Vec<Value>,
        flag: bool,
        done: bool,
    },
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
    /// An imported module. The namespace is NOT stored here — it is the module's
    /// globals slot (`PyHost::module_globals[slot]`), the very map the module's own
    /// functions resolve globals against. Holding a snapshot instead let the two
    /// drift: `base64.binascii = None` rebound the attribute while `b64encode` kept
    /// reading the original, so monkeypatching a module silently did nothing.
    Module {
        name: String,
        slot: usize,
    },
    /// `_io.BytesIO` — an in-memory binary stream. `pos` is a byte offset.
    BytesIO {
        buf: Vec<u8>,
        pos: usize,
        closed: bool,
    },
    /// `_io.StringIO` — an in-memory text stream. `pos` and `len` are CODE POINT
    /// counts (CPython's text positions are), and `len` is carried alongside the
    /// buffer so appending stays O(1) instead of re-counting the whole string.
    StringIO {
        buf: String,
        len: usize,
        pos: usize,
        closed: bool,
        /// `newline=None`: translate every line ending to '\n' on write.
        translate: bool,
    },
    /// PEP 750 `string.templatelib.Template` — the value a `t"..."` literal
    /// evaluates to. `strings` always has exactly one more element than
    /// `interpolations`, so the two interleave back into the original literal.
    Template {
        strings: Vec<String>,
        interpolations: Vec<Value>,
    },
    /// One `{...}` field of a template: the evaluated value plus everything the
    /// consumer needs to decide what to do with it — the SOURCE text of the
    /// expression, the `!r`/`!s`/`!a` conversion, and the format spec.
    Interpolation {
        value: Value,
        expression: String,
        conversion: Option<char>,
        format_spec: String,
    },
    /// A module's `__dict__`: a live view of its globals slot, not a copy. CPython
    /// hands back the real namespace dict, and code relies on writing through it —
    /// `enum.global_enum` publishes an enum's members into its defining module with
    /// `sys.modules[mod].__dict__.update(...)`, which is how `calendar.JANUARY` and
    /// `calendar.MONDAY` come to exist. A snapshot would drop those on the floor.
    ModuleDict {
        slot: usize,
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
        /// The attribute name, learned from the class-namespace key the way
        /// CPython's `property.__set_name__` learns it. Empty for a property
        /// built outside a class body, where `__name__` falls back to the
        /// getter's own name.
        name: String,
    },
    /// A `functools.cached_property` descriptor: a *non-data* descriptor whose
    /// first access computes `func(instance)` and stores it in the instance
    /// `__dict__` under `name`, so every later access hits the dict directly.
    /// `name` is filled from the class-namespace key at class-build time (CPython
    /// learns it via `__set_name__`).
    CachedProperty {
        func: Value,
        name: String,
    },
    /// A `contextlib.redirect_stdout` / `redirect_stderr` context manager. On
    /// `__enter__` it saves the current stream target into `saved` and points the
    /// stream at `target`; `__exit__` restores `saved`. `stderr` selects which
    /// stream; nesting works because each instance holds its own `saved`.
    Redirect {
        stderr: bool,
        target: Value,
        saved: Option<Value>,
    },
    /// The `NotImplemented` singleton: returned by a binary/comparison dunder to
    /// signal "this operand pair is not my business", so the interpreter tries the
    /// reflected operation (and, for `==`/`!=`, falls back to identity).
    NotImplemented,
    /// The `Ellipsis` (`...`) singleton — a distinct truthy object of type
    /// `ellipsis`, not `None` (used in slices and as a type-annotation placeholder).
    Ellipsis,
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
    /// A `functools.cached_property` on first access: invoke `func(inst)`, cache
    /// the result in `inst`'s `__dict__` under `name`, and return it.
    CachedProperty {
        func: Value,
        inst: Value,
        name: String,
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

/// Which concrete iterator type a snapshot-backed cursor is standing in for.
///
/// CPython gives every builtin container its own iterator type, and the name is
/// observable through `type(it).__name__` and the default `repr`. pythonrs walks
/// most of them with one snapshot cursor ([`IterState::Seq`]), so the source
/// type would otherwise be erased and every iterator would answer `iterator` —
/// the name CPython reserves for the `__getitem__` sequence iterator alone.
/// Carrying the tag keeps the answer CPython's without a type per container.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IterKind {
    /// `PySeqIter_Type` — the `__getitem__` protocol fallback, and the default
    /// for any source whose CPython iterator type pythonrs cannot name.
    Seq,
    List,
    Tuple,
    /// `str` whose code points are all ASCII; CPython uses a dedicated type.
    StrAscii,
    Str,
    Bytes,
    Bytearray,
    /// Both `set` and `frozenset` — CPython names them the same.
    Set,
    Memory,
    Deque,
    /// `reversed(range(...))` — CPython hands back the same type `iter(range)`
    /// does, just walking the other way.
    Range,
    LongRange,
    DictKey,
    DictValue,
    DictItem,
    /// `reversed(list)` has its own type; `reversed()` over anything else that
    /// is not special-cased is the generic `reversed` object.
    ListReverse,
    Reversed,
    DictReverseKey,
    DictReverseValue,
    DictReverseItem,
}

impl IterKind {
    /// The CPython type name, exactly as `type(it).__name__` reports it.
    pub fn type_name(self) -> &'static str {
        match self {
            IterKind::Seq => "iterator",
            IterKind::List => "list_iterator",
            IterKind::Tuple => "tuple_iterator",
            IterKind::StrAscii => "str_ascii_iterator",
            IterKind::Str => "str_iterator",
            IterKind::Bytes => "bytes_iterator",
            IterKind::Bytearray => "bytearray_iterator",
            IterKind::Set => "set_iterator",
            IterKind::Memory => "memory_iterator",
            IterKind::Deque => "_deque_iterator",
            IterKind::Range => "range_iterator",
            IterKind::LongRange => "longrange_iterator",
            IterKind::DictKey => "dict_keyiterator",
            IterKind::DictValue => "dict_valueiterator",
            IterKind::DictItem => "dict_itemiterator",
            IterKind::ListReverse => "list_reverseiterator",
            IterKind::Reversed => "reversed",
            IterKind::DictReverseKey => "dict_reversekeyiterator",
            IterKind::DictReverseValue => "dict_reversevalueiterator",
            IterKind::DictReverseItem => "dict_reverseitemiterator",
        }
    }

    /// The kind `iter(x)` produces for a `str` — CPython splits ASCII off into
    /// its own type.
    pub fn of_str(s: &str) -> IterKind {
        if s.is_ascii() {
            IterKind::StrAscii
        } else {
            IterKind::Str
        }
    }

    /// The kind `reversed(view)` produces for the dict view this kind iterates.
    pub fn reversed(self) -> IterKind {
        match self {
            IterKind::DictKey => IterKind::DictReverseKey,
            IterKind::DictValue => IterKind::DictReverseValue,
            IterKind::DictItem => IterKind::DictReverseItem,
            IterKind::List => IterKind::ListReverse,
            _ => IterKind::Reversed,
        }
    }
}

/// Iterator cursor state.
#[derive(Clone)]
pub enum IterState {
    Seq {
        items: Vec<Value>,
        idx: usize,
        kind: IterKind,
    },
    RangeIter {
        cur: i64,
        stop: i64,
        step: i64,
    },
    BigRangeIter {
        cur: num_bigint::BigInt,
        stop: num_bigint::BigInt,
        step: num_bigint::BigInt,
    },
    DictKeys {
        keys: Vec<Value>,
        idx: usize,
    },
}

// ── I/O side table ───────────────────────────────────────────────────────────

/// One live file / standard stream, indexed by `PyObj::File.id`. Slots 0/1/2 are
/// always `Stdout`/`Stderr`/`Stdin`. A `File` holds the owned `std::fs::File`
/// (`None` once closed), the path (for `repr`/`f.name`), the mode string exactly
/// as passed to `open` (what CPython's `f.mode` returns), and whether it was
/// opened for reading and/or writing. `std::fs::File` is not `Clone`, so — like
/// rubylang's `IoCell` — the handle lives here, never inline in a `PyObj`.
pub enum IoCell {
    Stdout,
    Stderr,
    Stdin,
    File {
        file: Option<std::fs::File>,
        path: String,
        mode: String,
        readable: bool,
        writable: bool,
        /// The `encoding=` a text handle was opened with. `open` used to drop the
        /// argument on the floor and always use UTF-8, so a latin-1 write
        /// produced two bytes where CPython produces one.
        encoding: TextEncoding,
        /// `newline=None` (the default): translate `\r\n` and a lone `\r` to
        /// `\n` on read, as CPython's universal newlines do. `newline=''` and an
        /// explicit terminator both leave the bytes alone.
        newline_translate: bool,
    },
}

/// The text encodings a native file handle can be opened with. pythonrs decodes
/// and encodes these itself; anything else is `LookupError` at `open` time rather
/// than silently-wrong bytes at write time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Ascii,
    Latin1,
}

impl TextEncoding {
    /// Resolve a Python encoding name, matching CPython's normalization (case
    /// and `-`/`_` insensitive) for the three this supports.
    pub fn from_name(name: &str) -> Option<Self> {
        let n: String = name
            .chars()
            .filter(|c| *c != '-' && *c != '_' && *c != ' ')
            .flat_map(char::to_lowercase)
            .collect();
        match n.as_str() {
            "utf8" | "u8" | "utf" | "utf8mb4" => Some(Self::Utf8),
            "ascii" | "usascii" | "646" => Some(Self::Ascii),
            "latin1" | "latin" | "iso88591" | "8859" | "cp819" | "l1" => Some(Self::Latin1),
            _ => None,
        }
    }

    /// Bytes → text. Undecodable input is replaced rather than raising, matching
    /// what the UTF-8 path already did (`errors=` is not implemented).
    fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Self::Ascii => bytes
                .iter()
                .map(|b| if *b < 0x80 { *b as char } else { '\u{fffd}' })
                .collect(),
            Self::Latin1 => bytes.iter().map(|b| *b as char).collect(),
        }
    }

    /// Text → bytes. A character the encoding cannot represent is the error
    /// CPython raises for the same write.
    fn encode(self, s: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Utf8 => Ok(s.as_bytes().to_vec()),
            Self::Ascii | Self::Latin1 => {
                let limit = if self == Self::Ascii { 0x80 } else { 0x100 };
                let name = if self == Self::Ascii {
                    "ascii"
                } else {
                    "latin-1"
                };
                let mut out = Vec::with_capacity(s.len());
                for (i, c) in s.chars().enumerate() {
                    let cp = c as u32;
                    if cp >= limit {
                        return Err(format!(
                            "UnicodeEncodeError: '{name}' codec can't encode character \
                             '\\u{cp:04x}' in position {i}: ordinal not in range({limit})"
                        ));
                    }
                    out.push(cp as u8);
                }
                Ok(out)
            }
        }
    }
}

/// Apply universal newlines to freshly decoded text: `\r\n` and a lone `\r`
/// both become `\n`.
fn translate_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
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
/// A NAMESPACE: identifier → value, in insertion order.
///
/// Every global read, local read, attribute read, and instance-attribute store
/// is a lookup in one of these, which makes the hash of a short `String` key one
/// of the most frequently executed operations in the whole runtime. `IndexMap`'s
/// default `RandomState` hashes it with SipHash, and a profile of an ordinary
/// `for i in range(n): t += a[i]` loop put the SipHash rounds plus the `String`
/// comparisons behind them at roughly the weight of the entire fusevm dispatch
/// loop — for keys that are one or two characters long.
///
/// `FxHasher` is the hasher rustc uses for its own identifier tables: a multiply
/// and a rotate per word. Nothing Python-visible changes with it. An `IndexMap`
/// keeps INSERTION order whatever the hasher, so `vars()`, `dir()`, `__dict__`,
/// and module iteration order are exactly as before; and Python's own `hash()`
/// and `dict` ordering do not run through here at all — they go through
/// `to_key`/`pyhash`, which implement CPython's algorithm and honor
/// `PYTHONHASHSEED`.
pub type NameMap = IndexMap<String, Value, rustc_hash::FxBuildHasher>;

pub struct EnvData {
    pub vars: NameMap,
    pub parent: Option<Env>,
}
pub type Env = Rc<RefCell<EnvData>>;

/// Bind `name` to `val` in a [`NameMap`], allocating the key ONLY when the name
/// is new to the map.
///
/// `map.insert(name.to_string(), val)` allocates and copies the name on every
/// write, and the overwhelming majority of writes are REBINDS of a name the map
/// already holds — a loop counter is stored once per iteration and its key
/// allocated once per iteration with it. Looking the slot up first makes the
/// rebind allocation-free at the cost of nothing: `get_mut` and `insert` hash
/// the name once each, so a genuinely new name pays one extra hash, once.
pub fn bind_name(map: &mut NameMap, name: &str, val: Value) {
    match map.get_mut(name) {
        Some(slot) => *slot = val,
        None => {
            map.insert(name.to_string(), val);
        }
    }
}

fn new_env(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(EnvData {
        vars: NameMap::default(),
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
    /// Shared by `Rc`: this is the callee's `FuncDef::locals` and never changes,
    /// so a call borrows it instead of rebuilding the set (a per-call HashSet
    /// allocation plus a String clone per local).
    pub locals_set: Rc<HashSet<String>>,
    /// True for a class-body frame. A class scope is NOT an enclosing scope for
    /// nested functions (methods, comprehensions), so a closure defined here
    /// captures the class body's PARENT env, skipping the class namespace.
    pub is_class_body: bool,
    pub self_obj: Option<Value>,
    pub owner: Option<String>,
    /// The scope name shown in a traceback frame (`<module>`, a function name, or
    /// a class name for a class body). `Rc` for the same reason as `owner` — this
    /// used to be two `String` allocations on every single Python call.
    pub name: Rc<str>,
    /// Source line currently executing in this frame — updated by the DAP debug
    /// line hook (`--dap`) and by the error path when an exception aborts a chunk.
    pub line: u32,
    /// Source span of the op that aborted this frame — set alongside `line` by
    /// the error path, used to draw the traceback caret. `Span::NONE` otherwise.
    pub span: Span,
}

/// A non-local control signal.
#[derive(Clone)]
pub enum Signal {
    Return(Value),
    Break,
    Continue,
}

/// The Python runtime.
/// Attribute-completion surface for a `base.<partial>` receiver, produced by
/// [`PyHost::attr_completions`]. Instances and modules carry concrete names;
/// builtin scalars/containers defer to the LSP corpus chapter for their method
/// list (the REPL completer holds the corpus, the host does not).
pub enum AttrCompletion {
    /// A builtin type — expand to its method names via the named LSP corpus
    /// chapter (`"str"`, `"list"`, `"dict"`, `"set"`, `"tuple"`, `"int"`,
    /// `"float"`, `"bytes"`, `"frozenset"`).
    BuiltinType(&'static str),
    /// Concrete attribute / member names (instance attrs + MRO methods, or a
    /// module's namespace).
    Names(Vec<String>),
}

/// One `atexit`-registered callback: `(func, args, kwargs)`.
pub type AtexitCallback = (Value, Vec<Value>, Vec<(String, Value)>);

pub struct PyHost {
    heap: Vec<PyObj>,
    /// Function/lambda templates, indexed by def id.
    /// Function definitions, shared by `Rc` so a call can take one without
    /// copying its signature vectors (or its bytecode) — `run_user_func` does
    /// this on every single Python call.
    pub funcs: Vec<Rc<FuncDef>>,
    /// `FuncDef::locals` as a ready-made set, one per entry of `funcs` and built
    /// with it. A call clones the `Rc` into its frame instead of collecting the
    /// set again (see `Frame::locals_set`).
    pub func_locals: Vec<Rc<HashSet<String>>>,
    /// Each function's traceback name, pre-shared. `Frame::name` used to be a
    /// `String` copied out of the `FuncDef` on every call.
    pub func_names: Vec<Rc<str>>,
    /// Class templates by name.
    pub classes: IndexMap<String, ClassDef>,
    /// Memoized C3 linearizations, keyed by class name.
    ///
    /// `mro_of` is on the path of every attribute read, every method dispatch and
    /// every `isinstance`, and it used to re-run the full C3 algorithm — recursing
    /// into each base and allocating a fresh `Vec<String>` at every level — on
    /// each call. One `obj.attr` through a 21-deep class chain cost 45us. The
    /// cache is dropped whenever a class is registered, since a new class can
    /// change what an existing name resolves to.
    mro_cache: std::cell::RefCell<HashMap<String, std::rc::Rc<Vec<String>>>>,
    /// try/except/finally block templates, indexed by try id.
    pub tries: Vec<TryDef>,
    /// Per-module global namespaces (each imported module's `__dict__`), index 0
    /// being `__main__`. A function/class-body/generator captures the id of the
    /// module it was defined in and resolves its globals through that slot, so a
    /// vendored stdlib function sees ITS module's names — not the importer's —
    /// exactly as CPython's `func.__globals__ is module.__dict__`. The slot for an
    /// imported module is kept alive after import for this reason.
    module_globals: Vec<NameMap>,
    /// One `__dict__` view per module slot, so `mod.__dict__ is mod.__dict__`.
    module_dicts: HashMap<usize, Value>,
    /// The module whose globals the currently-running code resolves against —
    /// swapped around every function call, class body, and generator resume.
    cur_module: usize,
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
    /// Names of classes decorated with `functools.total_ordering`. The decorator
    /// runs natively (keeping the class a native pythonrs class), and comparison
    /// dispatch derives the missing rich-comparison ops for a marked class.
    total_ordering: HashSet<String>,
    /// Exception chaining links, keyed by the exception object's heap index.
    /// `.0` = `__cause__` (`raise X from Y`), `.1` = `__context__` (the
    /// exception being handled when this one was raised). `Value::Undef` = unset.
    pub exc_links: HashMap<u32, (Value, Value)>,
    /// Per-exception traceback frames (`__traceback__`), keyed by the exception's
    /// heap index: the outermost-first `(scope, line)` stack captured when the
    /// exception is caught. Used to render `__cause__`/`__context__` chain blocks
    /// in an uncaught traceback (the final exception uses the live `traceback`).
    pub exc_tb: HashMap<u32, Vec<(String, u32, Span)>>,
    /// For every exception group carved out of another by `split`/`subgroup`/
    /// `derive`: the heap id of the ROOT group it came from. `except*` reads it
    /// to tell a piece of the caught group that a handler re-raised (which is
    /// merged back into the original's nesting) from a group the handler built
    /// fresh (which becomes a sibling). CPython answers the same question by
    /// comparing `__cause__`/`__context__`/`__traceback__`/`__notes__` identity;
    /// an explicit link is exact and cannot alias an unrelated group.
    pub eg_split_root: HashMap<u32, u32>,
    /// Interned type objects for the builtin names (see `builtin_object`).
    builtin_objects: HashMap<String, Value>,
    /// Exceptions whose traceback starts EMPTY at the frame that produced them:
    /// the group `except*` reconstructs once its handlers have run. CPython
    /// builds that group after the handler finishes, so the frame holding the
    /// `try` is not part of its traceback — only the frames it later unwinds
    /// through are.
    pub tb_starts_empty: HashSet<u32>,
    /// What the last `NameError`/`AttributeError` would need to offer CPython's
    /// `Did you mean: 'x'?` hint. Captured where the error is raised — the
    /// candidate scope has unwound by the time the traceback renders — and cheap
    /// enough to sit on an error path: a name plus either an `Rc` scope handle or
    /// the receiver.
    pub suggest: Option<SuggestCtx>,
    /// Arbitrary attributes assigned to a function object (`func.__dict__`), keyed
    /// by the function's heap id. CPython functions carry a writable dict; the
    /// stdlib uses it for `__isabstractmethod__`, `functools.wraps`, decorators.
    pub func_attrs: HashMap<u32, NameMap>,
    /// Codec search functions registered by `_codecs.register` (the `encodings`
    /// package installs one at import), the resolved-codec cache keyed by
    /// normalized name, and user error handlers from `register_error`.
    pub codec_search: Vec<Value>,
    pub codec_cache: HashMap<String, Value>,
    pub codec_errors: HashMap<String, Value>,
    /// Exception heap ids raised with an explicit `from` clause (`raise X from Y`
    /// or `raise X from None`), which sets `__suppress_context__` — the implicit
    /// `__context__` is then hidden from the rendered traceback.
    pub suppress_context: HashSet<u32>,
    /// Base-class names (its CPython `__mro__`) of each exception type raised over
    /// the stdlib-ffi bridge, keyed by the exception's class name — so
    /// `except ValueError` matches a foreign `json.JSONDecodeError`, which pythonrs
    /// has no builtin knowledge of. Populated at raise time by the ffi error path.
    pub foreign_exc_bases: HashMap<String, Vec<String>>,
    /// What the last exception raised over the stdlib-ffi bridge carried beyond
    /// its rendered line. See [`ForeignExc`].
    pub foreign_exc: Option<ForeignExc>,
    /// Process arguments exposed to the program as `sys.argv`. Set once per run
    /// by `init_runtime` (`['']` for the REPL/stdin default, `['script', …]` for
    /// a file, `['-c', …]` for `-c`).
    pub argv: Vec<String>,
    /// Absolute path bound to the top-level `__file__`, `None` for `-c`/stdin.
    pub main_file: Option<String>,
    /// `__main__`'s `__loader__`/`__builtins__` are still placeholders.
    ///
    /// Both are real CPython objects (`_frozen_importlib.BuiltinImporter`, the
    /// `builtins` module), so materializing them boots the embedded interpreter
    /// — ~12 ms that a script importing nothing should not pay. `init_runtime`
    /// therefore reserves the two names in CPython's own insertion order and
    /// leaves this flag set; [`ensure_main_dunders`] fills the values the first
    /// time anything can observe them. Cleared once filled, and by an
    /// assignment to either name (the user's value wins).
    pub pending_main_dunders: bool,
    /// The full program source — used to reconstruct traceback source lines.
    pub prog_source: String,
    /// The filename shown in traceback frames (`<string>`, `<stdin>`, or a path).
    pub tb_filename: String,
    /// Whether traceback frames print their source line (true for a file / `-c`,
    /// false for stdin — CPython cannot retrieve stdin source).
    pub tb_show_source: bool,
    /// Frames captured (innermost first) as an exception unwinds the call stack,
    /// each `(scope_name, line, span)`. Cleared when the exception is caught.
    pub traceback: Vec<(String, u32, Span)>,
    /// The current `sys.stdout` / `sys.stderr` targets when reassigned away from
    /// the native streams (`sys.stdout = io.StringIO()`,
    /// `contextlib.redirect_stdout`). `None` = the native stream. Tracked on the
    /// host (not a module ns) because `import` is not cached, so different `sys`
    /// module instances must share one redirect. `print` and the REPL displayhook
    /// consult these.
    pub stdout_target: Option<Value>,
    pub stderr_target: Option<Value>,
    /// In-process output sink. When `Some`, everything the program writes to the
    /// native stdout/stderr streams is appended here instead of reaching the
    /// process — what an embedder that owns the terminal (a TUI) needs so a
    /// `print` cannot corrupt its display. `None` (the default) is the ordinary
    /// standalone `python` behaviour. Distinct from `stdout_target`, which is
    /// Python-level (`sys.stdout = …`) and redirects to a Python object; this is
    /// host-level and catches every native write, including one through a
    /// reassigned `sys.stdout` that still ends at the real stream.
    capture: Option<String>,
    /// Imported modules keyed by dotted name — pythonrs's `sys.modules`. A second
    /// `import x` returns the cached module object (CPython identity + run-once
    /// side effects) instead of re-executing the vendored `.py`. Populated by
    /// `import_module` on every successful import (native, vendored, or bridged).
    modules: NameMap,
    /// The live `sys.modules` dict handle (set when `sys` is built). Kept in sync
    /// with the internal cache so Python-level `sys.modules[k] = v` (e.g. os.py's
    /// `sys.modules['os.path'] = path`) is honored by subsequent imports.
    sys_modules: Option<Value>,
    /// Per-`_random.Random`-instance Mersenne Twister state, keyed by the
    /// instance's heap id (the RNG backing `random`).
    pub mt_states: HashMap<u32, MtState>,
    /// `atexit`-registered callbacks: `(func, args, kwargs)` in registration
    /// order. Run LIFO at interpreter shutdown ([`run_atexit_callbacks`]).
    pub atexit_callbacks: Vec<AtexitCallback>,
    /// Compiled `re` patterns, indexed by [`PyObj::Pattern`]'s `id`. The native
    /// `re` module (backed by the `regex` crate) stores each compiled `Regex`
    /// here so a `PyObj::Pattern` stays cheap to clone.
    pub regexes: Vec<crate::regexpr::PyRegex>,
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
    /// The module the generator body resolves globals against — restored on every
    /// resume so a generator function defined in a vendored module sees its own
    /// module's names (see [`FuncVal::module`]).
    module: usize,
}

/// The parts of an exception raised over the stdlib-ffi bridge that its rendered
/// `"Class: message"` line cannot carry.
///
/// The bridge can only hand a fusevm abort a STRING, and rebuilding the
/// exception by re-parsing that string assumes two things that are often false:
/// that `str(exc) == args[0]`, and that `args` is all there is. `KeyError`
/// breaks the first — its `__str__` is `repr(args[0])`, so `os.environ['X']`
/// came back as `KeyError("'X'")` with a doubled quote layer in both `str(e)`
/// and `e.args`. `json.JSONDecodeError` breaks the second — `e.lineno`,
/// `e.colno`, `e.pos`, `e.msg` and `e.doc` live in the instance `__dict__` and
/// were simply gone, so the standard `except ValueError as e: e.lineno` idiom
/// raised `AttributeError`.
///
/// Recorded at raise time by the ffi error path and consumed by `synth_exc` on
/// the first byte-identical line match, which also clears it. A stale entry can
/// only be picked up by an identical rendering, for which its contents are still
/// the right ones.
pub struct ForeignExc {
    /// The `"Class: message"` line the exception rendered to.
    pub line: String,
    /// The real `args` tuple.
    pub args: Vec<Value>,
    /// Instance attributes beyond `args`, from the exception's `__dict__`.
    pub attrs: Vec<(String, Value)>,
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
    /// Native recursion depth inside [`PyHost::equal`], and whether it was ever
    /// exceeded since the flag was last read.
    ///
    /// A self-referential container compared against a DIFFERENT self-referential
    /// container recurses forever — `a = [1]; a.append(a); b = [1]; b.append(b);
    /// a == b` walked the cycle until the native stack was gone and the process
    /// ABORTED, where CPython raises a catchable RecursionError. `equal` returns
    /// a bare `bool` and cannot report an error, so it records one here and the
    /// operator entry points turn it into the exception.
    static EQUAL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static EQUAL_OVERFLOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How deep `equal` may nest before it gives up. Comfortably past any real
/// data — CPython's own comparison limit is the same order — and far short of
/// the native stack.
const EQUAL_DEPTH_LIMIT: u32 = 1000;

/// Take the "comparison recursed too deeply" flag, clearing it. `true` means the
/// last comparison gave up and its `false` answer is not an answer.
pub fn equal_overflowed() -> bool {
    EQUAL_OVERFLOW.with(|c| c.replace(false))
}

/// The error that flag stands for.
pub fn comparison_recursion_error() -> String {
    "RecursionError: maximum recursion depth exceeded in comparison".to_string()
}

thread_local! {
    /// Op-index → source span, keyed by the owning chunk's `op_hash` (stable
    /// across the clones made per call). The compiler registers one table per
    /// chunk it builds; `record_err_line` looks the failing op's span up here to
    /// draw a CPython-style traceback caret. Grows for the process lifetime, like
    /// the object heap.
    static POSITIONS: RefCell<HashMap<u64, Vec<Span>>> = RefCell::new(HashMap::new());
}

/// Register a chunk's op-index → span table under its `op_hash`. Called by the
/// compiler as each chunk is finalized.
pub fn register_positions(op_hash: u64, table: Vec<Span>) {
    POSITIONS.with(|p| {
        p.borrow_mut().insert(op_hash, table);
    });
}

/// The span recorded for op `idx` of the chunk with hash `op_hash`, or
/// `Span::NONE` if none (unregistered chunk, or a non-caret op).
pub fn lookup_position(op_hash: u64, idx: usize) -> Span {
    POSITIONS.with(|p| {
        p.borrow()
            .get(&op_hash)
            .and_then(|t| t.get(idx))
            .copied()
            .unwrap_or(Span::NONE)
    })
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

/// Take the pending-key table, leaving it empty. A container op starts from a
/// fresh keying context and hands the caller's table back when it finishes.
fn pending_key_take() -> HashMap<u32, PKey> {
    PENDING_KEY.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// Put back a table taken by [`pending_key_take`].
fn pending_key_restore(t: HashMap<u32, PKey>) {
    PENDING_KEY.with(|p| *p.borrow_mut() = t);
}

#[cfg(feature = "stdlib-ffi")]
thread_local! {
    /// Pre-computed CPython `str`/`repr` for reachable `Foreign` objects, keyed by
    /// heap id. A `Foreign` with a user-defined `__str__`/`__repr__` runs pythonrs
    /// code when CPython formats it, which re-enters `with_host`; computing it here
    /// *outside* any borrow (via [`prefetch_foreign_display`]) and reading it from
    /// the borrowed [`PyHost::str_of`]/[`PyHost::repr_of`] avoids the double borrow.
    static PENDING_DISPLAY: RefCell<HashMap<u32, (String, String)>> =
        RefCell::new(HashMap::new());
}

/// Read the prefetched `(str, repr)` for a foreign id, if prepared.
#[cfg(feature = "stdlib-ffi")]
fn pending_display_get(id: u32) -> Option<(String, String)> {
    PENDING_DISPLAY.with(|p| p.borrow().get(&id).cloned())
}

/// Walk `v` and, for every reachable `Foreign`, compute its CPython `str`/`repr`
/// with NO host borrow held and cache it in `PENDING_DISPLAY`. Short scoped
/// borrows only read the object graph; the ffi calls (which may re-enter pythonrs
/// for a user `__str__`/`__repr__`) run between them. Mirrors `prepare_key`.
#[cfg(feature = "stdlib-ffi")]
fn prefetch_foreign_display(v: &Value) {
    fn walk(v: &Value, seen: &mut HashSet<u32>) {
        let Value::Obj(id) = v else { return };
        if !seen.insert(*id) {
            return;
        }
        // Copy out just what we need under a short borrow, then release it.
        enum Kind {
            Foreign(u32),
            Children(Vec<Value>),
            None,
        }
        let kind = with_host(|h| match h.get(v) {
            Some(PyObj::Foreign(f)) => Kind::Foreign(*f),
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Kind::Children(l.clone()),
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => {
                Kind::Children(s.values().cloned().collect())
            }
            Some(PyObj::Deque { items, .. }) => Kind::Children(items.iter().cloned().collect()),
            Some(PyObj::Dict(d)) => Kind::Children(
                d.values()
                    .flat_map(|(k, val)| [k.clone(), val.clone()])
                    .collect(),
            ),
            _ => Kind::None,
        });
        match kind {
            Kind::Foreign(fid) => {
                // ffi runs with no borrow held (may re-enter pythonrs). Cache under
                // the FOREIGN id — that is the key `PyObj::Foreign(id)` presents to
                // the borrowed `str_of`/`repr_of`, not the outer heap id.
                let s = crate::ffi::str_of(fid);
                let r = crate::ffi::repr_of(fid);
                PENDING_DISPLAY.with(|p| p.borrow_mut().insert(fid, (s, r)));
            }
            Kind::Children(children) => {
                for c in &children {
                    walk(c, seen);
                }
            }
            Kind::None => {}
        }
    }
    let mut seen = HashSet::new();
    walk(v, &mut seen);
}

/// `str(v)` from outside any host borrow: prefetch reachable foreign displays,
/// then format. Callers that already hold a borrow use `PyHost::str_of` directly
/// (safe as long as the borrowing op prefetched, or the value has no foreign
/// with user `__str__`/`__repr__`).
pub fn str_of(v: &Value) -> String {
    #[cfg(feature = "stdlib-ffi")]
    prefetch_foreign_display(v);
    let s = with_host(|h| h.str_of(v));
    #[cfg(feature = "stdlib-ffi")]
    PENDING_DISPLAY.with(|p| p.borrow_mut().clear());
    s
}

/// `repr(v)` from outside any host borrow — see [`str_of`].
pub fn repr_of(v: &Value) -> String {
    #[cfg(feature = "stdlib-ffi")]
    prefetch_foreign_display(v);
    let s = with_host(|h| h.repr_of(v));
    #[cfg(feature = "stdlib-ffi")]
    PENDING_DISPLAY.with(|p| p.borrow_mut().clear());
    s
}

/// Run `f` with mutable access to the thread-local host.
pub fn with_host<R>(f: impl FnOnce(&mut PyHost) -> R) -> R {
    HOST.with(|h| f(&mut h.borrow_mut()))
}

/// Reset the host to a clean slate (fresh module frame).
pub fn reset_host() {
    with_host(|h| *h = PyHost::new());
    // The VM pool is keyed by `def_id`, which indexes the host's function table —
    // a table this call restarts at zero. A VM left over from the previous
    // program would be handed to whatever function takes that id next, running
    // the OLD body. Drop them with the table they belong to.
    VM_POOL.with(|p| p.borrow_mut().clear());
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
        // The module dunders CPython binds in `__main__` before the body runs,
        // in CPython's own insertion order — `list(globals())` is observable and
        // module globals are an `IndexMap`. `__doc__` is overwritten by the
        // compiled `__doc__ = <docstring>` store when the script opens with a
        // string literal. `__loader__` and `__builtins__` are reserved here and
        // filled by `ensure_main_dunders` on first observation; reserving them
        // now is what keeps them in the right position once they are.
        for dunder in [
            "__doc__",
            "__package__",
            "__loader__",
            "__spec__",
            "__builtins__",
        ] {
            h.set_global(dunder, Value::Undef);
        }
        h.pending_main_dunders = true;
        // `__file__` and `__cached__` follow, in that order — they exist only
        // when there IS a script (`python -c` has neither).
        if let Some(path) = main_file {
            let f = h.new_str(path);
            h.set_global("__file__", f);
            h.set_global("__cached__", Value::Undef);
        }
    });
}

/// Fill `__main__`'s reserved `__loader__` / `__builtins__` with the real
/// CPython objects, the first time anything can observe them.
///
/// `__builtins__` is the `builtins` module; `__loader__` is
/// `_frozen_importlib.BuiltinImporter` for `-c`/stdin and a
/// `_frozen_importlib_external.SourceFileLoader('__main__', path)` for a script
/// — exactly what CPython's `runpy`/`pymain` bind. Both come back over the FFI
/// bridge as the interpreter's own objects rather than as synthesized
/// look-alikes, so their `repr`, `dir`, and attributes are CPython's.
///
/// Must NOT be called from inside a `with_host` closure: importing re-enters the
/// host and would panic on the `RefCell`.
pub fn ensure_main_dunders() {
    if !with_host(|h| h.pending_main_dunders) {
        return;
    }
    // Clear first: the import below runs through paths that observe globals, and
    // a re-entrant call would recurse without a terminating condition.
    let main_file = with_host(|h| {
        h.pending_main_dunders = false;
        h.main_file.clone()
    });
    let builtins = import_module("builtins").ok();
    let loader = main_dunder_loader(main_file.as_deref());
    with_host(|h| {
        // Write into `__main__` (slot 0) explicitly: the trigger may have come
        // from code running with another module's globals current.
        for (name, val) in [("__loader__", loader), ("__builtins__", builtins)] {
            if let Some(v) = val {
                h.module_globals[0].insert(name.to_string(), v);
            }
        }
    });
}

/// `__main__.__loader__` for this invocation, or `None` when the bridge is off
/// (the name then stays bound to `None`, as it is for a frozen `__main__`).
#[cfg(feature = "stdlib-ffi")]
fn main_dunder_loader(main_file: Option<&str>) -> Option<Value> {
    let bootstrap = import_module("_frozen_importlib").ok()?;
    match main_file {
        // A script is loaded from source: CPython binds a live SourceFileLoader
        // instance carrying the module name and the path it was read from.
        Some(path) => {
            let ext = import_module("_frozen_importlib_external").ok()?;
            let cls = with_host(|h| h.get_attr(&ext, "SourceFileLoader")).ok()?;
            let args = with_host(|h| vec![h.new_str("__main__"), h.new_str(path)]);
            invoke(&cls, args, vec![]).ok()
        }
        // `-c` / stdin have no source file, so `__main__` keeps the importer that
        // handles built-in modules.
        None => with_host(|h| h.get_attr(&bootstrap, "BuiltinImporter")).ok(),
    }
}

#[cfg(not(feature = "stdlib-ffi"))]
fn main_dunder_loader(_main_file: Option<&str>) -> Option<Value> {
    None
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
            func_locals: Vec::new(),
            func_names: Vec::new(),
            classes: IndexMap::new(),
            mro_cache: std::cell::RefCell::new(HashMap::new()),
            tries: Vec::new(),
            module_globals: vec![NameMap::default()],
            module_dicts: HashMap::new(),
            cur_module: 0,
            frames: vec![Frame {
                env: module_env,
                globals_decl: HashSet::new(),
                nonlocals_decl: HashSet::new(),
                locals_set: Rc::new(HashSet::new()),
                is_class_body: false,
                self_obj: None,
                owner: None,
                name: Rc::from("<module>"),
                line: 0,
                span: Span::NONE,
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
            total_ordering: HashSet::new(),
            exc_links: HashMap::new(),
            exc_tb: HashMap::new(),
            eg_split_root: HashMap::new(),
            builtin_objects: HashMap::new(),
            tb_starts_empty: HashSet::new(),
            suggest: None,
            func_attrs: HashMap::new(),
            codec_search: Vec::new(),
            codec_cache: HashMap::new(),
            codec_errors: HashMap::new(),
            suppress_context: HashSet::new(),
            foreign_exc_bases: HashMap::new(),
            foreign_exc: None,
            argv: vec![String::new()],
            main_file: None,
            pending_main_dunders: false,
            prog_source: String::new(),
            tb_filename: "<string>".to_string(),
            tb_show_source: true,
            traceback: Vec::new(),
            stdout_target: None,
            stderr_target: None,
            capture: None,
            modules: NameMap::default(),
            sys_modules: None,
            mt_states: HashMap::new(),
            atexit_callbacks: Vec::new(),
            regexes: Vec::new(),
        }
    }

    /// A previously-imported module by dotted name (pythonrs's `sys.modules`).
    pub fn cached_module(&self, name: &str) -> Option<Value> {
        if let Some(m) = self.modules.get(name) {
            return Some(m.clone());
        }
        // A module registered only in the Python-level sys.modules (e.g. os.py's
        // `sys.modules['os.path'] = path`).
        if let Some(sm) = &self.sys_modules {
            if let Some(PyObj::Dict(d)) = self.get(sm) {
                if let Some((_, v)) = d.get(&PKey::Str(name.to_string())) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// Record `module` under `name` so a re-import returns the same object. Also
    /// mirrored into the live `sys.modules` dict.
    pub fn cache_module(&mut self, name: &str, module: Value) {
        self.modules.insert(name.to_string(), module.clone());
        if let Some(sm) = self.sys_modules.clone() {
            let kv = self.new_str(name.to_string());
            if let Some(PyObj::Dict(d)) = self.get_mut(&sm) {
                d.insert(PKey::Str(name.to_string()), (kv, module));
            }
        }
    }

    /// What `sys.modules[name]` currently holds, if anything. Python code can
    /// rebind it during an import (`sys.modules[__name__] = other`).
    pub fn sys_module_entry(&self, name: &str) -> Option<Value> {
        let sm = self.sys_modules.as_ref()?;
        match self.get(sm) {
            Some(PyObj::Dict(d)) => d.get(&PKey::Str(name.to_string())).map(|(_, v)| v.clone()),
            _ => None,
        }
    }

    /// Drop `name` from the module cache (and live `sys.modules`). Used when a
    /// module body fails mid-import: CPython removes the half-built module so a
    /// retry re-runs the body and re-raises, rather than resolving to a broken
    /// cached shell (which would silently mask a dependency's import failure).
    pub fn uncache_module(&mut self, name: &str) {
        self.modules.shift_remove(name);
        if let Some(sm) = self.sys_modules.clone() {
            if let Some(PyObj::Dict(d)) = self.get_mut(&sm) {
                d.shift_remove(&PKey::Str(name.to_string()));
            }
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
        for f in funcs {
            self.func_locals
                .push(Rc::new(f.locals.iter().cloned().collect()));
            self.func_names.push(Rc::from(f.name.as_str()));
            self.funcs.push(Rc::new(f));
        }
        self.tries.extend(tries);
    }
    pub fn try_def(&self, id: usize) -> Option<TryDef> {
        self.tries.get(id).cloned()
    }
    /// The value a (sub)generator `return`ed — its `StopIteration.value`, read by
    /// the `yield from` delegation op.
    /// A generator's introspection state: `(name, running, suspended)`.
    ///
    /// `running` is true only while the body is on the stack — the cell's
    /// coroutine is TAKEN OUT across a resume, so its absence is the running
    /// flag. `suspended` is CPython 3.13's `gi_suspended`: started, parked at a
    /// `yield`, and not yet finished.
    pub fn gen_state(&self, gen: &Value) -> Option<(String, bool, bool)> {
        match self.get(gen) {
            Some(PyObj::Generator { id }) => {
                let g = &self.generators[*id as usize];
                let running = g.coro.is_none() && !g.done;
                Some((
                    g.func_name.clone(),
                    running,
                    g.started && !g.done && !running,
                ))
            }
            _ => None,
        }
    }

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

    /// The interned type object for a builtin name (`len`, `int`, `ValueError`).
    /// CPython's builtins are singletons — `id(len) == id(len)` — and reading one
    /// is on the hot path of every call whose callee is a builtin, so the object
    /// is allocated once and handed back.
    pub fn builtin_object(&mut self, name: &str) -> Value {
        if let Some(v) = self.builtin_objects.get(name) {
            return v.clone();
        }
        let v = self.alloc(PyObj::Builtin(name.to_string()));
        self.builtin_objects.insert(name.to_string(), v.clone());
        v
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

    /// Native-only build (`--no-default-features`): there is no `stdlib-ffi`
    /// bridge, so no value is ever a CPython `Foreign` object. Kept as an
    /// always-`None` companion so the unconditional call sites in `builtins.rs`
    /// and the foreign-object fast paths here compile without a `cfg` at every
    /// caller.
    #[cfg(not(feature = "stdlib-ffi"))]
    pub fn foreign_id(&self, _v: &Value) -> Option<u32> {
        None
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
    /// A one-shot sequence iterator whose CPython type is the `__getitem__`
    /// protocol iterator: `next()` walks the items once, then exhausts.
    pub fn new_iter_seq(&mut self, items: Vec<Value>) -> Value {
        self.new_iter_kind(items, IterKind::Seq)
    }
    /// The same cursor, standing in for a named CPython iterator type.
    pub fn new_iter_kind(&mut self, items: Vec<Value>, kind: IterKind) -> Value {
        self.alloc(PyObj::Iter(IterState::Seq {
            items,
            idx: 0,
            kind,
        }))
    }
    pub fn new_dict(&mut self, pairs: IndexMap<PKey, (Value, Value)>) -> Value {
        self.alloc(PyObj::Dict(pairs))
    }

    /// Allocate a class instance with a fresh live `__dict__` (a real
    /// [`PyObj::Dict`]) seeded from `attrs`. Every `PyObj::Instance` must be
    /// built through here so its `dict` field points at heap storage that
    /// `obj.__dict__` can hand back by handle (see [`Instance`]).
    pub fn new_instance(&mut self, class: String, attrs: NameMap) -> Value {
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
    /// The backing dict's key map of a `dict_keys` view — the set a key view
    /// participates as, with the dict's own (already canonical) keys. `None` for
    /// any other object, including the `dict_values`/`dict_items` views.
    pub fn keys_view_map(&self, v: &Value) -> Option<IndexMap<PKey, Value>> {
        let dict = match self.get(v) {
            Some(PyObj::DictView { dict, kind: 0 }) => dict.clone(),
            _ => return None,
        };
        match self.get(&dict) {
            Some(PyObj::Dict(d)) => Some(
                d.iter()
                    .map(|(k, (kv, _))| (k.clone(), kv.clone()))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The key-set of a set-like operand for `==` and the subset order: a
    /// `set`/`frozenset`'s own keys, or a `dict_keys`/`dict_items` view's —
    /// CPython's key and item views ARE set-like, so `d.keys() == {1, 2}` and
    /// `d.keys() <= {1, 2}` are real answers, not `False` and a `TypeError`. A
    /// `dict_values` view has no set behavior, and neither has anything else;
    /// both give `None`. Unlike [`Self::setmap_operand`] this allocates no
    /// objects and hashes no user instances, so the borrowed `equal` can use it.
    pub fn view_keyset(&self, v: &Value) -> Option<Vec<PKey>> {
        match self.get(v) {
            Some(PyObj::Set(s) | PyObj::Frozenset(s)) => Some(s.keys().cloned().collect()),
            Some(PyObj::DictView { dict, kind }) if *kind == 0 || *kind == 2 => {
                let kind = *kind;
                match self.get(dict) {
                    Some(PyObj::Dict(d)) => {
                        let mut out = Vec::with_capacity(d.len());
                        for (k, (_, val)) in d {
                            out.push(if kind == 0 {
                                k.clone()
                            } else {
                                PKey::Tuple(vec![k.clone(), self.to_key(val).ok()?])
                            });
                        }
                        Some(out)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// True when either operand is a dict view — the cue to route a comparison
    /// through [`Self::view_keyset`] instead of the zero-copy `setlike` path.
    fn either_is_view(&self, a: &Value, b: &Value) -> bool {
        matches!(self.get(a), Some(PyObj::DictView { .. }))
            || matches!(self.get(b), Some(PyObj::DictView { .. }))
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

    /// If `v` is a valid PEP 604 union member — a type object, an existing union
    /// (flattened), or `None` (as `NoneType`) — return its member list. `None`
    /// otherwise, so `|` falls through to its numeric/set meanings.
    fn union_members(&self, v: &Value) -> Option<Vec<Value>> {
        match self.get(v) {
            Some(PyObj::Union { args }) => Some(args.clone()),
            Some(PyObj::Class(_)) | Some(PyObj::GenericAlias { .. }) => Some(vec![v.clone()]),
            Some(PyObj::Builtin(n)) if crate::builtins::is_type_object_name(n) => {
                Some(vec![v.clone()])
            }
            // Bare `None` in a union stands for `NoneType` (`int | None`).
            None if matches!(v, Value::Undef) => Some(vec![v.clone()]),
            _ => None,
        }
    }

    /// The display name of a union member for `repr` (`int | str`): a type's
    /// name, or `None` for the bare-`None` (`NoneType`) member.
    fn union_member_name(&self, v: &Value) -> String {
        match self.get(v) {
            // `NoneType` prints as `None` inside a union — `int | None`, never
            // `int | NoneType`. `typing.Optional[X]` reaches this with the real
            // type object rather than the bare `None` literal.
            Some(PyObj::Builtin(n)) if n == "NoneType" => "None".to_string(),
            Some(PyObj::Builtin(n)) => n.clone(),
            Some(PyObj::Class(n)) => self.class_display_path(n),
            None if matches!(v, Value::Undef) => "None".to_string(),
            _ => self.repr_of(v),
        }
    }

    /// A user class's `module.qualname` — how a union or generic alias names it
    /// (`int | __main__.A`, `list[__main__.Outer.Inner]`), where a builtin type
    /// is named bare. A nested class contributes its full lexical path, which is
    /// why this reads `qualname` rather than the registry key.
    pub fn class_display_path(&self, class: &str) -> String {
        let Some(cd) = self.classes.get(class) else {
            return class.to_string();
        };
        let module = if cd.module.is_empty() {
            "__main__"
        } else {
            &cd.module
        };
        let qual = if cd.qualname.is_empty() {
            &cd.name
        } else {
            &cd.qualname
        };
        format!("{module}.{qual}")
    }

    /// The display name of a generic-alias argument for `repr` (`list[int]`): a
    /// type object's name, a nested alias's own repr, else the value's repr.
    fn generic_arg_name(&self, v: &Value) -> String {
        match self.get(v) {
            Some(PyObj::Builtin(n)) => n.clone(),
            Some(PyObj::Class(n)) => self.class_display_path(n),
            // `tuple[int, ...]` — inside an alias CPython prints the ellipsis in
            // its literal spelling, not as the `Ellipsis` repr.
            Some(PyObj::Ellipsis) => "...".to_string(),
            Some(PyObj::GenericAlias { .. }) | Some(PyObj::Union { .. }) => self.repr_of(v),
            None if matches!(v, Value::Undef) => "None".to_string(),
            _ => self.repr_of(v),
        }
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
        self.setmap_operand(v, false)
    }

    /// The key-set of `v` for a set operation. A `set`/`frozenset` and the
    /// key/item dict views are always set-like; a list or tuple is coerced only
    /// when the OTHER operand already is one, which is CPython's rule — a dict
    /// view's set operators accept any iterable (`d.keys() - ['a']`, which
    /// `csv.DictWriter` uses to find extra keys), while `[1] - [2]` stays a
    /// TypeError.
    pub fn setmap_operand(
        &mut self,
        v: &Value,
        other_is_set: bool,
    ) -> Option<IndexMap<PKey, Value>> {
        if let Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) = self.get(v) {
            return Some(s.clone());
        }
        if other_is_set {
            if let Some(PyObj::List(items)) | Some(PyObj::Tuple(items)) = self.get(v) {
                let items = items.clone();
                let mut out: IndexMap<PKey, Value> = IndexMap::new();
                for it in items {
                    if let Ok(k) = self.to_key(&it) {
                        out.insert(k, it);
                    }
                }
                return Some(out);
            }
        }
        let kind = match self.get(v) {
            Some(PyObj::DictView { kind, .. }) if *kind == 0 || *kind == 2 => *kind,
            _ => return None,
        };
        // A `dict_keys` view IS its dict's key map — take it verbatim instead of
        // re-hashing the key objects. Re-hashing dropped every value key (a user
        // `__hash__` cannot run under this borrow, and the `Err` was discarded),
        // so `d.keys() & s` silently lost exactly those elements.
        if kind == 0 {
            if let Some(d) = self.keys_view_map(v) {
                return Some(d);
            }
        }
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

    /// Park any active function frames so a nested `eval`/`exec` run executes at
    /// MODULE scope — its name binding and lookup then reach the real module
    /// globals rather than the calling function's locals (`exec("g = 1")` inside a
    /// function sets a module global, matching CPython's default-namespace rule).
    /// Returns the parked frames to hand back to [`Self::restore_scope`]. A run already
    /// at module scope parks nothing.
    pub fn enter_module_scope(&mut self) -> Vec<Frame> {
        self.frames.split_off(1)
    }

    /// Restore the frames parked by [`Self::enter_module_scope`] once the nested run
    /// finishes, so the interrupted caller resumes with its frame intact.
    pub fn restore_scope(&mut self, parked: Vec<Frame>) {
        self.frames.truncate(1);
        self.frames.extend(parked);
    }

    /// The visible local bindings of the innermost (calling) frame — its env chain
    /// flattened, inner scopes shadowing enclosing ones. This is the `locals()` a
    /// nested `eval`/`exec` reads from when called inside a function. Empty at
    /// module scope (its bindings are the globals, included separately).
    pub fn caller_locals(&self) -> NameMap {
        let mut out: NameMap = NameMap::default();
        if self.frames.len() <= 1 {
            return out;
        }
        // Collect the env chain, then apply outermost→innermost so the nearest
        // scope wins on a name shadowed along the chain.
        let mut chain: Vec<Env> = Vec::new();
        let mut cur = Some(self.frame().env.clone());
        while let Some(e) = cur {
            cur = e.borrow().parent.clone();
            chain.push(e);
        }
        for e in chain.into_iter().rev() {
            for (k, v) in e.borrow().vars.iter() {
                out.insert(k.clone(), v.clone());
            }
        }
        out
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
    /// Record the failing op's line AND caret span into the innermost frame — the
    /// error path (`record_err_line`) calls this so an uncaught traceback can both
    /// name the line and underline the exact sub-expression.
    pub fn set_cur_line_span(&mut self, line: u32, span: Span) {
        if let Some(f) = self.frames.last_mut() {
            f.line = line;
            f.span = span;
        }
    }
    /// Capture the innermost frame's `(name, line, span)` into the in-flight
    /// traceback as an exception unwinds past it. Called just before the frame is
    /// popped.
    pub fn push_tb_frame(&mut self) {
        if let Some(f) = self.frames.last() {
            self.traceback.push((f.name.to_string(), f.line, f.span));
        }
    }
    /// Snapshot `exc`'s traceback (outermost-first) into `exc_tb`, just before the
    /// caught exception's live `traceback` is cleared. The exception's own trace
    /// runs from the frame that *catches* it (the innermost still-active frame,
    /// where the `try` lives) down to the raise point — frames *above* the catcher
    /// (its callers) are not part of this exception's trace, since it was caught
    /// before reaching them. Lets an uncaught traceback later render this
    /// exception's frames when it appears as a `__cause__`/`__context__`.
    pub fn capture_exc_tb(&mut self, exc: &Value) {
        let Value::Obj(id) = exc else { return };
        let mut tb: Vec<(String, u32, Span)> = Vec::new();
        if let Some(f) = self.frames.last() {
            if !self.tb_starts_empty.contains(id) {
                tb.push((f.name.to_string(), f.line, f.span));
            }
        }
        for f in self.traceback.iter().rev() {
            tb.push(f.clone());
        }
        self.exc_tb.insert(*id, tb);
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
            .map(|f| (f.name.to_string(), f.line))
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
        self.globals().get(name).cloned()
    }

    /// The globals of the currently-running module (the `cur_module` slot).
    #[inline]
    fn globals(&self) -> &NameMap {
        &self.module_globals[self.cur_module]
    }

    /// Mutable globals of the currently-running module.
    #[inline]
    fn globals_mut(&mut self) -> &mut NameMap {
        &mut self.module_globals[self.cur_module]
    }

    /// The current module's `__package__` (the anchor for a relative import).
    /// Falls back to `__name__`'s parent when unset, as CPython's importlib does.
    pub fn current_package(&self) -> String {
        let g = self.globals();
        if let Some(v) = g.get("__package__") {
            if let Some(s) = self.as_str(v) {
                return s;
            }
        }
        // No `__package__`: derive from `__name__` (a package keeps its own name,
        // a module drops its final component).
        if let Some(v) = g.get("__name__") {
            if let Some(n) = self.as_str(v) {
                return match n.rsplit_once('.') {
                    Some((parent, _)) => parent.to_string(),
                    None => String::new(),
                };
            }
        }
        String::new()
    }

    /// The globals slot backing a module object, if `v` is one.
    #[inline]
    pub fn module_slot(&self, v: &Value) -> Option<usize> {
        match self.get(v) {
            Some(PyObj::Module { slot, .. }) => Some(*slot),
            _ => None,
        }
    }

    /// A specific module's namespace — the same map its functions read globals
    /// from, so a write here is visible to the module's own code.
    #[inline]
    pub fn module_ns(&self, slot: usize) -> &NameMap {
        &self.module_globals[slot]
    }

    #[inline]
    pub fn module_ns_mut(&mut self, slot: usize) -> &mut NameMap {
        &mut self.module_globals[slot]
    }

    /// Set attribute `name = val` on a module object.
    pub fn set_module_attr(&mut self, module: &Value, name: &str, val: Value) {
        if let Some(slot) = self.module_slot(module) {
            self.module_globals[slot].insert(name.to_string(), val);
        }
    }

    /// The module a freshly-created function/class-body captures (the module now
    /// executing).
    #[inline]
    pub fn cur_module(&self) -> usize {
        self.cur_module
    }

    /// Set the module code resolves globals against, returning the previous one.
    /// Callers save the return value and restore it once the run completes, so
    /// nested calls across modules stay isolated.
    #[inline]
    pub fn swap_module(&mut self, m: usize) -> usize {
        std::mem::replace(&mut self.cur_module, m)
    }

    /// Allocate a fresh module-globals slot (its `__dict__`) and return its id.
    /// The slot is never freed — functions defined in the module keep resolving
    /// their globals through it for the life of the process.
    pub fn new_module_slot(&mut self, ns: NameMap) -> usize {
        let id = self.module_globals.len();
        self.module_globals.push(ns);
        id
    }

    /// Read-only view of a specific module's globals (for building its
    /// `PyObj::Module` namespace snapshot after import).
    pub fn module_globals_pairs(&self, id: usize) -> Vec<(String, Value)> {
        self.module_globals[id]
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn read_global(&self, name: &str) -> Option<Value> {
        self.globals().get(name).cloned()
    }

    /// The `__dict__` view of module slot `slot`, allocated once and reused so the
    /// identity is stable (`mod.__dict__ is mod.__dict__`, as in CPython).
    pub fn module_dict(&mut self, slot: usize) -> Value {
        if let Some(v) = self.module_dicts.get(&slot) {
            return v.clone();
        }
        let v = self.alloc(PyObj::ModuleDict { slot });
        self.module_dicts.insert(slot, v.clone());
        v
    }

    /// If `v` is a module `__dict__` view, a plain dict holding the same entries.
    /// Read-only operations answer from this copy rather than duplicating every
    /// dict code path; the mutators write through to the slot instead.
    pub fn module_dict_snapshot(&mut self, v: &Value) -> Option<Value> {
        let slot = match self.get(v) {
            Some(PyObj::ModuleDict { slot }) => *slot,
            _ => return None,
        };
        let pairs = self.module_globals_pairs(slot);
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::with_capacity(pairs.len());
        for (k, val) in pairs {
            let kv = self.new_str(k.clone());
            d.insert(PKey::Str(k), (kv, val));
        }
        Some(self.new_dict(d))
    }

    /// The module slot a `__dict__` view writes through to.
    #[inline]
    pub fn module_dict_slot(&self, v: &Value) -> Option<usize> {
        match self.get(v) {
            Some(PyObj::ModuleDict { slot }) => Some(*slot),
            _ => None,
        }
    }

    /// Look `name` up along a specific captured env chain (for `__closure__`
    /// free-variable reads), independent of the current frame.
    fn env_lookup(&self, env: &Env, name: &str) -> Option<Value> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if let Some(v) = e.borrow().vars.get(name) {
                return Some(v.clone());
            }
            cur = e.borrow().parent.clone();
        }
        None
    }

    /// `UnboundLocalError`-aware read for a bare-name load. If `name` is a genuine
    /// local of the current frame (in `locals_set`, not declared `global`/
    /// `nonlocal`) it resolves ONLY in the current env: present → its value,
    /// absent → [`NameRead::Unbound`] (an `UnboundLocalError`, never a fall-through
    /// to an enclosing or global binding). Otherwise it is a normal LEGB read.
    pub fn read_name_checked(&self, name: &str) -> NameRead {
        let f = self.frame();
        // Ordered by cost, because this runs on EVERY name read and each set
        // probe hashes the whole name.
        //
        // The two declaration sets go first as `is_empty`, not `contains`: a
        // `global`/`nonlocal` declaration is rare (most functions have neither),
        // and an empty set can be excluded without hashing anything.
        //
        // Then `vars` — NOT `locals_set` — because a hit there is the answer.
        // `read_name`'s own first step is this same lookup in this same env, so
        // a name bound in the current env resolves to the same value down either
        // path; `locals_set` only ever decided whether a MISS is an unbound local
        // or a name from an outer scope, and a miss is the rare case. Probing it
        // first cost a second hash on every successful read to classify a failure
        // that had not happened.
        if (f.globals_decl.is_empty() || !f.globals_decl.contains(name))
            && (f.nonlocals_decl.is_empty() || !f.nonlocals_decl.contains(name))
        {
            if let Some(v) = self.cur_env().borrow().vars.get(name) {
                return NameRead::Value(v.clone());
            }
            if f.locals_set.contains(name) {
                return NameRead::Unbound;
            }
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
        // An explicit `__loader__ = …` / `__builtins__ = …` at module level wins
        // over the reserved placeholder, so stop planning to overwrite it. The
        // `bool` short-circuits before the name compares, and it is only ever
        // true for the handful of stores before the first observation.
        if self.pending_main_dunders && matches!(name, "__loader__" | "__builtins__") {
            self.pending_main_dunders = false;
        }
        // Same empty-set short-circuit as `read_name_checked`, for the same
        // reason: this is on every variable WRITE, and both declaration sets are
        // empty in the overwhelming majority of frames.
        let f = self.frame();
        if !f.globals_decl.is_empty() && f.globals_decl.contains(name) {
            bind_name(self.globals_mut(), name, val);
            return;
        }
        if !self.frame().nonlocals_decl.is_empty() && self.frame().nonlocals_decl.contains(name) {
            // Rebind the nearest ENCLOSING function scope that binds `name`
            // (skip the current env — that is what distinguishes it from a plain
            // local assignment and from `global`).
            let cur = self.cur_env();
            let mut env = cur.borrow().parent.clone();
            while let Some(e) = env {
                if e.borrow().vars.contains_key(name) {
                    bind_name(&mut e.borrow_mut().vars, name, val);
                    return;
                }
                let parent = e.borrow().parent.clone();
                env = parent;
            }
            // No binding found up the chain: fall back to the immediate parent.
            let parent = cur.borrow().parent.clone();
            if let Some(p) = parent {
                bind_name(&mut p.borrow_mut().vars, name, val);
                return;
            }
        }
        // Module scope is the only env with no parent. Test that, not
        // `frames.len() == 1`: a generator/coroutine runs on an isolated
        // single-frame stack, so the length test would wrongly route its locals
        // to globals (invisible to an `UnboundLocalError`-aware local read).
        let cur = self.cur_env();
        if cur.borrow().parent.is_none() {
            bind_name(self.globals_mut(), name, val);
        } else {
            bind_name(&mut cur.borrow_mut().vars, name, val);
        }
    }

    pub fn set_global(&mut self, name: &str, val: Value) {
        bind_name(self.globals_mut(), name, val);
    }

    /// A `list`/`tuple` of `str` flattened to owned `String`s — reads a module's
    /// `__all__`. Errors if it is not a sequence of strings.
    fn str_sequence(&self, v: &Value) -> Result<Vec<String>, String> {
        match self.get(v) {
            Some(PyObj::List(items)) | Some(PyObj::Tuple(items)) => items
                .iter()
                .map(|it| {
                    self.as_str(it)
                        .ok_or_else(|| type_error("__all__ must contain only strings"))
                })
                .collect(),
            _ => Err(type_error("__all__ must be a list or tuple of str")),
        }
    }

    /// The names `from <module> import *` binds: the module's `__all__` if it
    /// defines one, otherwise every namespace entry whose name does not begin with
    /// `_`. Returns `(name, value)` pairs for the caller to bind.
    pub fn import_star_bindings(&mut self, module: &Value) -> Result<Vec<(String, Value)>, String> {
        // Copy the bridge handle out before the namespace clone below borrows
        // the heap; `foreign_id` is always `None` on a native-only build.
        let foreign = self.foreign_id(module);
        match self
            .module_slot(module)
            .map(|s| self.module_globals[s].clone())
        {
            Some(ns) => {
                if let Some(all) = ns.get("__all__").cloned() {
                    let mut out = Vec::new();
                    for n in self.str_sequence(&all)? {
                        if let Some(v) = ns.get(&n) {
                            out.push((n, v.clone()));
                        }
                    }
                    Ok(out)
                } else {
                    Ok(ns
                        .iter()
                        .filter(|(k, _)| !k.starts_with('_'))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect())
                }
            }
            // Not a native module. A CPython module over the ffi bridge is
            // still a valid target: honor its `__all__` (the stdlib modules
            // reached by star-import all define one).
            _ => match foreign {
                #[cfg(feature = "stdlib-ffi")]
                Some(id) => {
                    let all = crate::ffi::get_attr(self, id, "__all__").map_err(|_| {
                        type_error(
                            "'import *' from this module needs an __all__ it does not define",
                        )
                    })?;
                    let mut out = Vec::new();
                    for n in self.str_sequence(&all)? {
                        if let Ok(v) = crate::ffi::get_attr(self, id, &n) {
                            out.push((n, v));
                        }
                    }
                    Ok(out)
                }
                // Native-only build: `crate::ffi` is not compiled, and the
                // always-`None` `foreign_id` above means nothing reaches here.
                // The arm exists only to keep the match exhaustive.
                #[cfg(not(feature = "stdlib-ffi"))]
                Some(_) => Err(type_error("'import *' requires a module object")),
                None => Err(type_error("'import *' requires a module object")),
            },
        }
    }

    /// Remove a module global, returning its value if bound. Used by `eval()` to
    /// reclaim the temporary that captured the evaluated expression's value.
    pub fn del_global(&mut self, name: &str) -> Option<Value> {
        self.globals_mut().shift_remove(name)
    }

    /// A clone of the whole module-global namespace — the save half of the
    /// save/replace/run/restore used by `eval`/`exec` with an explicit `globals`
    /// dict, so the caller's real globals are untouched by the evaluated code.
    pub fn snapshot_globals(&self) -> NameMap {
        self.globals().clone()
    }

    /// Replace the module-global namespace wholesale (the restore/replace half of
    /// the `eval`/`exec` explicit-namespace flow). Builtins resolve through a
    /// separate registry, so they remain available regardless of this map.
    pub fn replace_globals(&mut self, g: NameMap) {
        *self.globals_mut() = g;
    }

    /// The module globals as `(name, value)` pairs — lets `eval`/`exec` copy the
    /// post-run namespace back into a caller-supplied `globals` dict.
    pub fn globals_pairs(&self) -> Vec<(String, Value)> {
        self.globals()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Module-level binding names plus defined class names — the dynamic half of
    /// REPL tab completion (user variables, defs, imports, classes). Top-level
    /// `def`/`class` bind into `globals`; class names are also unioned in so a
    /// class defined and immediately referenced still completes.
    pub fn global_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.globals().keys().cloned().collect();
        names.extend(self.classes.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    /// Attribute / method completion candidates for `base.<partial>`, where
    /// `base` is a module-global name. Returns `None` when `base` is unbound or
    /// its runtime type carries no completable attribute surface — the caller
    /// then falls back to plain word completion.
    pub fn attr_completions(&self, base: &str) -> Option<AttrCompletion> {
        let v = self.read_global(base)?;
        // Scalars carried inline in the `Value` (not on the object heap).
        match &v {
            Value::Bool(_) | Value::Int(_) => return Some(AttrCompletion::BuiltinType("int")),
            Value::Float(_) => return Some(AttrCompletion::BuiltinType("float")),
            Value::Str(_) => return Some(AttrCompletion::BuiltinType("str")),
            Value::Obj(_) => {}
            _ => return None,
        }
        match self.get(&v)? {
            PyObj::Str(_) => Some(AttrCompletion::BuiltinType("str")),
            PyObj::Bytes(_) | PyObj::Bytearray(_) => Some(AttrCompletion::BuiltinType("bytes")),
            PyObj::List(_) => Some(AttrCompletion::BuiltinType("list")),
            PyObj::Tuple(_) => Some(AttrCompletion::BuiltinType("tuple")),
            PyObj::Dict(_) => Some(AttrCompletion::BuiltinType("dict")),
            PyObj::Set(_) => Some(AttrCompletion::BuiltinType("set")),
            PyObj::Frozenset(_) => Some(AttrCompletion::BuiltinType("frozenset")),
            PyObj::BigInt(_) => Some(AttrCompletion::BuiltinType("int")),
            // A module (`import math`) → its own namespace members.
            PyObj::Module { slot, .. } => Some(AttrCompletion::Names(
                self.module_globals[*slot].keys().cloned().collect(),
            )),
            // A user instance → its instance attributes plus every method /
            // class attribute reachable along the MRO.
            PyObj::Instance(i) => {
                let class = i.class.clone();
                let dict = i.dict.clone();
                let mut names = self.inst_attr_names(&dict);
                if let Some(c) = self.classes.get(&class) {
                    for cls in &c.mro {
                        if let Some(cd) = self.classes.get(cls) {
                            names.extend(cd.ns.keys().cloned());
                        }
                    }
                }
                names.sort();
                names.dedup();
                Some(AttrCompletion::Names(names))
            }
            _ => None,
        }
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
        if self.globals_mut().shift_remove(name).is_some() {
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

thread_local! {
    /// Recycled VMs, keyed by the chunk they already hold.
    ///
    /// EVERY Python function call runs its body on a VM, and building one is far
    /// from free: `VM::new` allocates the stack/frame/globals vectors and
    /// `builtins::install` registers ~80 builtin handlers — per call. `fib(27)`
    /// paid that 400k times. `VM::reset` clears only execution state and keeps
    /// the builtin table, the numeric hook, the JIT flag, and every vector's
    /// capacity.
    ///
    /// Keyed by `op_hash` rather than one flat pool so a repeat call to the same
    /// function gets a VM that ALREADY holds its chunk: `run_chunk_on` hands that
    /// chunk straight back to `reset` (a move, not a copy), so the bytecode is
    /// cloned once per function instead of once per call. Each key holds a stack
    /// of VMs, so recursion — which needs the same chunk live at several depths
    /// at once — just pops another one.
    /// Boxed: a `VM` is ~1.7 KB, and pooling it BY VALUE memcpy'd the whole
    /// struct out of the pool on the way in and back on the way out — nearly
    /// 4 KB of copying per Python call, which profiled as the single largest
    /// cost in call-heavy code. A `Box` makes both moves pointer-sized.
    ///
    /// `clippy::vec_box` argues the `Vec` already heap-allocates so the `Box` is
    /// redundant. That is true of the *storage* and false of the *moves*, which
    /// are what cost here: pushing and popping a bare `VM` memcpys the struct,
    /// and this pool is on the hot call path. Kept deliberately, per the
    /// measurement above.
    #[allow(clippy::vec_box)]
    static VM_POOL: RefCell<HashMap<u64, Vec<Box<VM>>>> = RefCell::new(HashMap::new());
}

/// Register every pythonrs builtin + the numeric hook on a VM, then run it.
///
/// This is the ONE-SHOT path — the module body, a generator resume, an `exec`.
/// It builds a VM and drops it, which is the right trade when the chunk runs
/// once: pooling a chunk that is never re-entered only adds bookkeeping, and it
/// measurably slowed container-heavy top-level code. Repeated calls to the same
/// function go through `run_chunk_cached` instead.
pub fn run_chunk_on(chunk: Chunk) -> Result<Value, String> {
    finish_run(Box::new(new_configured_vm(chunk)), None)
}

/// Build a VM with pythonrs's builtins, numeric hook, and JIT/debug wiring.
fn new_configured_vm(chunk: Chunk) -> VM {
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
    vm
}

/// Run `vm` to completion and translate the outcome, recycling it into the pool
/// under `pool_key` when one is given.
fn finish_run(mut vm: Box<VM>, pool_key: Option<u64>) -> Result<Value, String> {
    let outcome = vm.run();
    let halted_top = matches!(outcome, VMResult::Halted)
        .then(|| vm.stack.last().cloned().unwrap_or(Value::Undef));
    if let VMResult::Error(_) = &outcome {
        // A native fast-path op (Add/Sub/Mul/Negate/comparisons) raised via the
        // VM directly, not through a pythonrs builtin's `abort`, so the failing
        // op's line/caret span was never recorded. Capture it here from the
        // still-valid `ip` before unwinding.
        crate::builtins::record_err_line(&vm);
    }
    if let Some(key) = pool_key {
        // A suspended generator parks its VM on the coroutine stack instead — it
        // only reaches here once `run` has returned, so a recycled VM is never
        // one some frame is still executing on.
        //
        // The VM moves straight into the pool. Taking it by `&mut` meant building
        // a fresh `VM` — which installs every builtin and allocates its stack,
        // frames and globals — purely as a placeholder to swap out and drop, on
        // EVERY call that returned one. That construct-and-discard was the single
        // largest cost in a call-heavy program.
        VM_POOL.with(|p| p.borrow_mut().entry(key).or_default().push(vm));
    }
    if let Some(e) = with_host(|h| h.take_error()) {
        return Err(e);
    }
    match outcome {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(halted_top.unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// The same, but the chunk is produced only if the pool has no VM holding it.
///
/// `key` is the callee's `def_id` — an index into the host's function table,
/// which `load_program` only ever EXTENDS, so ids are stable and unique for the
/// life of a host. `reset_host` clears the pool along with that table.
///
/// Keying by `Chunk::op_hash` instead does not work: it is `serde(skip)` (0 for
/// any deserialized chunk) and covers only ops and constants, not the name pool,
/// so two same-shaped functions from different modules would share a bucket and
/// one would run with the other's globals.
/// so on a hit there is nothing to build: the caller's `make` is never called and
/// no bytecode is copied. That is what turns a Python call from "clone the
/// function body, run it, drop the copy" into "reset a VM and run".
pub fn run_chunk_cached(key: u64, make: impl FnOnce() -> Chunk) -> Result<Value, String> {
    let pooled = VM_POOL.with(|p| p.borrow_mut().get_mut(&key).and_then(|v| v.pop()));
    let vm = match pooled {
        Some(mut vm) => {
            // Hand the VM its own chunk straight back — `reset` moves it, so this
            // is a pointer swap rather than a copy of the bytecode.
            let own = std::mem::take(&mut vm.chunk);
            vm.reset(own);
            vm
        }
        None => Box::new(new_configured_vm(make())),
    };
    finish_run(vm, Some(key))
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
/// The name an `AttributeError` uses for a value's type. This is CPython's
/// `tp_name`, which for the C-accelerated `collections` containers is
/// module-qualified even though their `__name__` is not: `deque().__name__` is
/// `deque` but the error says `'collections.deque' object`. The pure-Python
/// `Counter` is NOT qualified, so this is a short explicit list rather than a
/// blanket "prefix everything in collections".
fn attr_error_type_name(tn: &str) -> String {
    match tn {
        "deque" | "OrderedDict" | "defaultdict" => format!("collections.{tn}"),
        _ => tn.to_string(),
    }
}

fn type_object_class_name(n: &str) -> Option<String> {
    // Module-qualified stdlib types.
    let qualified = match n {
        "Counter" => Some("collections.Counter"),
        "defaultdict" => Some("collections.defaultdict"),
        "OrderedDict" => Some("collections.OrderedDict"),
        "deque" => Some("collections.deque"),
        "partial" => Some("functools.partial"),
        "TextIOWrapper" => Some("_io.TextIOWrapper"),
        "BufferedReader" => Some("_io.BufferedReader"),
        "BufferedWriter" => Some("_io.BufferedWriter"),
        "BufferedRandom" => Some("_io.BufferedRandom"),
        // `type_name` already returns these fully qualified.
        "functools._lru_cache_wrapper" => Some("functools._lru_cache_wrapper"),
        "re.Pattern" => Some("re.Pattern"),
        "os.stat_result" => Some("os.stat_result"),
        _ if n
            .strip_prefix("_io.")
            .is_some_and(|t| crate::stdlib::pyio::STREAM_TYPES.contains(&t)) =>
        {
            Some(n)
        }
        "re.Match" => Some("re.Match"),
        // Since 3.14 the PEP 604 union type IS `typing.Union` — `__name__` is
        // `Union`, `__module__` is `typing`, and messages name it
        // `'typing.Union' object …`. It is no longer `builtins.UnionType`.
        "typing.Union" => Some("typing.Union"),
        "string.templatelib.Template" => Some("string.templatelib.Template"),
        "string.templatelib.Interpolation" => Some("string.templatelib.Interpolation"),
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
    if !matches!(mname, "math" | "collections" | "functools" | "contextlib") {
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
                Some(PyObj::StructFmt(_)) => "Struct".into(),
                Some(PyObj::ContextVar { .. }) => "ContextVar".into(),
                Some(PyObj::ContextToken { .. }) => "Token".into(),
                Some(PyObj::ContextObj) => "Context".into(),
                Some(PyObj::Unbound) => "unbound".into(),
                Some(PyObj::CsvWriter { .. }) => "_csv.writer".into(),
                Some(PyObj::CsvDialect(_)) => "Dialect".into(),
                Some(PyObj::CsvReader { .. }) => "_csv.reader".into(),
                Some(PyObj::Hasher { algo, .. }) => (*algo).name().into(),
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
                Some(PyObj::Range { .. }) | Some(PyObj::BigRange { .. }) => "range".into(),
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
                // Every builtin container has its own iterator type in CPython;
                // the snapshot cursor carries which one it is standing in for.
                Some(PyObj::Iter(IterState::Seq { kind, .. })) => kind.type_name().into(),
                Some(PyObj::Iter(IterState::RangeIter { .. })) => "range_iterator".into(),
                Some(PyObj::Iter(IterState::BigRangeIter { .. })) => "longrange_iterator".into(),
                Some(PyObj::Iter(IterState::DictKeys { .. })) => "dict_keyiterator".into(),
                Some(PyObj::Zip { .. }) => "zip".into(),
                Some(PyObj::MapObj { .. }) => "map".into(),
                Some(PyObj::FilterObj { .. }) => "filter".into(),
                Some(PyObj::EnumerateObj { .. }) => "enumerate".into(),
                Some(PyObj::CallIter { .. }) => "callable_iterator".into(),
                Some(PyObj::Module { .. }) => "module".into(),
                // A `__dict__` view IS a dict as far as Python can tell.
                Some(PyObj::ModuleDict { .. }) => "dict".into(),
                Some(PyObj::BytesIO { .. }) => "_io.BytesIO".into(),
                Some(PyObj::StringIO { .. }) => "_io.StringIO".into(),
                Some(PyObj::Template { .. }) => "string.templatelib.Template".into(),
                Some(PyObj::Interpolation { .. }) => "string.templatelib.Interpolation".into(),
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
                Some(PyObj::File { id }) => self.file_class_name(*id).into(),
                Some(PyObj::Deque { .. }) => "deque".into(),
                Some(PyObj::NamedTupleType { .. }) => "type".into(),
                Some(PyObj::Partial { .. }) => "partial".into(),
                Some(PyObj::Code { .. }) => "code".into(),
                Some(PyObj::Union { .. }) => "typing.Union".into(),
                Some(PyObj::GenericAlias { .. }) => "GenericAlias".into(),
                Some(PyObj::TypeVarLike { kind, .. }) => kind.type_name().into(),
                Some(PyObj::StructTime { .. }) => "struct_time".into(),
                Some(PyObj::Pattern { .. }) => "re.Pattern".into(),
                Some(PyObj::Match { .. }) => "re.Match".into(),
                Some(PyObj::Namespace { .. }) => "SimpleNamespace".into(),
                Some(PyObj::MappingProxy { .. }) => "mappingproxy".into(),
                Some(PyObj::Descriptor { kind, .. }) => kind.type_name().into(),
                Some(PyObj::Traceback { .. }) => "traceback".into(),
                Some(PyObj::PyFrame { .. }) => "frame".into(),
                Some(PyObj::FrameCode { .. }) => "code".into(),
                Some(PyObj::Cell { .. }) => "cell".into(),
                Some(PyObj::Lock { reentrant, .. }) => {
                    if *reentrant { "RLock" } else { "lock" }.into()
                }
                Some(PyObj::ItertoolsIter { kind, .. }) => kind.type_name().into(),
                Some(PyObj::LruCache { .. }) => "functools._lru_cache_wrapper".into(),
                Some(PyObj::Super { .. }) => "super".into(),
                Some(PyObj::StaticMethod(_)) => "staticmethod".into(),
                Some(PyObj::ClassMethod(_)) => "classmethod".into(),
                Some(PyObj::Property { .. }) => "property".into(),
                Some(PyObj::CachedProperty { .. }) => "cached_property".into(),
                Some(PyObj::Redirect { stderr, .. }) => {
                    if *stderr {
                        "redirect_stderr".into()
                    } else {
                        "redirect_stdout".into()
                    }
                }
                Some(PyObj::NotImplemented) => "NotImplementedType".into(),
                Some(PyObj::Ellipsis) => "ellipsis".into(),
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
                Some(PyObj::BigRange { start, stop, step }) => {
                    use num_traits::Zero;
                    !big_range_len(start, stop, step).is_zero()
                }
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
                Some(PyObj::StructFmt(f)) => format!("<_struct.Struct object, format '{f}'>"),
                Some(PyObj::BytesIO { .. }) => "<_io.BytesIO object>".to_string(),
                Some(PyObj::StringIO { .. }) => "<_io.StringIO object>".to_string(),
                Some(PyObj::ContextVar { name, .. }) => format!("<ContextVar name='{name}'>"),
                Some(PyObj::ContextToken { .. }) => "<Token>".to_string(),
                Some(PyObj::ContextObj) => "<Context>".to_string(),
                Some(PyObj::Unbound) => "<unbound local>".to_string(),
                Some(PyObj::CsvWriter { .. }) => "<_csv.writer object>".to_string(),
                Some(PyObj::CsvDialect(_)) => "<_csv.Dialect object>".to_string(),
                Some(PyObj::CsvReader { .. }) => "<_csv.reader object>".to_string(),
                Some(PyObj::Hasher { algo, .. }) => {
                    format!("<{} _hashlib.HASH object>", algo.name())
                }
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
                Some(PyObj::Code { def_id }) => {
                    let name = self
                        .funcs
                        .get(*def_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    format!("<code object {name} at 0x0000000000000000, file \"<string>\", line 1>")
                }
                Some(PyObj::TypeVarLike { name, .. }) => name.clone(),
                Some(PyObj::StructTime { fields }) => {
                    let fields = fields.clone();
                    let parts: Vec<String> = STRUCT_TIME_FIELDS
                        .iter()
                        .zip(fields.iter())
                        .filter(|(_, v)| !matches!(v, Value::Undef))
                        .map(|(name, v)| format!("{name}={}", self.repr_of(v)))
                        .collect();
                    format!("time.struct_time({})", parts.join(", "))
                }
                Some(PyObj::Pattern { pattern, .. }) => {
                    // CPython truncates a long pattern in the repr.
                    let shown: String = pattern.chars().take(200).collect();
                    let q = shown.replace('\\', "\\\\").replace('\'', "\\'");
                    format!("re.compile('{q}')")
                }
                Some(PyObj::Match { text, spans, .. }) => {
                    let (s, e) = spans.first().copied().flatten().unwrap_or((0, 0));
                    let matched = text.get(s..e).unwrap_or("");
                    let q = matched.replace('\\', "\\\\").replace('\'', "\\'");
                    // The repr renders the group-0 span, so it is a position
                    // boundary like `span()` and reports codepoints too.
                    let (s, e) = (
                        crate::regexpr::char_index_of(text, s),
                        crate::regexpr::char_index_of(text, e),
                    );
                    format!("<re.Match object; span=({s}, {e}), match='{q}'>")
                }
                Some(PyObj::Union { args }) => {
                    let args = args.clone();
                    args.iter()
                        .map(|a| self.union_member_name(a))
                        .collect::<Vec<_>>()
                        .join(" | ")
                }
                Some(PyObj::GenericAlias { origin, args }) => {
                    let (origin, args) = (origin.clone(), args.clone());
                    let inner = args
                        .iter()
                        .map(|a| self.generic_arg_name(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{}[{inner}]", self.generic_arg_name(&origin))
                }
                Some(PyObj::Namespace { attrs }) => {
                    let attrs: Vec<(String, Value)> =
                        attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let inner = attrs
                        .iter()
                        .map(|(k, v)| format!("{k}={}", self.repr_of(v)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("namespace({inner})")
                }
                Some(PyObj::MappingProxy { dict }) => {
                    let dict = dict.clone();
                    format!("mappingproxy({})", self.repr_of(&dict))
                }
                Some(PyObj::Descriptor { kind, qual }) => {
                    let (kind, qual) = (*kind, qual.clone());
                    let (owner, name) = qual.split_once('.').unwrap_or(("", qual.as_str()));
                    match kind {
                        DescKind::MethodWrapper => {
                            format!("<method-wrapper '{name}' of object>")
                        }
                        DescKind::GetSetDescriptor | DescKind::MemberDescriptor => {
                            format!("<attribute '{name}' of '{owner}' objects>")
                        }
                        // CPython calls a wrapper descriptor a "slot wrapper" in
                        // its repr, even though its type is `wrapper_descriptor`.
                        DescKind::WrapperDescriptor => {
                            format!("<slot wrapper '{name}' of '{owner}' objects>")
                        }
                        _ => format!("<{} '{name}' of '{owner}' objects>", kind.type_name()),
                    }
                }
                Some(PyObj::Traceback { .. }) => {
                    format!("<traceback object at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::FrameCode { name, .. }) => format!("<code object {name}>"),
                Some(PyObj::PyFrame { name, lineno }) => {
                    format!(
                        "<frame at 0x{:012x}, file '<string>', line {lineno}, code {name}>",
                        self.addr_of(v)
                    )
                }
                Some(PyObj::Cell { value }) => {
                    let value = value.clone();
                    if matches!(value, Value::Undef) {
                        format!("<cell at 0x{:012x}: empty>", self.addr_of(v))
                    } else {
                        format!(
                            "<cell at 0x{:012x}: {} object at 0x{:012x}>",
                            self.addr_of(v),
                            self.type_name(&value),
                            self.addr_of(&value)
                        )
                    }
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
                Some(PyObj::Template {
                    strings,
                    interpolations,
                }) => {
                    let ss: Vec<String> = strings.iter().map(|s| quote_str(s)).collect();
                    let is: Vec<String> = interpolations.iter().map(|v| self.repr_of(v)).collect();
                    format!(
                        "Template(strings=({}), interpolations=({}))",
                        tuple_body(&ss),
                        tuple_body(&is)
                    )
                }
                Some(PyObj::Interpolation {
                    value,
                    expression,
                    conversion,
                    format_spec,
                }) => format!(
                    "Interpolation({}, {}, {}, {})",
                    self.repr_of(value),
                    quote_str(expression),
                    match conversion {
                        Some(c) => quote_str(&c.to_string()),
                        None => "None".to_string(),
                    },
                    quote_str(format_spec)
                ),
                Some(PyObj::ModuleDict { slot }) => {
                    let items: Vec<String> = self.module_globals[*slot]
                        .iter()
                        .map(|(k, v)| format!("{}: {}", quote_str(k), self.repr_of(v)))
                        .collect();
                    format!("{{{}}}", items.join(", "))
                }
                Some(PyObj::Range { start, stop, step }) => {
                    if *step == 1 {
                        format!("range({start}, {stop})")
                    } else {
                        format!("range({start}, {stop}, {step})")
                    }
                }
                Some(PyObj::BigRange { start, stop, step }) => {
                    use num_traits::One;
                    if step.is_one() {
                        format!("range({start}, {stop})")
                    } else {
                        format!("range({start}, {stop}, {step})")
                    }
                }
                Some(PyObj::Iter(_)) => "<iterator>".into(),
                Some(PyObj::Zip { .. }) => format!("<zip object at 0x{:012x}>", self.addr_of(v)),
                // `count` and `repeat` are the two itertools iterators CPython
                // gives a constructor-style repr (`count(5, 2)`, `repeat('x', 2)`)
                // instead of the generic `<... object at 0x...>`; both report LIVE
                // state, so a partly-consumed `repeat('x', 3)` reprs as
                // `repeat('x', 2)`. `count`'s step is omitted when it is 1, and
                // `repeat`'s count is omitted when it is unbounded.
                Some(PyObj::ItertoolsIter {
                    kind: ItKind::Count,
                    buf,
                    ..
                }) => {
                    let (cur, step) = (buf[0].clone(), buf[1].clone());
                    let one = matches!(step, Value::Int(1));
                    if one {
                        format!("count({})", self.repr_of(&cur))
                    } else {
                        format!("count({}, {})", self.repr_of(&cur), self.repr_of(&step))
                    }
                }
                Some(PyObj::ItertoolsIter {
                    kind: ItKind::Repeat,
                    nums,
                    buf,
                    ..
                }) => {
                    let (obj, left) = (buf[0].clone(), nums[0]);
                    if left < 0 {
                        format!("repeat({})", self.repr_of(&obj))
                    } else {
                        format!("repeat({}, {left})", self.repr_of(&obj))
                    }
                }
                Some(PyObj::ItertoolsIter { kind, .. }) => {
                    format!(
                        "<{} object at 0x{:012x}>",
                        kind.type_name(),
                        self.addr_of(v)
                    )
                }
                Some(PyObj::Lock { count, reentrant }) => {
                    let state = if *count > 0 { "locked" } else { "unlocked" };
                    let kind = if *reentrant {
                        "_thread.RLock"
                    } else {
                        "_thread.lock"
                    };
                    format!("<{state} {kind} object at 0x{:012x}>", self.addr_of(v))
                }
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
                Some(PyObj::CachedProperty { .. }) => "<functools.cached_property object>".into(),
                Some(PyObj::Redirect { stderr, .. }) => {
                    let nm = if *stderr {
                        "redirect_stderr"
                    } else {
                        "redirect_stdout"
                    };
                    format!("<contextlib.{nm} object at 0x{:012x}>", self.addr_of(v))
                }
                Some(PyObj::NotImplemented) => "NotImplemented".into(),
                Some(PyObj::Ellipsis) => "Ellipsis".into(),
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(id)) => pending_display_get(*id)
                    .map(|(s, _)| s)
                    .unwrap_or_else(|| crate::ffi::str_of(*id)),
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
                    let meta = match v {
                        Value::Obj(i) => self.dict_meta.get(i),
                        _ => None,
                    };
                    // A Counter reprs in `most_common()` order, not insertion
                    // order: CPython's `Counter.__repr__` is
                    // `f'Counter({dict(self.most_common())!r})'`. Descending by
                    // count, stable, so equal counts keep insertion order.
                    let mut entries: Vec<&(Value, Value)> = d.values().collect();
                    if meta.map(|m| m.kind) == Some(DictKind::Counter) {
                        entries.sort_by(|a, b| self.count_order(&b.1, &a.1));
                    }
                    let body: Vec<String> = entries
                        .iter()
                        .map(|(k, val)| format!("{}: {}", self.repr_of(k), self.repr_of(val)))
                        .collect();
                    let dict_repr = format!("{{{}}}", body.join(", "));
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
                Some(PyObj::Foreign(id)) => pending_display_get(*id)
                    .map(|(_, r)| r)
                    .unwrap_or_else(|| crate::ffi::repr_of(*id)),
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
                    let mut ks: Vec<PKey> = if s.keys().any(pkey_is_value_keyed) {
                        // A value-keyed element's stored key carries the heap id
                        // of the object it collapsed onto WHEN THE FROZENSET WAS
                        // BUILT, which is nothing the destination container knows
                        // about. Recompute it so the `prepare_key` collapse
                        // against that container applies — otherwise two equal
                        // frozensets are two different dict slots.
                        s.values()
                            .map(|e| self.to_key(e))
                            .collect::<Result<_, String>>()?
                    } else {
                        s.keys().cloned().collect()
                    };
                    ks.sort();
                    ks.dedup();
                    PKey::Frozenset(ks)
                }
                // A type object keys by name (types are singletons by name).
                Some(PyObj::Class(n)) => PKey::Class(n.clone()),
                Some(PyObj::Builtin(n)) => PKey::Class(n.clone()),
                // A C-level descriptor (`dict.__repr__`) keys by what it names,
                // not by heap id: CPython caches one object per type/slot, so
                // `dict.__repr__` is the same object every time it is read, while
                // this runtime builds a fresh one per access. `pprint` fills its
                // dispatch table with `_dispatch[dict.__repr__] = ...` and then
                // looks up `type(obj).__repr__`, which is a DIFFERENT read of the
                // same slot — hashing by id would never find the entry.
                Some(PyObj::Descriptor { kind, qual }) => {
                    PKey::Class(format!("{}:{qual}", kind.type_name()))
                }
                // Functions/methods/other callables hash by identity (heap id).
                Some(
                    PyObj::Func(_)
                    | PyObj::BoundMethod { .. }
                    | PyObj::StaticMethod(_)
                    | PyObj::ClassMethod(_)
                    | PyObj::Module { .. }
                    | PyObj::Code { .. }
                    | PyObj::Lock { .. }
                    | PyObj::TypeVarLike { .. },
                ) => {
                    let id = match v {
                        Value::Obj(i) => *i,
                        _ => 0,
                    };
                    PKey::Instance {
                        hash: id as i64,
                        id,
                    }
                }
                Some(PyObj::Ellipsis) => PKey::Singleton(0),
                Some(PyObj::NotImplemented) => PKey::Singleton(1),
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
                    } else if self.implicit_hash_none(&class) {
                        // Shadows any inherited `__hash__`; see
                        // `implicit_hash_none`.
                        return Err(type_error(&format!("unhashable type: '{class}'")));
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
                // A CPython Foreign object as a key: a value-equal collapse
                // resolved by `prepare_key` (which ran `ffi::foreign_eq` outside
                // the borrow) wins; otherwise key by CPython's own hash with this
                // object's own heap id. Same-handle lookups match directly; a
                // fresh value-equal handle is collapsed by `prepare_key` at the
                // container op, exactly like the `Instance` path.
                #[cfg(feature = "stdlib-ffi")]
                Some(PyObj::Foreign(fid)) => {
                    let id = match v {
                        Value::Obj(i) => *i,
                        _ => 0,
                    };
                    if let Some(k) = pending_key_get(id) {
                        k
                    } else {
                        let hash = crate::ffi::foreign_hash(*fid)?;
                        PKey::Foreign { hash, id }
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
        // Depth-guarded: two DIFFERENT self-referential containers recurse
        // without end, and aborting the process is not an answer. See
        // `EQUAL_DEPTH`.
        let depth = EQUAL_DEPTH.with(|d| {
            let n = d.get() + 1;
            d.set(n);
            n
        });
        let r = if depth > EQUAL_DEPTH_LIMIT {
            EQUAL_OVERFLOW.with(|c| c.set(true));
            false
        } else {
            self.equal_inner(a, b)
        };
        EQUAL_DEPTH.with(|d| d.set(d.get() - 1));
        r
    }

    /// One ELEMENT of a container comparison. CPython's `PyObject_RichCompareBool`
    /// shortcuts on identity first, which is why `a = [1]; a.append(a); a == a`
    /// is `True` rather than an endless walk of the cycle — the third element IS
    /// the list being compared.
    fn elem_equal(&self, p: &Value, q: &Value) -> bool {
        matches!((p, q), (Value::Obj(i), Value::Obj(j)) if i == j) || self.equal(p, q)
    }

    fn equal_inner(&self, a: &Value, b: &Value) -> bool {
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
            (Value::Int(x), Value::Int(y)) => x == y,
            _ => {
                // Integers compare EXACTLY, at any size. Routing them through f64
                // made any two integers within one ULP of each other equal — for
                // 29-digit values that is a gap of billions. `_pydecimal.sqrt` ends
                // with `exact = n*n == c`, so `Decimal(2).sqrt()` took the "exact"
                // branch on a wrong `n` and returned 1.
                if self.is_bignum(a) || self.is_bignum(b) {
                    if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                        return x == y;
                    }
                }
                // An integer equals a float only when the float is EXACTLY that
                // integer — never by rounding both to the same double. Not a
                // bignum-only concern: `3**34` fits an `i64` and still has no
                // `f64`, so `16677181699666569 == 16677181699666568.0` answered
                // True until the pair was compared in the integer domain.
                if let Some((x, f, _)) = self.rounding_int_float_pair(a, b) {
                    return exact_int_cmp_float(&x, f) == Some(std::cmp::Ordering::Equal);
                }
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
                // A `dict_keys`/`dict_items` view compares by membership against
                // a set/frozenset or another view (`d.keys() == {1, 2}`).
                if self.either_is_view(a, b) {
                    if let (Some(x), Some(y)) = (self.view_keyset(a), self.view_keyset(b)) {
                        let ys: HashSet<&PKey> = y.iter().collect();
                        return x.len() == y.len() && x.iter().all(|k| ys.contains(k));
                    }
                }
                match (self.get(a), self.get(b)) {
                    (Some(PyObj::Str(x)), Some(PyObj::Str(y))) => x == y,
                    (Some(PyObj::List(x)), Some(PyObj::List(y)))
                    | (Some(PyObj::Tuple(x)), Some(PyObj::Tuple(y))) => {
                        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.elem_equal(p, q))
                    }
                    (Some(PyObj::Dict(x)), Some(PyObj::Dict(y))) => {
                        x.len() == y.len()
                            && x.iter().all(|(k, (_, xv))| {
                                y.get(k)
                                    .map(|(_, yv)| self.elem_equal(xv, yv))
                                    .unwrap_or(false)
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
                        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.elem_equal(p, q))
                    }
                    // Two unions are equal iff they hold the same members, order
                    // irrelevant: `int | str == str | int`, as in CPython.
                    (Some(PyObj::Union { args: x }), Some(PyObj::Union { args: y })) => {
                        x.len() == y.len() && x.iter().all(|p| y.iter().any(|q| self.equal(p, q)))
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
                    // Singletons compare equal to themselves regardless of heap
                    // identity (`... == ...`, and `lst.count(...)`).
                    (Some(PyObj::Ellipsis), Some(PyObj::Ellipsis))
                    | (Some(PyObj::NotImplemented), Some(PyObj::NotImplemented)) => true,
                    // Two CPython `Foreign` objects (stdlib-ffi): defer to CPython's
                    // own identity-then-`__eq__`, so an enum member `in (A, B)`,
                    // `(A, B).index(member)`, `list.count(member)`, and a list/tuple
                    // `==` holding foreign elements all match CPython. Two distinct
                    // handles onto the same singleton (`S.A` fetched twice) are `is`
                    // and `==` equal there; the raw `a == b` handle-id fallback below
                    // would wrongly call them unequal.
                    #[cfg(feature = "stdlib-ffi")]
                    (Some(PyObj::Foreign(x)), Some(PyObj::Foreign(y))) => {
                        crate::ffi::foreign_eq(*x, *y)
                    }
                    // A CPython Foreign object vs a native scalar: CPython's own
                    // `__eq__`, so `IntEnum.HIGH == 3` / `Decimal('1.5') == 1.5`
                    // hold inside `in` / `.index` / `.count` / list `==`.
                    #[cfg(feature = "stdlib-ffi")]
                    (Some(PyObj::Foreign(f)), _) => self.foreign_eq_native(*f, b),
                    #[cfg(feature = "stdlib-ffi")]
                    (_, Some(PyObj::Foreign(f))) => self.foreign_eq_native(*f, a),
                    _ => match (a, b) {
                        (Value::Str(x), Value::Str(y)) => x == y,
                        _ => a == b,
                    },
                }
            }
        }
    }

    /// `foreign == native-scalar` via CPython's `__eq__` (borrow-free bridge).
    /// `other` is the non-Foreign operand; an `int`/`bool` compares as an int, any
    /// other number as a float, a `str` as a str. A non-scalar native (list, dict,
    /// user instance) has no scalar form here and defaults unequal — CPython would
    /// consult the object's `__eq__`, but that path is not reachable borrow-free.
    #[cfg(feature = "stdlib-ffi")]
    fn foreign_eq_native(&self, fid: u32, other: &Value) -> bool {
        if let Some(n) = self.as_int(other) {
            crate::ffi::foreign_eq_prim(fid, crate::ffi::Prim::Int(n))
        } else if let Some(f) = self.num_val(other) {
            crate::ffi::foreign_eq_prim(fid, crate::ffi::Prim::Float(f))
        } else if let Some(s) = self.as_str(other) {
            crate::ffi::foreign_eq_prim(fid, crate::ffi::Prim::Str(&s))
        } else {
            false
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

    /// Whether `v` is an integer whose magnitude lives in a heap bignum — the
    /// only way an `int` can be past the `f64` range. An `int` SUBCLASS counts:
    /// it carries the same payload one level down.
    fn is_bignum_like(&self, v: &Value) -> bool {
        if self.is_bignum(v) {
            return true;
        }
        match self.get(v) {
            Some(PyObj::Instance(_)) => {
                self.base_payload_num(v).is_some_and(|p| self.is_bignum(&p))
            }
            _ => false,
        }
    }

    /// A numeric operand as the `f64` a MIXED int/float ARITHMETIC operation
    /// must see, or the `OverflowError` CPython raises instead of converting.
    ///
    /// CPython reads an `int` operand through `PyLong_AsDouble`, which RAISES
    /// once the magnitude is past the `f64` range rather than saturating to
    /// `inf`. `num_val` saturates deliberately — it also backs comparison,
    /// where CPython never converts and `(2**2000) > 1.0` must stay `True` — so
    /// arithmetic needs this checked form. Reading the saturated value instead
    /// made `(2**2000) * 1.0` answer `inf` and `(2**2000) // 1.0` answer `nan`,
    /// silently-wrong numbers where CPython raises.
    ///
    /// Only an INTEGER operand raises: a `float` operand that already IS an
    /// infinity converts to itself (`float('inf') * 2.0` is `inf`, not an error).
    pub fn num_val_arith(&self, v: &Value) -> Result<Option<f64>, String> {
        let f = self.num_val(v);
        if matches!(f, Some(x) if !x.is_finite()) && self.is_bignum_like(v) {
            return Err("OverflowError: int too large to convert to float".into());
        }
        Ok(f)
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

    /// How `v` reads as a `Py_ssize_t`. [`PyHost::as_int`] collapses the two
    /// failures — "not an integer" and "an integer that does not fit" — into one
    /// `None`, and every index site then reported the first, so `[1][10**30]`
    /// was `TypeError: list indices must be integers or slices, not int` where
    /// CPython raises `IndexError: cannot fit 'int' into an index-sized
    /// integer`. CPython keeps them apart: `__index__` succeeds and
    /// `PyLong_AsSsize_t` is what overflows.
    pub fn index_fit(&self, v: &Value) -> IndexFit {
        if let Some(n) = self.as_int(v) {
            return IndexFit::Fits(n);
        }
        if matches!(v, Value::Obj(_)) {
            if let Some(PyObj::BigInt(b)) = self.get(v) {
                return IndexFit::TooLarge(b.sign() == num_bigint::Sign::Minus);
            }
        }
        IndexFit::NotInt
    }

    /// A sequence subscript's integer index.
    ///
    /// `not_int` builds the `TypeError` text for a subscript that is not an
    /// integer at all. One that IS an integer but does not fit `Py_ssize_t` is a
    /// different error in CPython: `PySequence_GetItem`/`SetItem`/`DelItem` call
    /// `PyNumber_AsSsize_t(key, PyExc_IndexError)`, so `[1][10**30]`,
    /// `l[10**30] = 2` and `del l[10**30]` all raise
    /// `IndexError: cannot fit 'int' into an index-sized integer`.
    pub fn seq_index(&self, idx: &Value, not_int: impl FnOnce() -> String) -> Result<i64, String> {
        match self.index_fit(idx) {
            IndexFit::Fits(n) => Ok(n),
            IndexFit::TooLarge(_) => Err(format!("IndexError: {INDEX_OVERFLOW}")),
            IndexFit::NotInt => Err(not_int()),
        }
    }

    /// `v` as a slice bound, saturating rather than raising.
    ///
    /// `_PyEval_SliceIndex` calls `PyNumber_AsSsize_t(v, NULL)`, and the NULL
    /// exception type means an overflow CLAMPS to `PY_SSIZE_T_MAX`/`MIN`
    /// instead of raising — which is why `'abc'[10**30:]` is `''` in CPython
    /// and `'abc'[::10**30]` is `'a'`. Reading the bound with
    /// [`PyHost::as_int`] answered `None` for a bignum, i.e. "bound omitted",
    /// so both of those silently returned the whole string.
    pub fn as_slice_index(&self, v: &Value) -> Option<i64> {
        match self.index_fit(v) {
            IndexFit::Fits(n) => Some(n),
            IndexFit::TooLarge(neg) => Some(if neg { i64::MIN } else { i64::MAX }),
            IndexFit::NotInt => None,
        }
    }

    /// `v` as an exact integer, but ONLY when its magnitude is past the point
    /// where `f64` stops holding every integer — a heap bignum always, an `i64`
    /// (or an `int`-subclass payload) only past 2^53. `None` means the plain
    /// `f64` reading of `v` is already exact.
    fn lossy_int_val(&self, v: &Value) -> Option<num_bigint::BigInt> {
        if matches!(v, Value::Obj(_)) {
            if let Some(PyObj::BigInt(b)) = self.get(v) {
                return Some(b.clone());
            }
        }
        self.as_int(v)
            .filter(|n| f64_would_round(*n))
            .map(num_bigint::BigInt::from)
    }

    /// A mixed integer/float pair, in either operand order, whose integer is too
    /// large to survive the trip through `f64` — the only pair shape that cannot
    /// be resolved by reading both operands as doubles. Yields the exact integer,
    /// the float, and whether the integer was the LEFT operand.
    ///
    /// `None` for every pair the `f64` route already answers exactly: two
    /// integers, two floats, a small integer against a float, a non-numeric.
    fn rounding_int_float_pair(
        &self,
        a: &Value,
        b: &Value,
    ) -> Option<(num_bigint::BigInt, f64, bool)> {
        if let Some(f) = self.float_val(b) {
            return self.lossy_int_val(a).map(|x| (x, f, true));
        }
        if let Some(f) = self.float_val(a) {
            return self.lossy_int_val(b).map(|x| (x, f, false));
        }
        None
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
// insertion order. For a set of plain machine ints that order is deterministic —
// the hash is `|n|` reduced modulo `2**61-1` with the sign reapplied, so it is
// the same on every run — and this table reproduces it for `set(iterable)`,
// for `.add()` in a loop, and for `frozenset`.
//
// It does NOT yet reproduce a set DISPLAY. A literal compiles to `BUILD_SET 0`
// + `LOAD_CONST frozenset({...})` + `SET_UPDATE`, and CPython's set-to-set
// update presizes the table with `(used + other->used) * 2`, so `{1,2,3,10,20}`
// gets 16 slots where five separate inserts get 8-then-32. The resulting order
// differs (`{1, 2, 3, 20, 10}` vs `{1, 2, 3, 10, 20}`); see BUGS.md.
//
// String hashes are per-process randomized in CPython (SipHash keyed by
// `_Py_HashSecret`), so a set of strings can only agree with a CPython pinned to
// `PYTHONHASHSEED=0` — the documented boundary.

const SET_MINSIZE: usize = 8;
const SET_LINEAR_PROBES: usize = 9;
const SET_PERTURB_SHIFT: u32 = 5;

/// CPython's `hash()` for a machine int.
///
/// This returned `n` unchanged (bar `-1`), which is right only below the
/// modulus: `hash(2**62)` is `2`, not `2**62`. Since the value feeds the set
/// table's slot probe, a wrong hash placed large ints in the wrong slots and
/// so produced the wrong ITERATION ORDER, not just a wrong number.
fn cpython_int_hash(n: i64) -> i64 {
    crate::pyhash::int_i64(n)
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
    // CPython's `slot_tp_hash` does NOT reduce every `__hash__` result modulo
    // 2**61-1. It first tries `PyLong_AsSsize_t`, so any value that already
    // fits in a `Py_hash_t` is used VERBATIM — `__hash__` returning `2**62`
    // hashes to `2**62`, not to `2`. Only on overflow does it fall back to
    // `long.__hash__`. That is deliberate: it preserves `x.__hash__() ==
    // hash(y)` implying `hash(x) == hash(y)`. Reducing unconditionally would
    // silently rewrite every large in-range hash a user returns.
    let h = match v {
        Value::Bool(b) => *b as i64,
        Value::Int(n) => *n,
        _ => with_host(|h| match h.get(v) {
            Some(PyObj::BigInt(b)) => {
                use num_traits::ToPrimitive;
                // Out of `Py_hash_t` range: only now does `long.__hash__` run.
                Ok(b.to_i64().unwrap_or_else(|| bigint_pyhash(b)))
            }
            _ => Err(type_error("__hash__ method should return an integer")),
        })?,
    };
    // `-1` is reserved for errors.
    Ok(if h == -1 { -2 } else { h })
}

/// CPython's `long_hash`: `|x| mod (2**61 - 1)` with the sign applied last,
/// `-1` mapped to `-2`.
///
/// The previous version took a SIGNED remainder and then folded it into
/// `[0, modulus)` by adding the modulus before negating, which computes
/// `-(modulus - (|x| mod modulus))` for negatives — measured as
/// `hash(-(2**64))` returning `-2305843009213693943` where CPython returns
/// `-8`. CPython reduces the magnitude digits and multiplies by the sign at the
/// very end, which is what [`crate::pyhash::int_big`] does.
fn bigint_pyhash(b: &num_bigint::BigInt) -> i64 {
    crate::pyhash::int_big(b)
}

/// Resolve — outside any host borrow — the dict/set key for a user instance whose
/// class defines `__hash__`, stashing it in the pending-key table so the borrowed
/// [`PyHost::to_key`] can pick it up. `candidates` are the `(key, key-object)`
/// pairs already in the target container; if the instance's `__hash__` matches an
/// existing instance key whose object is `__eq__`-equal, the key collapses onto
/// that entry (CPython value semantics). A no-op for non-instances and for
/// identity-hashed instances (`to_key` handles those inline).
pub fn prepare_key(v: &Value, candidates: &[(PKey, Value)]) -> Result<(), String> {
    let mut collapsed = Vec::new();
    prepare_key_walk(v, candidates, &mut collapsed)
}

/// What [`prepare_key_walk`] has to do with a value, decided in ONE host borrow.
enum KeyPrep {
    /// A `tuple`/`frozenset`: hashed element-wise, so its elements are keys too.
    Nested(Vec<Value>),
    /// A user instance — `(heap id, class name)`.
    Instance(u32, String),
    /// A bridged CPython object — `(heap id, foreign id)`.
    #[cfg(feature = "stdlib-ffi")]
    Foreign(u32, u32),
    /// Nothing to prepare; `to_key` resolves it without running user code.
    Plain,
}

/// The recursive half of [`prepare_key`].
///
/// A `tuple`/`frozenset` key is hashed element-wise, so a value-keyed object
/// NESTED inside one is a key in its own right and needs the same preparation —
/// `{(P(1),): 5}` raised `unhashable type: 'P'` from the borrowed `to_key`,
/// which cannot run `__hash__`, because only the TOP-LEVEL object was prepared.
///
/// `collapsed` accumulates the keys resolved so far in THIS walk and is searched
/// after `candidates`, so two equal elements of one key (`((P(1),), (P(1),))`)
/// collapse onto each other exactly as they would onto an already-stored key.
/// Without it each took its own heap id and no independently built equal key
/// could ever match.
fn prepare_key_walk(
    v: &Value,
    candidates: &[(PKey, Value)],
    collapsed: &mut Vec<(PKey, Value)>,
) -> Result<(), String> {
    let what = with_host(|h| match v {
        Value::Obj(i) => match h.get(v) {
            Some(PyObj::Tuple(t)) => KeyPrep::Nested(t.clone()),
            Some(PyObj::Frozenset(s)) => KeyPrep::Nested(s.values().cloned().collect()),
            Some(PyObj::Instance(inst)) => KeyPrep::Instance(*i, inst.class.clone()),
            #[cfg(feature = "stdlib-ffi")]
            Some(PyObj::Foreign(f)) => KeyPrep::Foreign(*i, *f),
            _ => KeyPrep::Plain,
        },
        _ => KeyPrep::Plain,
    });
    let (id, class) = match what {
        KeyPrep::Plain => return Ok(()),
        KeyPrep::Nested(items) => {
            for it in &items {
                prepare_key_walk(it, candidates, collapsed)?;
            }
            return Ok(());
        }
        // A CPython Foreign key (enum member, Decimal, datetime, …): hash via the
        // bridge outside the borrow, then collapse onto a value-equal existing key
        // of the same hash (CPython `PyObject_RichCompareBool`), so a fresh handle
        // keys to the same slot as an equal one already present. Mirrors the
        // instance path below.
        #[cfg(feature = "stdlib-ffi")]
        KeyPrep::Foreign(id, fid) => {
            // A foreign object that IS a number keys as that native number.
            //
            // CPython's dict finds `1` and `Decimal(1)` in one slot because
            // they hash equally and compare equal — the hash table never asks
            // what TYPE they are. Here a key is a structural `PKey`, so
            // `PKey::Foreign` and `PKey::Int` are different slots however their
            // hashes compare, and `{1, Decimal(1)}` stayed a 2-element set even
            // once both hashed to 1. Keying the foreign object as its native
            // equivalent restores CPython's behaviour in BOTH directions: a
            // native key already present is now found by the foreign one, and a
            // foreign key already present is found by the native one (which
            // never reaches this function at all, so a scan of the candidate
            // list could only ever have fixed one direction).
            //
            // The dict stores the key OBJECT alongside its `PKey`, so
            // `{Decimal(1): 'dec'}` still reprs as `Decimal('1')` — only the
            // slot is shared, not the identity CPython shows.
            if let Some(native) = crate::ffi::foreign_numeric_key(fid) {
                let key = with_host(|h| h.to_key(&native))?;
                collapsed.push((key.clone(), v.clone()));
                pending_key_set(id, key);
                return Ok(());
            }
            let hash = crate::ffi::foreign_hash(fid)?;
            let mut canonical = PKey::Foreign { hash, id };
            for (pk, kobj) in candidates.iter().chain(collapsed.iter()) {
                if let PKey::Foreign { hash: ch, .. } = pk {
                    if *ch == hash {
                        if let Some(cf) = with_host(|h| h.foreign_id(kobj)) {
                            if crate::ffi::foreign_eq(fid, cf) {
                                canonical = pk.clone();
                                break;
                            }
                        }
                    }
                }
            }
            collapsed.push((canonical.clone(), v.clone()));
            pending_key_set(id, canonical);
            return Ok(());
        }
        KeyPrep::Instance(id, class) => (id, class),
    };
    // Shadows any inherited `__hash__`; see `implicit_hash_none`.
    if with_host(|h| h.implicit_hash_none(&class)) {
        return Err(type_error(&format!("unhashable type: '{class}'")));
    }
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
    // Collapse onto a value-equal existing instance key of the same hash. The
    // comparison runs through the full `==` dispatch rather than a bare
    // `__eq__` call: a class may define `__hash__` WITHOUT `__eq__` (CPython
    // then inherits `object.__eq__`, i.e. identity), and a builtin-type
    // subclass compares through its payload. Calling `__eq__` directly raised
    // `AttributeError: 'P' object has no attribute '__eq__'` for the first of
    // those, so `{P(1): 1, P(2): 2}` was unusable whenever the two hashes
    // collided.
    let mut canonical = PKey::Instance { hash, id };
    for (pk, kobj) in candidates.iter().chain(collapsed.iter()) {
        if let PKey::Instance { hash: ch, .. } = pk {
            if *ch == hash {
                let eqr = crate::builtins::numeric_hook(NumOp::Eq, v, kobj)?;
                if with_host(|h| h.truthy(&eqr)) {
                    canonical = pk.clone();
                    break;
                }
            }
        }
    }
    collapsed.push((canonical.clone(), v.clone()));
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
    // Asked before the MRO lookup: the implicit `__hash__ = None` shadows an
    // inherited `__hash__`, so a subclass defining only `__eq__` is unhashable
    // even though `class_lookup` would find its base's real one.
    if with_host(|h| h.implicit_hash_none(&class)) {
        return Err(type_error(&format!("unhashable type: '{class}'")));
    }
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

/// Whether a key was resolved through user code — a user `__hash__`/`__eq__`
/// instance or a bridged CPython object — at ANY depth. A `tuple`/`frozenset`
/// key is hashed element-wise, so one holding such an element is itself
/// value-keyed and takes part in every collapse the top-level ones do.
pub fn pkey_is_value_keyed(k: &PKey) -> bool {
    match k {
        PKey::Instance { .. } | PKey::Foreign { .. } => true,
        PKey::Tuple(ks) | PKey::Frozenset(ks) => ks.iter().any(pkey_is_value_keyed),
        _ => false,
    }
}

/// Collect the value-keyed `(key, key-object)` pairs reachable from one
/// container entry, descending into `tuple`/`frozenset` keys alongside the
/// object they were built from. A nested element is a collapse candidate for
/// any nested element of another key: `{P(1): 1, (P(1),): 2}` must key both
/// `P(1)`s identically or `{(P(1),): 2}[(P(1),)]` cannot find its entry.
fn collect_key_candidates(h: &PyHost, k: &PKey, obj: &Value, out: &mut Vec<(PKey, Value)>) {
    match k {
        PKey::Instance { .. } | PKey::Foreign { .. } => out.push((k.clone(), obj.clone())),
        PKey::Tuple(ks) => {
            if let Some(PyObj::Tuple(items)) = h.get(obj) {
                if items.len() == ks.len() {
                    for (sub, it) in ks.iter().zip(items.iter()) {
                        collect_key_candidates(h, sub, it, out);
                    }
                }
            }
        }
        // A `PKey::Frozenset`'s element keys are sorted+deduped, so they no
        // longer line up positionally with anything; walk the frozenset's own
        // map, which is the authoritative key→object pairing.
        PKey::Frozenset(_) => {
            if let Some(PyObj::Frozenset(s)) = h.get(obj) {
                for (sub, e) in s.iter() {
                    collect_key_candidates(h, sub, e, out);
                }
            }
        }
        _ => {}
    }
}

/// Instance-key collapse candidates from an in-flight set map (a literal/ctor
/// being built element by element).
pub fn set_local_candidates(h: &PyHost, s: &IndexMap<PKey, Value>) -> Vec<(PKey, Value)> {
    let mut out = Vec::new();
    for (k, v) in s.iter() {
        collect_key_candidates(h, k, v, &mut out);
    }
    out
}

/// Instance-key collapse candidates from an in-flight dict map.
pub fn dict_local_candidates(h: &PyHost, d: &IndexMap<PKey, (Value, Value)>) -> Vec<(PKey, Value)> {
    let mut out = Vec::new();
    for (k, (kv, _)) in d.iter() {
        collect_key_candidates(h, k, kv, &mut out);
    }
    out
}

/// The `(key, key-object)` pairs of any instance keys already present in a heap
/// dict or set/frozenset — the collapse candidates for [`prepare_key`].
pub fn instance_key_candidates(container: &Value) -> Vec<(PKey, Value)> {
    instance_key_candidates_for(container, None)
}

/// The same, but skipping the scan when `key` cannot collapse against an
/// instance key. The candidate list exists so a value-equal user instance finds
/// an existing entry; a plain `int`/`str` key can never do that, and
/// `with_instance_key` is a passthrough for it. Scanning anyway walked (and
/// cloned out of) the WHOLE container on every subscript — `d[i]` over a 200k
/// dict was quadratic.
pub fn instance_key_candidates_for(container: &Value, key: Option<&Value>) -> Vec<(PKey, Value)> {
    if let Some(k) = key {
        if !with_host(|h| value_can_collapse(h, k)) {
            return Vec::new();
        }
    }
    with_host(|h| match h.get(container) {
        Some(PyObj::Dict(d)) => dict_local_candidates(h, d),
        Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => set_local_candidates(h, s),
        _ => vec![],
    })
}

/// Whether `k` could collapse onto an existing value-keyed entry — i.e. whether
/// scanning the destination container for candidates can pay off. A `str`/`int`/
/// `bytes` key never can; a `tuple`/`frozenset` can only through an element, so
/// it is walked rather than rejected outright (the pre-existing blanket
/// rejection is what left `{(P(1),): 5}[(P(1),)]` unable to find its entry).
fn value_can_collapse(h: &PyHost, k: &Value) -> bool {
    match k {
        Value::Obj(_) => match h.get(k) {
            Some(PyObj::Str(_) | PyObj::Bytes(_) | PyObj::BigInt(_)) => false,
            Some(PyObj::Tuple(items)) => items.iter().any(|it| value_can_collapse(h, it)),
            Some(PyObj::Frozenset(s)) => s.values().any(|e| value_can_collapse(h, e)),
            _ => true,
        },
        _ => false,
    }
}

impl PyHost {
    /// Whether `k` is a key that can NEVER collapse onto an existing value-equal
    /// entry, so the candidate scan `with_instance_key` performs is guaranteed
    /// to come back empty and the whole detour can be skipped.
    ///
    /// The inverse of `value_can_collapse`, exposed so the container-op fast
    /// paths test exactly the same condition the slow path would, rather than
    /// re-deriving a weaker approximation of it.
    pub fn key_cannot_collapse(&self, k: &Value) -> bool {
        !value_can_collapse(self, k)
    }
}

/// Prepare an instance key for a container op, run `f` (the borrowed access that
/// calls `to_key`), then clear the pending table. `candidates` collapses a
/// value-equal existing key. Any non-instance `v` makes this a thin passthrough.
pub fn with_instance_key<R>(
    v: &Value,
    role: KeyRole,
    candidates: &[(PKey, Value)],
    f: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    let out = with_instance_key_inner(v, candidates, f);
    match out {
        Ok(r) => Ok(r),
        // The unhashable failure is raised while PREPARING the key, before the
        // container op ever runs, which is why wrapping the ops alone left every
        // construction and membership spelling reporting the bare message.
        Err(e) => Err(with_host(|h| wrap_unhashable(h, e, role, v))),
    }
}

fn with_instance_key_inner<R>(
    v: &Value,
    candidates: &[(PKey, Value)],
    f: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    // Save the caller's table first. `prepare_key` runs user `__hash__`/`__eq__`,
    // which can itself perform a container op (`hash(self.v)`, `k in cache`); that
    // INNER op must not drop the keys the outer one already resolved.
    // `{(P(1), P(2)): 5}` lost P(1)'s key to the `hash()` call inside P(2)'s
    // `__hash__` and raised `unhashable type: 'P'`.
    let saved = pending_key_take();
    let prep = prepare_key(v, candidates);
    let r = prep.and_then(|()| f());
    pending_key_restore(saved);
    r
}

/// Whether any of `keys` was resolved through user code — a user
/// `__hash__`/`__eq__` instance, or a bridged CPython object. Those keys carry
/// the heap id of the object they collapsed onto (see `PKey::Instance`), so two
/// independently built containers key value-equal elements differently.
fn any_value_keyed<'a>(mut keys: impl Iterator<Item = &'a PKey>) -> bool {
    keys.any(pkey_is_value_keyed)
}

/// The key map an operand contributes to the set algebra: a `set`/`frozenset`'s
/// own map, or a `dict_keys` view's backing dict. `None` for everything else.
fn setlike_map(h: &PyHost, v: &Value) -> Option<IndexMap<PKey, Value>> {
    match h.get(v) {
        Some(PyObj::Set(s) | PyObj::Frozenset(s)) => Some(s.clone()),
        _ => h.keys_view_map(v),
    }
}

/// The right operand of a set/dict operation, re-keyed into the LEFT operand's
/// key space and returned as a fresh container of the same kind.
///
/// The borrowed comparisons that implement the set algebra (`& | - ^`), the
/// subset orders (`<= < >= >`), and container `==` match keys structurally, and
/// a key built from user code embeds the heap id of the object it collapsed
/// onto — so `{P(1), P(2)} & {P(2)}` compared two different keys for the two
/// `P(2)`s and yielded `set()`. Running every element of `b` back through
/// [`prepare_key`] against `a`'s existing keys (which calls `__hash__`/`__eq__`,
/// or the CPython bridge's, outside the borrow) collapses each value-equal
/// element onto `a`'s key, after which the structural comparison is correct.
///
/// Returns `None` — no work done, the caller uses `b` unchanged — unless BOTH
/// operands are the same kind of container AND both carry a key that resolved
/// through user code. Nothing else can collapse, so the ordinary
/// `int`/`str`/tuple-keyed containers keep their existing fast path.
pub fn align_operand(a: &Value, b: &Value) -> Result<Option<Value>, String> {
    enum Side {
        Set(Vec<Value>, bool),
        Dict(Vec<(Value, Value)>),
    }
    let (cands, side) = match with_host(|h| match (setlike_map(h, a), setlike_map(h, b)) {
        // Both operands set-like: a `set`, a `frozenset`, or a `dict_keys` view
        // (CPython's key view IS a set and takes part in the same algebra).
        (Some(x), Some(y)) => {
            if !any_value_keyed(x.keys()) || !any_value_keyed(y.keys()) {
                return None;
            }
            let items = y.values().cloned().collect();
            Some((
                set_local_candidates(h, &x),
                Side::Set(items, h.is_frozenset(b)),
            ))
        }
        _ => match (h.get(a), h.get(b)) {
            (Some(PyObj::Dict(x)), Some(PyObj::Dict(y))) => {
                if !any_value_keyed(x.keys()) || !any_value_keyed(y.keys()) {
                    return None;
                }
                let pairs = y.values().cloned().collect();
                Some((dict_local_candidates(h, x), Side::Dict(pairs)))
            }
            _ => None,
        },
    }) {
        Some(t) => t,
        None => return Ok(None),
    };
    match side {
        Side::Set(items, frozen) => {
            let mut out: IndexMap<PKey, Value> = IndexMap::with_capacity(items.len());
            for it in items {
                let k =
                    with_instance_key(&it, KeyRole::Set, &cands, || with_host(|h| h.to_key(&it)))?;
                set_put(&mut out, k, it);
            }
            Ok(Some(with_host(|h| h.new_setlike(out, frozen))))
        }
        Side::Dict(pairs) => {
            let mut out: IndexMap<PKey, (Value, Value)> = IndexMap::with_capacity(pairs.len());
            for (kv, vv) in pairs {
                let k =
                    with_instance_key(&kv, KeyRole::Dict, &cands, || with_host(|h| h.to_key(&kv)))?;
                dict_put(&mut out, k, kv, vv);
            }
            Ok(Some(with_host(|h| h.new_dict(out))))
        }
    }
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

/// Number of elements in `range(start, stop, step)`, EXACTLY — in `i128`,
/// because the count of a range spanning the i64 domain does not fit an i64.
///
/// `len(range(sys.maxsize))` computed `stop - start + step - 1` in `i64` and
/// overflowed on the `+ step`, panicking the debug build outright.
pub fn range_len_exact(start: i64, stop: i64, step: i64) -> i128 {
    let (start, stop, step) = (start as i128, stop as i128, step as i128);
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

/// Number of elements in `range(start, stop, step)`, saturated to `i64::MAX`.
///
/// Every caller but `len()` uses the count for indexing and iteration, where a
/// range longer than `i64::MAX` cannot be exhausted anyway; `len()` reads
/// [`range_len_exact`] so it can report the OverflowError CPython raises.
pub fn range_len(start: i64, stop: i64, step: i64) -> i64 {
    range_len_exact(start, stop, step).min(i64::MAX as i128) as i64
}

/// Number of elements in a bignum `range(start, stop, step)`.
pub fn big_range_len(
    start: &num_bigint::BigInt,
    stop: &num_bigint::BigInt,
    step: &num_bigint::BigInt,
) -> num_bigint::BigInt {
    use num_bigint::BigInt;
    use num_traits::{One, Zero};
    let zero = BigInt::zero();
    if *step > zero {
        if stop > start {
            (stop - start + step - BigInt::one()) / step
        } else {
            zero
        }
    } else if start > stop {
        (start - stop - step - BigInt::one()) / (-step)
    } else {
        zero
    }
}

/// Whether reading `n` as an `f64` would land on a DIFFERENT number. Past 2^53
/// the doubles are spaced more than 1 apart, so consecutive integers collapse
/// onto a shared neighbour and two distinct values compare equal. Mirrors the
/// gate fusevm applies before handing a mixed int/float pair to this host.
#[inline]
fn f64_would_round(n: i64) -> bool {
    n.unsigned_abs() > (1u64 << 53)
}

/// The EXACT ordering of an integer against a float. Both sides are resolved in
/// the integer domain rather than as a common `f64`, so no rounding can make two
/// different numbers agree — Python compares `int` against `float` exactly at any
/// magnitude, unlike the JVM languages that promote the integer to a double.
/// `None` for a NaN, which is unordered against everything.
fn exact_int_cmp_float(i: &num_bigint::BigInt, f: f64) -> Option<std::cmp::Ordering> {
    use num_traits::FromPrimitive;
    use std::cmp::Ordering;
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    // `floor(f)` is integral, so its `BigInt` is exact. With `n = floor(f)` and
    // `n <= f < n+1`, an integer `i != n` is settled by `i.cmp(&n)` alone; only
    // `i == n` needs the fraction, where any leftover makes `f` the larger.
    let n = num_bigint::BigInt::from_f64(f.floor())?;
    Some(i.cmp(&n).then(if f.fract() == 0.0 {
        Ordering::Equal
    } else {
        Ordering::Less
    }))
}

/// Render tuple elements with Python's trailing comma for a 1-tuple.
fn tuple_body(items: &[String]) -> String {
    match items {
        [one] => format!("{one},"),
        _ => items.join(", "),
    }
}

fn bigint_to_f64(b: &num_bigint::BigInt) -> f64 {
    use num_traits::ToPrimitive;
    b.to_f64().unwrap_or(f64::INFINITY)
}

/// `int / int` as the exactly-rounded `f64` CPython's `long_true_divide`
/// produces, or the `OverflowError` it raises when the QUOTIENT is past the
/// `f64` range.
///
/// Converting each side to `f64` and dividing — what the float fallback did —
/// is wrong twice over once either side is past that range: `2**2000 / 2**1999`
/// became `inf / inf` = `nan` instead of `2.0`, and a perfectly representable
/// quotient was reported as `inf` because an OPERAND alone did not fit.
///
/// The quotient is therefore formed in the integer domain. It is scaled to carry
/// 55 significant bits — TWO more than an `f64` mantissa holds — and its lowest
/// bit is forced ODD whenever the division left a remainder. Round-to-odd is
/// what makes the two-step rounding exact: the single round-to-nearest-even
/// inside `to_f64` can no longer be pulled the wrong way by a discarded tail,
/// including in the subnormal range where naive double rounding is worst.
///
/// TWO spare bits, not one, is the load-bearing part. With a single spare bit an
/// odd quotient sits exactly halfway between two doubles — it IS the tie that
/// round-to-odd exists to prevent — and `to_f64` then breaks it by its own
/// half-to-even rule, which knows nothing of the discarded remainder. That cost
/// one ulp on `(10**20) / 3`: `3.3333333333333336e+19` for CPython's
/// `3.333333333333333e+19`. With two spare bits a tie is the bit pattern `…10`,
/// which an odd low bit can never be.
fn bigint_true_divide(a: &num_bigint::BigInt, b: &num_bigint::BigInt) -> Result<f64, String> {
    use num_integer::Integer;
    use num_traits::{Signed, ToPrimitive, Zero};

    if b.is_zero() {
        return Err("ZeroDivisionError: division by zero".into());
    }
    let negative = a.is_negative() != b.is_negative();
    if a.is_zero() {
        // CPython keeps the sign of a zero quotient (`0 / -1` is `-0.0`).
        return Ok(if negative { -0.0 } else { 0.0 });
    }
    let (a, b) = (a.abs(), b.abs());

    // `q = floor(a << shift / b)` then has 55 or 56 bits, whatever the inputs.
    let shift = 55i64 + b.bits() as i64 - a.bits() as i64;
    let (num, den) = if shift >= 0 {
        (a << shift as u64, b)
    } else {
        (a, b << shift.unsigned_abs())
    };
    let (mut q, r) = num.div_rem(&den);
    if !r.is_zero() && q.is_even() {
        q += 1u32;
    }

    // `q` is 56 bits at most, so `to_f64` cannot overflow here; the scaling is
    // where a too-large quotient becomes infinite, and a too-small one becomes
    // zero exactly as CPython's does (`1 / 2**2000` is `0.0`, not an error).
    let scaled = libm::ldexp(
        q.to_f64().unwrap_or(f64::INFINITY),
        -shift.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    );
    if !scaled.is_finite() {
        return Err("OverflowError: integer division result too large for a float".into());
    }
    Ok(if negative { -scaled } else { scaled })
}

// ── arithmetic / comparison delegated from the numeric hook ──────────────────

impl PyHost {
    /// The strict numeric-hook path. Usually `op` on operands where at least one
    /// is not a native int/float (bool, bignum, str, list, …), but two plain
    /// numbers reach it too: an int op that overflowed, an `x op= y` rebind, and
    /// a mixed int/float pair whose integer is past 2^53 — which fusevm hands
    /// over precisely BECAUSE reading it as an `f64` would answer the wrong
    /// number. See `numeric_hook` and `rounding_int_float_pair`.
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
                if let (Some(x), Some(y)) = (self.num_val_arith(a)?, self.num_val_arith(b)?) {
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
                // Two templates concatenate: the seam joins the left's trailing
                // literal to the right's leading one, keeping
                // `len(strings) == len(interpolations) + 1`.
                if let (
                    Some(PyObj::Template {
                        strings: ls,
                        interpolations: li,
                    }),
                    Some(PyObj::Template {
                        strings: rs,
                        interpolations: ri,
                    }),
                ) = (self.get(a), self.get(b))
                {
                    let mut strings = ls.clone();
                    let mut interpolations = li.clone();
                    let (rs, ri) = (rs.clone(), ri.clone());
                    let seam = strings.pop().unwrap_or_default();
                    let mut rs = rs.into_iter();
                    strings.push(seam + &rs.next().unwrap_or_default());
                    strings.extend(rs);
                    interpolations.extend(ri);
                    return Ok(self.alloc(PyObj::Template {
                        strings,
                        interpolations,
                    }));
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
                if let (Some(x), Some(y)) = (self.num_val_arith(a)?, self.num_val_arith(b)?) {
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
                let a_set = self.setmap_of(a).is_some();
                if let (Some(mut out), Some(y)) = (self.setmap_of(a), self.setmap_operand(b, a_set))
                {
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
                if let (Some(x), Some(y)) = (self.num_val_arith(a)?, self.num_val_arith(b)?) {
                    return Ok(Value::Float(x * y));
                }
                if self.is_complex(a) || self.is_complex(b) {
                    if let (Some((ar, ai)), Some((br, bi))) =
                        (self.complex_val(a), self.complex_val(b))
                    {
                        return Ok(self.alloc(PyObj::Complex(ar * br - ai * bi, ar * bi + ai * br)));
                    }
                }
                // A SEQUENCE on either side gets CPython's sequence-specific
                // message, which names the non-int operand's type; the generic
                // `unsupported operand type(s) for *` is what CPython says only
                // when neither side is a sequence.
                for (seq, other) in [(a, b), (b, a)] {
                    if self.is_sequence_for_repeat(seq) {
                        // A count that IS an int and merely does not fit is
                        // `PySequence_Repeat`'s `PyNumber_AsSsize_t(n,
                        // PyExc_OverflowError)`, not the non-int TypeError:
                        // `[1] * 10**20` is
                        // `OverflowError: cannot fit 'int' into an index-sized
                        // integer`.
                        if matches!(self.index_fit(other), IndexFit::TooLarge(_)) {
                            return Err(format!("OverflowError: {INDEX_OVERFLOW}"));
                        }
                        return Err(type_error(&format!(
                            "can't multiply sequence by non-int of type '{}'",
                            self.type_name(other)
                        )));
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
                if let Some(c) = self.counter_unary(a, true) {
                    return Ok(c);
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

    /// Whether `v` is one of the built-in sequences `*` repeats — the receivers
    /// for which CPython raises `can't multiply sequence by non-int` instead of
    /// the generic operand-type message.
    pub fn is_sequence_for_repeat(&self, v: &Value) -> bool {
        matches!(
            self.get(v),
            Some(PyObj::Str(_))
                | Some(PyObj::List(_))
                | Some(PyObj::Tuple(_))
                | Some(PyObj::Bytes(_))
                | Some(PyObj::Bytearray(_))
        )
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

    /// Whether `v` is an integer too large for `i64` (a heap bignum).
    #[inline]
    pub fn is_bignum(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(_)) && matches!(self.get(v), Some(PyObj::BigInt(_)))
    }

    /// `v` as an `f64` only when it genuinely IS a float (no int coercion).
    #[inline]
    pub fn float_val(&self, v: &Value) -> Option<f64> {
        match v {
            Value::Float(f) => Some(*f),
            _ => None,
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
        // The result length has to be reserved FALLIBLY. `Vec::with_capacity` /
        // `str::repeat` abort the process on a failed allocation, so
        // `'a' * (2**48)` printed `memory allocation of … bytes failed` and
        // exited 134; CPython raises a catchable `MemoryError` for every one of
        // `[1]*(2**48)`, `'a'*(2**62)` and `(1,)*(2**62)`, and the bytes path
        // has its own `OverflowError: repeated bytes are too long`.
        match self.get(&seq) {
            Some(PyObj::Str(s)) => {
                let mut r = String::new();
                reserve_repeat(&mut r, s.len(), n, false)?;
                let base = s.clone();
                for _ in 0..n {
                    r.push_str(&base);
                }
                Ok(Some(self.new_str(r)))
            }
            Some(PyObj::List(l)) => {
                let base = l.clone();
                let mut out: Vec<Value> = Vec::new();
                reserve_repeat(&mut out, base.len(), n, false)?;
                for _ in 0..n {
                    out.extend(base.clone());
                }
                Ok(Some(self.new_list(out)))
            }
            Some(PyObj::Tuple(l)) => {
                let base = l.clone();
                let mut out: Vec<Value> = Vec::new();
                reserve_repeat(&mut out, base.len(), n, false)?;
                for _ in 0..n {
                    out.extend(base.clone());
                }
                Ok(Some(self.new_tuple(out)))
            }
            Some(PyObj::Bytes(s)) => {
                let base = s.clone();
                let mut out: Vec<u8> = Vec::new();
                reserve_repeat(&mut out, base.len(), n, true)?;
                for _ in 0..n {
                    out.extend_from_slice(&base);
                }
                Ok(Some(self.alloc(PyObj::Bytes(out))))
            }
            Some(PyObj::Bytearray(s)) => {
                let base = s.clone();
                let mut out: Vec<u8> = Vec::new();
                reserve_repeat(&mut out, base.len(), n, true)?;
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
        let subset_order = |a_sub_b: bool, b_sub_a: bool, la: usize, lb: usize| match op {
            NumOp::Le => a_sub_b,
            NumOp::Lt => a_sub_b && la < lb,
            NumOp::Ge => b_sub_a,
            NumOp::Gt => b_sub_a && lb < la,
            _ => unreachable!(),
        };
        if let (Some(x), Some(y)) = (self.setlike(a), self.setlike(b)) {
            let a_sub_b = x.keys().all(|k| y.contains_key(k)); // a ⊆ b
            let b_sub_a = y.keys().all(|k| x.contains_key(k)); // b ⊆ a
            return Ok(Value::Bool(subset_order(
                a_sub_b,
                b_sub_a,
                x.len(),
                y.len(),
            )));
        }
        // The same partial order for a `dict_keys`/`dict_items` view against a
        // set or another view (`d.keys() <= {1, 2}`). Only reached when a view
        // is involved, so a plain set comparison keeps the zero-copy path above.
        if self.either_is_view(a, b) {
            if let (Some(x), Some(y)) = (self.view_keyset(a), self.view_keyset(b)) {
                let xs: HashSet<&PKey> = x.iter().collect();
                let ys: HashSet<&PKey> = y.iter().collect();
                let a_sub_b = xs.iter().all(|k| ys.contains(*k));
                let b_sub_a = ys.iter().all(|k| xs.contains(*k));
                return Ok(Value::Bool(subset_order(
                    a_sub_b,
                    b_sub_a,
                    x.len(),
                    y.len(),
                )));
            }
        }
        // The operator symbol drives the `'<' not supported …` message; CPython
        // uses the OUTER operator even for a failing list/tuple element compare
        // (`[1] >= ["a"]` reports `>=`), so it threads through `order`'s recursion.
        let sym = match op {
            NumOp::Lt => "<",
            NumOp::Le => "<=",
            NumOp::Gt => ">",
            NumOp::Ge => ">=",
            _ => "<",
        };
        let ord = self.order(a, b, sym)?;
        let res = match op {
            NumOp::Lt => ord == Ordering::Less,
            NumOp::Le => ord != Ordering::Greater,
            NumOp::Gt => ord == Ordering::Greater,
            NumOp::Ge => ord != Ordering::Less,
            _ => unreachable!(),
        };
        Ok(Value::Bool(res))
    }

    fn order(&self, a: &Value, b: &Value, sym: &str) -> Result<std::cmp::Ordering, String> {
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
        // A mixed integer/float pair orders in the integer domain too. The guard
        // above deliberately skips a float operand, so until this arm existed
        // EVERY mixed pair — bignum included, where `equal` was already exact —
        // fell to the `f64` route below and called two different numbers Equal:
        // `3**40 > float(3**40)` and `3**34 > float(3**34)` both answered False.
        if let Some((x, f, int_on_left)) = self.rounding_int_float_pair(a, b) {
            if let Some(ord) = exact_int_cmp_float(&x, f) {
                return Ok(if int_on_left { ord } else { ord.reverse() });
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
                    let o = self.order(p, q, sym)?;
                    if o != Ordering::Equal {
                        return Ok(o);
                    }
                }
                Ok(x.len().cmp(&y.len()))
            }
            // Two CPython Foreign objects order by CPython's own rich comparison,
            // so foreign elements inside a list/tuple sort or `<` compare correctly
            // (`sorted([(IntEnum, …)])`, `[date] < [date]`, `[Decimal] < [Decimal]`).
            #[cfg(feature = "stdlib-ffi")]
            (Some(PyObj::Foreign(x)), Some(PyObj::Foreign(y))) => crate::ffi::foreign_cmp(*x, *y),
            _ => Err(type_error(&format!(
                "'{sym}' not supported between instances of '{}' and '{}'",
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
        // The float reading of each operand is taken per-arm through
        // `num_val_arith`, not eagerly here: the integer arms below return
        // without ever needing it, and the conversion can RAISE (a bignum past
        // the f64 range), which an eager read had no way to report.
        match tag {
            binop::DIV => {
                // Two integers divide in the INTEGER domain (CPython's
                // `long_true_divide`), never by converting each side to `f64`
                // first — see `bigint_true_divide`. Small operands take the
                // shortcut: below 2^53 both convert exactly, so the `f64`
                // division is already the one correctly-rounded operation the
                // long path would arrive at.
                if let (Some(x), Some(y)) = (ai, bi) {
                    if y == 0 {
                        return Err("ZeroDivisionError: division by zero".into());
                    }
                    if x.unsigned_abs() < (1 << 53) && y.unsigned_abs() < (1 << 53) {
                        return Ok(Value::Float(x as f64 / y as f64));
                    }
                }
                if let (Some(x), Some(y)) = (self.big_val(a), self.big_val(b)) {
                    return bigint_true_divide(&x, &y).map(Value::Float);
                }
                match (self.num_val_arith(a)?, self.num_val_arith(b)?) {
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
                }
            }
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
                match (self.num_val_arith(a)?, self.num_val_arith(b)?) {
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
                match (self.num_val_arith(a)?, self.num_val_arith(b)?) {
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
                _ => match (self.num_val_arith(a)?, self.num_val_arith(b)?) {
                    // `0 ** <negative>` is a division by zero in CPython, not `inf`.
                    // As of 3.14 both int and float bases word it the same way.
                    (Some(x), Some(y)) if x == 0.0 && y < 0.0 => {
                        Err("ZeroDivisionError: zero to a negative power".into())
                    }
                    // A negative real base raised to a non-integer power yields a
                    // complex result in CPython (Rust's `powf` returns NaN).
                    //
                    // An INFINITE exponent is not a non-integer power: C99 gives
                    // `pow(-1.0, inf) == 1.0`, which CPython inherits. `fract()`
                    // of an infinity is NaN, and NaN compares unequal to 0.0, so
                    // every negative base with an infinite exponent used to fall
                    // into the complex branch — `(-1.0) ** float('inf')` answered
                    // `(nan+nanj)` where CPython answers `1.0`.
                    (Some(x), Some(y)) if x < 0.0 && y.is_finite() && y.fract() != 0.0 => {
                        let (r, i) = c_pow((x, 0.0), (y, 0.0));
                        Ok(self.alloc(PyObj::Complex(r, i)))
                    }
                    (Some(x), Some(y)) => {
                        let r = x.powf(y);
                        // CPython's `float_pow` reports the C library's ERANGE
                        // as `OverflowError` rather than returning an infinity.
                        // Only a FINITE pair can overflow into one — `2.0 **
                        // float('inf')` and `float('inf') ** 2` stay `inf`.
                        if r.is_infinite() && x.is_finite() && y.is_finite() {
                            // The message half is the C library's own
                            // `strerror(ERANGE)`, which CPython passes through
                            // verbatim — "Result too large" on Apple libc,
                            // "Numerical result out of range" on glibc. Hard-
                            // coding either one made this diverge from the
                            // reference on the other platform.
                            // SAFETY: `strerror` returns a pointer to a static,
                            // NUL-terminated string for any errno value.
                            let msg =
                                unsafe { std::ffi::CStr::from_ptr(libc::strerror(libc::ERANGE)) }
                                    .to_string_lossy()
                                    .into_owned();
                            return Err(format!("OverflowError: ({}, '{msg}')", libc::ERANGE));
                        }
                        Ok(Value::Float(r))
                    }
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
                let a_set = self.setmap_of(a).is_some();
                let b_set = self.setmap_of(b).is_some();
                if let (Some(x), Some(y)) =
                    (self.setmap_operand(a, b_set), self.setmap_operand(b, a_set))
                {
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
                // PEP 604: `X | Y` on type objects builds a `types.UnionType`.
                if tag == binop::BITOR {
                    if let (Some(mut xs), Some(ys)) = (self.union_members(a), self.union_members(b))
                    {
                        xs.extend(ys);
                        // Dedupes, and collapses `int | int` to the single type.
                        return Ok(self.build_union(xs));
                    }
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

    /// Order two Counter counts for `most_common`/`repr`. Counts are usually
    /// ints but need not be (`Counter(a=1.5)` is legal), and CPython's
    /// `most_common` falls back to insertion order when the values are not
    /// mutually orderable — `Equal` here reproduces that, since the sort is
    /// stable.
    pub fn count_order(&self, a: &Value, b: &Value) -> std::cmp::Ordering {
        match (self.num_val(a), self.num_val(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// `+c` / `-c` on a `collections.Counter`. CPython defines them as
    /// `c - Counter()` and `Counter() - c`, so both inherit multiset
    /// subtraction's rule that non-positive counts are DROPPED:
    /// `+Counter(a=3, b=-1)` is `Counter({'a': 3})` and `-Counter(a=3, b=-1)` is
    /// `Counter({'b': 1})`. That pair is how a signed tally is split back into
    /// its gains and its losses. Returns `None` for anything but a Counter so
    /// the ordinary numeric unary paths still report their TypeError.
    fn counter_unary(&mut self, v: &Value, negate: bool) -> Option<Value> {
        let is_counter = match v {
            Value::Obj(i) => self.dict_meta.get(i).map(|m| m.kind) == Some(DictKind::Counter),
            _ => false,
        };
        if !is_counter {
            return None;
        }
        let entries: Vec<(PKey, Value, Value)> = match self.get(v) {
            Some(PyObj::Dict(m)) => m
                .iter()
                .map(|(k, (kv, cnt))| (k.clone(), kv.clone(), cnt.clone()))
                .collect(),
            _ => Vec::new(),
        };
        let mut out: IndexMap<PKey, (Value, Value)> = IndexMap::new();
        for (k, kv, cnt) in entries {
            // Counts need not be ints (`Counter(a=1.5)`), so negate through the
            // numeric `-` and test positivity numerically.
            let cnt = if negate {
                match self.arith(NumOp::Neg, &cnt, &Value::Undef) {
                    Ok(n) => n,
                    Err(_) => continue,
                }
            } else {
                cnt
            };
            if self.num_val(&cnt).is_some_and(|n| n > 0.0) {
                out.insert(k, (kv, cnt));
            }
        }
        let d = self.alloc(PyObj::Dict(out));
        if let Value::Obj(i) = d {
            self.dict_meta.insert(
                i,
                DictMeta {
                    kind: DictKind::Counter,
                    factory: None,
                },
            );
        }
        Some(d)
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
                // `+complex`/`+bignum` return the value unchanged.
                _ if matches!(self.get(v), Some(PyObj::Complex(..))) => Ok(v.clone()),
                _ if self.num_val(v).is_some() => Ok(v.clone()),
                _ => match self.counter_unary(v, false) {
                    Some(c) => Ok(c),
                    None => Err(type_error(&format!(
                        "bad operand type for unary +: '{}'",
                        self.type_name(v)
                    ))),
                },
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
                let w = self.next_arg_int(&arglist, &mut ai, false)?;
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
                    prec = Some(self.next_arg_int(&arglist, &mut ai, true)?.max(0) as usize);
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
            // CPython's message names WHERE the bad conversion character sits.
            let conv_at = i;
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
            let core = self
                .format_conv(
                    conv,
                    &val,
                    ConvFlags {
                        plus: f_plus,
                        space: f_space,
                        hash: f_hash,
                    },
                    prec,
                    premap,
                )
                .map_err(|e| locate_unsupported_conv(e, conv, conv_at))?;
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
                let w = self.next_arg_int(&arglist, &mut ai, false)?;
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
                    prec = Some(self.next_arg_int(&arglist, &mut ai, true)?.max(0) as usize);
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
            let conv_at = i;
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
                        // An out-of-range INT is an OverflowError with its own
                        // wording; the TypeError below is for a wrong TYPE.
                        if !(0..=255).contains(&cp) {
                            return Err("OverflowError: %c arg not in range(256)".into());
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
                    return Err(locate_unsupported_conv(
                        format!("ValueError: unsupported format character '{other}'"),
                        other,
                        conv_at,
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
    /// The next `%`-format argument read as a `Py_ssize_t` width or precision
    /// (the `*` forms). A bignum used to read as `0` — `'%*d' % (10**20, 1)`
    /// printed `1` — where CPython's `PyLong_AsSsize_t` raises.
    /// `%`'s `*` width is a `Py_ssize_t` and its `*` precision is a C `int`, and
    /// the two overflows name different C types.
    fn next_arg_int(&self, arglist: &[Value], ai: &mut usize, c_int: bool) -> Result<i64, String> {
        let v = arglist.get(*ai).cloned().unwrap_or(Value::Int(0));
        *ai += 1;
        let too_big = || {
            if c_int {
                "OverflowError: Python int too large to convert to C int".to_string()
            } else {
                "OverflowError: Python int too large to convert to C ssize_t".to_string()
            }
        };
        match self.index_fit(&v) {
            IndexFit::Fits(n) if c_int && (n > i32::MAX as i64 || n < i32::MIN as i64) => {
                Err(too_big())
            }
            IndexFit::Fits(n) => Ok(n),
            IndexFit::TooLarge(_) => Err(too_big()),
            IndexFit::NotInt => Ok(0),
        }
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
                    let ch = char::from_u32(cp as u32).ok_or_else(|| {
                        "OverflowError: %c arg not in range(0x110000)".to_string()
                    })?;
                    Ok(ch.to_string())
                } else if self.big_val(val).is_some() {
                    // An int too large for `as_int` is still an INT, so this is
                    // a range error and not a type error — `'%c' % 10**20` is
                    // `OverflowError`, the same as `'%c' % -1`.
                    Err("OverflowError: %c arg not in range(0x110000)".into())
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
                        // `PyNumber_Long` on a non-finite float raises rather
                        // than saturating; `f.trunc() as i64` would silently
                        // print `9223372036854775807` for `'%d' % inf`. The
                        // bignum conversion also keeps a large finite float
                        // exact (`'%d' % 1e30`), which an `i64` cast truncates.
                        Some(f) if f.is_nan() => {
                            return Err("ValueError: cannot convert float NaN to integer".into())
                        }
                        Some(f) if f.is_infinite() => {
                            return Err(
                                "OverflowError: cannot convert float infinity to integer".into()
                            )
                        }
                        Some(f) => {
                            use num_traits::FromPrimitive;
                            num_bigint::BigInt::from_f64(f.trunc()).unwrap_or_default()
                        }
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
                let core = if hash { alt_decimal_point(&core) } else { core };
                Ok(format!("{}{}", sign_str(neg), core))
            }
            other => Err(format!(
                "ValueError: unsupported format character '{other}'"
            )),
        }
    }
}

/// Add the code point and format-string index CPython appends to an unsupported
/// `%`-conversion. CPython's full message is
/// `unsupported format character 'z' (0x7a) at index 1`; pythonrs reported only
/// the character, so nothing said WHERE in the template the bad conversion was —
/// the one piece of the message that is useful in a long format string.
/// A non-`ValueError` (a type error from the conversion itself) passes through.
fn locate_unsupported_conv(err: String, conv: char, at: usize) -> String {
    let bare = format!("ValueError: unsupported format character '{conv}'");
    if err == bare {
        return format!(
            "ValueError: unsupported format character '{conv}' (0x{:x}) at index {at}",
            conv as u32
        );
    }
    err
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

/// The `#` alternate form on a FLOAT conversion (`Py_DTSF_ALT`): the output
/// always keeps a decimal point, even when the precision rounded every
/// fractional digit away — `'%#.0e' % 1.0` is `1.e+00` and `format(1.0, '#.0f')`
/// is `1.`.
///
/// The point goes right after the integer digits, so an exponent or a trailing
/// `%` stays behind it. A body with no digits at all (`inf`, `nan`) is returned
/// untouched: CPython prints those verbatim under `#`.
pub fn alt_decimal_point(s: &str) -> String {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'-' | b'+' | b' ')));
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start || b.get(i) == Some(&b'.') {
        return s.to_string();
    }
    format!("{}.{}", &s[..i], &s[i..])
}

/// `%g` / `%G`: choose `f` or `e` style by exponent, `precision` significant
/// digits (min 1), trailing zeros stripped unless the `#` flag is set.
///
/// Zero takes the SAME path as everything else. Short-circuiting it to `"0"`
/// looks harmless and is wrong twice over: it drops the sign of `-0.0`
/// (`format(-0.0, 'g')` is `-0`, not `0`) and it ignores `#`
/// (`format(0.0, '#g')` is `0.00000`). `{:e}` of a zero reports exponent 0, so
/// the general branch already produces both correctly.
pub fn fmt_g(x: f64, prec: usize, upper: bool, hash: bool) -> String {
    let p = prec.max(1);
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

/// Format a float with a precision but NO presentation type (`format(x, '.3')`,
/// `f"{x:.3}"`). This is CPython's "general" float format: like `'g'` with two
/// differences — it switches to scientific one exponent sooner (`exp >= p-1`
/// instead of `exp >= p`), and a result that renders as a bare integer keeps a
/// trailing `.0` (`Py_DTSF_ADD_DOT_0`), so `format(2.0, '.3')` is `'2.0'`, not
/// `'2'`. Trailing zeros are otherwise stripped as in `'g'`, unless `alt` (the
/// `#` flag) keeps them: `format(2.0, '#.3')` is `'2.00'`.
pub fn fmt_none_float(x: f64, prec: usize, alt: bool) -> String {
    let p = prec.max(1);
    // Exponent of `x` *after* rounding to `p` significant figures, so a carry
    // (`9.99` → `10.` at `p=2`) bumps the exponent and the scientific-vs-fixed
    // decision sees the rounded magnitude, matching CPython.
    let exp: i32 = if x == 0.0 {
        0
    } else {
        fmt_sci(x, p - 1, false)
            .split_once(['e', 'E'])
            .and_then(|(_, e)| e.parse().ok())
            .unwrap_or(0)
    };
    let mut s = if exp < -4 || exp >= p as i32 - 1 {
        let sci = fmt_sci(x, p - 1, false);
        if alt {
            sci
        } else {
            strip_g_sci(&sci)
        }
    } else {
        let dec = (p as i32 - 1 - exp).max(0) as usize;
        let s = format!("{:.*}", dec, x);
        if s.contains('.') && !alt {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if !s.contains('.') && !s.contains(['e', 'E']) {
        s.push_str(".0");
    }
    s
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
    /// Thin wrapper: an unhashable-key failure gets CPython's container
    /// context (`cannot use 'X' as a dict key (...)`). See
    /// [`wrap_unhashable`]. One place per op, so no call site can forget.
    pub fn get_item(&mut self, recv: &Value, idx: &Value) -> Result<Value, String> {
        let r = self.get_item_raw(recv, idx);
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(wrap_unhashable(self, e, KeyRole::Of(recv), idx)),
        }
    }

    fn get_item_raw(&mut self, recv: &Value, idx: &Value) -> Result<Value, String> {
        // A module `__dict__` view answers reads from a snapshot of the slot.
        if let Some(d) = self.module_dict_snapshot(recv) {
            return self.get_item(&d, idx);
        }
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::get_item(self, id, idx);
        }
        // Slice?
        if let Some(PyObj::Slice { lo, hi, step }) = self.get(idx) {
            let (lo, hi, step) = (lo.clone(), hi.clone(), step.clone());
            return self.get_slice(recv, &lo, &hi, &step);
        }
        // A `struct_time` indexes as its 9-element sequence (`t[0]` == `tm_year`).
        if let Some(PyObj::StructTime { fields }) = self.get(recv) {
            let seq: Vec<Value> = fields.iter().take(9).cloned().collect();
            let t = self.new_tuple(seq);
            return self.get_item(&t, idx);
        }
        match self.get(recv) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => {
                let is_tuple = matches!(self.get(recv), Some(PyObj::Tuple(_)));
                let n = l.len() as i64;
                let i = self.seq_index(idx, || {
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
                // CPython 3.11 added the offending type; the bare form is the
                // 3.9/3.10 wording.
                let i = self.seq_index(idx, || {
                    type_error(&format!(
                        "string indices must be integers, not '{}'",
                        self.type_name(idx)
                    ))
                })?;
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
            // A mappingproxy indexes through to its backing dict (read-only).
            Some(PyObj::MappingProxy { dict }) => {
                let dict = dict.clone();
                self.get_item(&dict, idx)
            }
            Some(PyObj::Range { start, step, .. }) => {
                let (start, step) = (*start, *step);
                let len = match self.get(recv) {
                    Some(PyObj::Range { start, stop, step }) => range_len(*start, *stop, *step),
                    _ => 0,
                };
                // `range` computes its index in arbitrary precision
                // (`compute_range_item`), so a bignum subscript is simply out of
                // range rather than an `IndexError: cannot fit 'int' …`.
                let i = match self.index_fit(idx) {
                    IndexFit::Fits(n) => n,
                    IndexFit::TooLarge(_) => {
                        return Err("IndexError: range object index out of range".into())
                    }
                    IndexFit::NotInt => return Err(type_error("range indices must be integers")),
                };
                let k = if i < 0 { i + len } else { i };
                if k < 0 || k >= len {
                    return Err("IndexError: range object index out of range".into());
                }
                Ok(Value::Int(start + k * step))
            }
            Some(PyObj::BigRange { start, stop, step }) => {
                let (start, stop, step) = (start.clone(), stop.clone(), step.clone());
                let len = big_range_len(&start, &stop, &step);
                let i = self
                    .big_val(idx)
                    .ok_or_else(|| type_error("range indices must be integers"))?;
                let k = if i < num_bigint::BigInt::from(0) {
                    &i + &len
                } else {
                    i
                };
                if k < num_bigint::BigInt::from(0) || k >= len {
                    return Err("IndexError: range object index out of range".into());
                }
                Ok(self.norm_big(start + k * step))
            }
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => {
                // `bytes` reports the bare message; `bytearray` names the type.
                let is_ba = matches!(self.get(recv), Some(PyObj::Bytearray(_)));
                let n = b.len() as i64;
                let i = self.seq_index(idx, || type_error("byte indices must be integers"))?;
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
                let i = self.seq_index(idx, || type_error("deque indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: deque index out of range".into());
                }
                Ok(items[k as usize].clone())
            }
            Some(PyObj::Memoryview { .. }) => {
                let bytes = self.mv_bytes(recv);
                let n = bytes.len() as i64;
                let i = self.seq_index(idx, || type_error("memoryview: invalid slice key"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: index out of bounds on dimension 1".into());
                }
                Ok(Value::Int(bytes[k as usize] as i64))
            }
            // `typing.Union[X, Y]`. Since 3.14 `typing.Union` IS `types.UnionType`,
            // so subscripting it builds exactly what `X | Y` builds — same flatten,
            // same dedupe, same collapse-to-one. `Union[int]` is `int`, and a Union
            // of nothing is an error rather than an empty union.
            Some(PyObj::Builtin(n)) if n == "_typing.Union" => {
                let items = match self.get(idx) {
                    Some(PyObj::Tuple(xs)) => xs.clone(),
                    _ => vec![idx.clone()],
                };
                if items.is_empty() {
                    return Err(type_error("Cannot take a Union of no types."));
                }
                Ok(self.build_union(items))
            }
            // A type object that does not parameterize names ITSELF in the
            // message (`type 'Box' is not subscriptable`), not its metaclass —
            // `'type' object is not subscriptable` tells the reader nothing about
            // which class they subscripted.
            _ => {
                let msg = match self.get(recv) {
                    Some(PyObj::Class(n)) => format!("type '{n}' is not subscriptable"),
                    Some(PyObj::Builtin(n))
                        if crate::builtins::BUILTIN_TYPES.contains(&n.as_str()) =>
                    {
                        format!("type '{n}' is not subscriptable")
                    }
                    _ => format!("'{}' object is not subscriptable", self.type_name(recv)),
                };
                Err(type_error(&msg))
            }
        }
    }

    /// Flatten nested unions, drop duplicates, and collapse a one-member result to
    /// that member. Shared by PEP 604 `X | Y` and `typing.Union[X, Y]` so the two
    /// spellings can never disagree.
    fn build_union(&mut self, items: Vec<Value>) -> Value {
        let mut args: Vec<Value> = Vec::with_capacity(items.len());
        for item in items {
            // A nested union contributes its members, not itself.
            let members = match self.get(&item) {
                Some(PyObj::Union { args }) => args.clone(),
                _ => vec![item],
            };
            for m in members {
                if !args.iter().any(|x| self.equal(x, &m)) {
                    args.push(m);
                }
            }
        }
        if args.len() == 1 {
            return args.into_iter().next().unwrap();
        }
        self.alloc(PyObj::Union { args })
    }

    fn get_slice(
        &mut self,
        recv: &Value,
        lo: &Value,
        hi: &Value,
        step: &Value,
    ) -> Result<Value, String> {
        let step = self.as_slice_index(step).unwrap_or(1);
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
    /// Thin wrapper: an unhashable-key failure gets CPython's container
    /// context (`cannot use 'X' as a dict key (...)`). See
    /// [`wrap_unhashable`]. One place per op, so no call site can forget.
    pub fn set_item(&mut self, recv: &Value, idx: &Value, val: Value) -> Result<(), String> {
        let r = self.set_item_raw(recv, idx, val);
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(wrap_unhashable(self, e, KeyRole::Of(recv), idx)),
        }
    }

    fn set_item_raw(&mut self, recv: &Value, idx: &Value, val: Value) -> Result<(), String> {
        // A module `__dict__` view writes THROUGH to the module's globals, so the
        // module's own functions see the new binding.
        if let Some(slot) = self.module_dict_slot(recv) {
            let k = self
                .as_str(idx)
                .ok_or_else(|| type_error("module namespace keys must be strings"))?;
            self.module_globals[slot].insert(k, val);
            return Ok(());
        }
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::set_item(self, id, idx, &val);
        }
        match self.get(recv) {
            Some(PyObj::List(l)) => {
                let n = l.len() as i64;
                let i = self.seq_index(idx, || type_error("list indices must be integers"))?;
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
                let i = self.seq_index(idx, || type_error("bytearray indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: bytearray index out of range".into());
                }
                // `bytearray[i] = huge` is a RANGE error, not a type one:
                // `PyNumber_AsSsize_t` succeeds on any int and the 0..256 check
                // is what rejects it.
                let v = match self.index_fit(&val) {
                    IndexFit::Fits(v) => v,
                    IndexFit::TooLarge(_) => {
                        return Err("ValueError: byte must be in range(0, 256)".into())
                    }
                    IndexFit::NotInt => {
                        return Err(type_error(&format!(
                            "'{}' object cannot be interpreted as an integer",
                            self.type_name(&val)
                        )))
                    }
                };
                if !(0..=255).contains(&v) {
                    return Err("ValueError: byte must be in range(0, 256)".into());
                }
                if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                    b[k as usize] = v as u8;
                }
                Ok(())
            }
            // `mv[i] = int` — one byte written THROUGH the view into its backing
            // `bytearray`. A view is a window, not a copy, so the store lands in
            // the backing object and every other view over it sees it.
            Some(PyObj::Memoryview {
                obj,
                start,
                len,
                readonly,
            }) => {
                let (obj, start, len, readonly) = (obj.clone(), *start, *len, *readonly);
                if readonly {
                    return Err(type_error("cannot modify read-only memory"));
                }
                let i = self.seq_index(idx, || type_error("memoryview: invalid slice key"))?;
                let k = if i < 0 { i + len as i64 } else { i };
                if k < 0 || k >= len as i64 {
                    return Err("IndexError: index out of bounds on dimension 1".into());
                }
                let b = self.mv_byte_value(&val)?;
                if let Some(PyObj::Bytearray(buf)) = self.get_mut(&obj) {
                    if let Some(slot) = buf.get_mut(start + k as usize) {
                        *slot = b;
                    }
                }
                Ok(())
            }
            _ => Err(type_error(&format!(
                "'{}' object does not support item assignment",
                self.type_name(recv)
            ))),
        }
    }

    /// One `format 'B'` element: an `__index__`-able value inside `0..=255`.
    ///
    /// CPython splits the two rejections, and the split is not cosmetic — a
    /// non-integer is a TYPE error (`memoryview_setitem` never gets a value out
    /// of `PyNumber_Index`) while an out-of-range integer is a VALUE error (the
    /// index succeeded and the `unsigned char` pack is what refused it). A
    /// bignum is out of range, not a non-integer, so it takes the ValueError.
    fn mv_byte_value(&self, val: &Value) -> Result<u8, String> {
        const BAD_VALUE: &str = "ValueError: memoryview: invalid value for format 'B'";
        match self.index_fit(val) {
            IndexFit::Fits(v) if (0..=255).contains(&v) => Ok(v as u8),
            IndexFit::Fits(_) | IndexFit::TooLarge(_) => Err(BAD_VALUE.into()),
            IndexFit::NotInt => Err(type_error("memoryview: invalid type for format 'B'")),
        }
    }

    /// `mv[lo:hi:step] = bytes-like`, with the replacement already flattened to
    /// raw bytes by the caller (a `memoryview` slice store takes a BUFFER, never
    /// an arbitrary iterable, so the caller must not have iterated it).
    ///
    /// CPython requires the two sides to select the same number of elements for
    /// EVERY step, contiguous included: a view has a fixed length and cannot
    /// splice, so `mv[1:3] = b'Y'` is an error where `list`'s equivalent is a
    /// resize.
    pub fn set_memoryview_slice(
        &mut self,
        recv: &Value,
        idx: &Value,
        repl: Vec<u8>,
    ) -> Result<(), String> {
        let Some(PyObj::Memoryview {
            obj,
            start,
            len,
            readonly,
        }) = self.get(recv)
        else {
            return Err(type_error("expected a memoryview"));
        };
        let (obj, start, len, readonly) = (obj.clone(), *start, *len, *readonly);
        if readonly {
            return Err(type_error("cannot modify read-only memory"));
        }
        let (lo, hi, step) = match self.get(idx) {
            Some(PyObj::Slice { lo, hi, step }) => (lo.clone(), hi.clone(), step.clone()),
            _ => return Err(type_error("expected a slice")),
        };
        let step = self.as_slice_index(&step).unwrap_or(1);
        if step == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        let picks = self.slice_indices(&lo, &hi, step, len as i64);
        if picks.len() != repl.len() {
            return Err(
                "ValueError: memoryview assignment: lvalue and rvalue have different structures"
                    .into(),
            );
        }
        if let Some(PyObj::Bytearray(buf)) = self.get_mut(&obj) {
            for (k, b) in picks.iter().zip(repl) {
                if let Some(slot) = buf.get_mut(start + *k as usize) {
                    *slot = b;
                }
            }
        }
        Ok(())
    }

    /// Thin wrapper: an unhashable-key failure gets CPython's container
    /// context (`cannot use 'X' as a dict key (...)`). See
    /// [`wrap_unhashable`]. One place per op, so no call site can forget.
    pub fn del_item(&mut self, recv: &Value, idx: &Value) -> Result<(), String> {
        let r = self.del_item_raw(recv, idx);
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(wrap_unhashable(self, e, KeyRole::Of(recv), idx)),
        }
    }

    fn del_item_raw(&mut self, recv: &Value, idx: &Value) -> Result<(), String> {
        if let Some(slot) = self.module_dict_slot(recv) {
            let k = self
                .as_str(idx)
                .ok_or_else(|| type_error("module namespace keys must be strings"))?;
            return match self.module_globals[slot].shift_remove(&k) {
                Some(_) => Ok(()),
                None => {
                    let kv = self.new_str(k);
                    Err(self.key_error(&kv))
                }
            };
        }
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::del_item(self, id, idx);
        }
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
                let i = self.seq_index(idx, || type_error("list indices must be integers"))?;
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
                let i = self.seq_index(idx, || type_error("bytearray indices must be integers"))?;
                let k = if i < 0 { i + n } else { i };
                if k < 0 || k >= n {
                    return Err("IndexError: bytearray index out of range".into());
                }
                if let Some(PyObj::Bytearray(b)) = self.get_mut(recv) {
                    b.remove(k as usize);
                }
                Ok(())
            }
            // A view has a fixed length, so there is nothing a delete could mean;
            // CPython refuses it by name rather than with the generic text.
            Some(PyObj::Memoryview { .. }) => Err(type_error("cannot delete memory")),
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
        let step = self.as_slice_index(&step).unwrap_or(1);
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
        let step = self.as_slice_index(step).unwrap_or(1);
        if step == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        let n = match self.get(recv) {
            Some(PyObj::List(l)) => l.len() as i64,
            Some(PyObj::Bytearray(b)) => b.len() as i64,
            // A view is fixed-length, so a slice delete is refused by the same
            // name as a single-element one rather than by the generic text.
            Some(PyObj::Memoryview { .. }) => return Err(type_error("cannot delete memory")),
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
        // A `_csv` reader drains to its remaining rows.
        if let Some(PyObj::CsvReader { rows, idx, .. }) = self.get(v) {
            let out = rows[(*idx).min(rows.len())..].to_vec();
            if let Some(PyObj::CsvReader { rows, idx, .. }) = self.get_mut(v) {
                *idx = rows.len();
            }
            return Ok(out);
        }
        if let Some(d) = self.module_dict_snapshot(v) {
            return self.iter_items(&d);
        }
        // Iterating an in-memory stream yields its remaining lines.
        if matches!(
            self.get(v),
            Some(PyObj::BytesIO { .. }) | Some(PyObj::StringIO { .. })
        ) {
            if let Some(r) = crate::stdlib::pyio::stream_lines(self, v) {
                return r;
            }
        }
        // A `Template` iterates as its literal pieces and interpolations, in
        // source order, with EMPTY literals skipped — so a consumer can walk a
        // template without special-casing the gaps between adjacent fields.
        if let Some(PyObj::Template {
            strings,
            interpolations,
        }) = self.get(v)
        {
            let (strings, interps) = (strings.clone(), interpolations.clone());
            let mut out = Vec::with_capacity(strings.len() + interps.len());
            for (i, s) in strings.iter().enumerate() {
                if !s.is_empty() {
                    let sv = self.new_str(s.clone());
                    out.push(sv);
                }
                if let Some(interp) = interps.get(i) {
                    out.push(interp.clone());
                }
            }
            return Ok(out);
        }
        // Iterating a file yields its remaining lines (each keeping its `\n`).
        // Read first (drops the immutable borrow) so `new_str` can borrow `&mut`.
        let file_id = match self.get(v) {
            Some(PyObj::File { id }) => Some(*id),
            _ => None,
        };
        if let Some(id) = file_id {
            if self.io_is_binary(id) {
                let lines = self.io_read_lines_bytes(id)?;
                return Ok(lines
                    .into_iter()
                    .map(|l| self.alloc(PyObj::Bytes(l)))
                    .collect());
            }
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
            Some(PyObj::StructTime { fields }) => Ok(fields.iter().take(9).cloned().collect()),
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
            Some(PyObj::BigRange { start, stop, step }) => {
                use num_traits::{ToPrimitive, Zero};
                // `list(range(10**25))` looped forever building a vector nothing
                // could hold — no panic, no error, just an unkillable process.
                // CPython asks the range for its length first
                // (`PyObject_LengthHint` -> `range.__len__` -> `PyLong_AsSsize_t`)
                // and refuses there.
                if big_range_len(start, stop, step).to_usize().is_none() {
                    return Err(
                        "OverflowError: Python int too large to convert to C ssize_t".to_string(),
                    );
                }
                let (stop, step) = (stop.clone(), step.clone());
                let pos = step > num_bigint::BigInt::zero();
                let mut out = Vec::new();
                let mut c = start.clone();
                loop {
                    let go = if pos { c < stop } else { c > stop };
                    if !go {
                        break;
                    }
                    out.push(self.norm_big(c.clone()));
                    c += &step;
                }
                Ok(out)
            }
            // Iterating a mappingproxy yields its backing dict's keys.
            Some(PyObj::MappingProxy { dict }) => {
                let dict = dict.clone();
                Ok(match self.get(&dict) {
                    Some(PyObj::Dict(d)) => d.values().map(|(k, _)| k.clone()).collect(),
                    _ => Vec::new(),
                })
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
            let kind = self.view_iter_kind(v);
            return Ok(self.new_iter_kind(items, kind));
        }
        // A builtin-subclass instance with no user `__iter__` (a namedtuple, a
        // `list`/`tuple`/`dict`/`str` subclass) iterates its native payload. Reached
        // when the instance is passed to `zip`/`map`/`enumerate` — their lazy
        // iterators call `make_iter` directly, unlike `list()`, which routes through
        // `iter_vec`/`iter_instance_items`.
        if let Some((payload, class)) = match self.get(v) {
            Some(PyObj::Instance(i)) if !matches!(i.payload, Value::Undef) => {
                Some((i.payload.clone(), i.class.clone()))
            }
            _ => None,
        } {
            if self.builtin_base_of(&class).is_some()
                && self.class_lookup(&class, "__iter__").is_none()
            {
                return self.make_iter(&payload);
            }
        }
        let state = match self.get(v) {
            Some(PyObj::List(l)) => IterState::Seq {
                items: l.clone(),
                idx: 0,
                kind: IterKind::List,
            },
            Some(PyObj::Tuple(l)) => IterState::Seq {
                items: l.clone(),
                idx: 0,
                kind: IterKind::Tuple,
            },
            // `time.struct_time` is a tuple subclass, so CPython hands back the
            // plain tuple iterator.
            Some(PyObj::StructTime { fields }) => IterState::Seq {
                items: fields.iter().take(9).cloned().collect(),
                idx: 0,
                kind: IterKind::Tuple,
            },
            Some(PyObj::Str(s)) => {
                let kind = IterKind::of_str(s);
                IterState::Seq {
                    items: s
                        .chars()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(PyObj::Str)
                        .map(|o| self.alloc(o))
                        .collect(),
                    idx: 0,
                    kind,
                }
            }
            Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => IterState::Seq {
                items: self.set_ordered_values(s),
                idx: 0,
                kind: IterKind::Set,
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
            Some(PyObj::BigRange { start, stop, step }) => IterState::BigRangeIter {
                cur: start.clone(),
                stop: stop.clone(),
                step: step.clone(),
            },
            Some(PyObj::Iter(_))
            | Some(PyObj::CsvReader { .. })
            | Some(PyObj::Generator { .. })
            | Some(PyObj::Zip { .. })
            | Some(PyObj::MapObj { .. })
            | Some(PyObj::FilterObj { .. })
            | Some(PyObj::EnumerateObj { .. })
            | Some(PyObj::ItertoolsIter { .. })
            | Some(PyObj::CallIter { .. }) => return Ok(v.clone()),
            _ => {
                // Everything else walks its elements through one snapshot cursor;
                // the tag is what keeps `type(it).__name__` CPython's.
                let kind = match self.get(v) {
                    Some(PyObj::Bytes(_)) => IterKind::Bytes,
                    Some(PyObj::Bytearray(_)) => IterKind::Bytearray,
                    Some(PyObj::Memoryview { .. }) => IterKind::Memory,
                    Some(PyObj::Deque { .. }) => IterKind::Deque,
                    _ => IterKind::Seq,
                };
                let items = self.iter_items(v)?;
                IterState::Seq {
                    items,
                    idx: 0,
                    kind,
                }
            }
        };
        Ok(self.alloc(PyObj::Iter(state)))
    }

    /// The CPython iterator type for `iter(view)` on a `dict_keys`/`dict_values`/
    /// `dict_items` view. Not a view → the `__getitem__` fallback name.
    pub fn view_iter_kind(&mut self, v: &Value) -> IterKind {
        match self.get(v) {
            Some(PyObj::DictView { kind, .. }) => match kind {
                0 => IterKind::DictKey,
                1 => IterKind::DictValue,
                _ => IterKind::DictItem,
            },
            _ => IterKind::Seq,
        }
    }

    /// Advance an iterator; `None` on exhaustion.
    pub fn iter_next(&mut self, it: &Value) -> Result<Option<Value>, String> {
        // A `_csv` reader yields its parsed rows, advancing `line_num` as it goes.
        if let Some(PyObj::CsvReader { rows, idx, .. }) = self.get_mut(it) {
            if *idx >= rows.len() {
                return Ok(None);
            }
            let row = rows[*idx].clone();
            *idx += 1;
            return Ok(Some(row));
        }
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(it) {
            return crate::ffi::iter_next(self, id);
        }
        // Bignum range iterator: clone the state out (releasing the borrow),
        // advance, write back, then normalize the yielded value — `norm_big`
        // needs `&mut self`, so it cannot run inside the `get_mut` match below.
        if let Some(PyObj::Iter(IterState::BigRangeIter { cur, stop, step })) = self.get(it) {
            use num_traits::Zero;
            let (cur, stop, step) = (cur.clone(), stop.clone(), step.clone());
            let go = if step > num_bigint::BigInt::zero() {
                cur < stop
            } else {
                cur > stop
            };
            if !go {
                return Ok(None);
            }
            if let Some(PyObj::Iter(IterState::BigRangeIter { cur: c, .. })) = self.get_mut(it) {
                *c = &cur + &step;
            }
            return Ok(Some(self.norm_big(cur)));
        }
        let out = match self.get_mut(it) {
            Some(PyObj::Iter(IterState::Seq { items, idx, .. })) => {
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
    /// Thin wrapper: an unhashable-key failure gets CPython's container
    /// context (`cannot use 'X' as a dict key (...)`). See
    /// [`wrap_unhashable`]. One place per op, so no call site can forget.
    pub fn contains(&mut self, item: &Value, container: &Value) -> Result<bool, String> {
        let r = self.contains_raw(item, container);
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(wrap_unhashable(self, e, KeyRole::Of(container), item)),
        }
    }

    fn contains_raw(&mut self, item: &Value, container: &Value) -> Result<bool, String> {
        if let Some(d) = self.module_dict_snapshot(container) {
            return self.contains(item, &d);
        }
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
            // A mappingproxy tests membership over its backing dict's keys.
            Some(PyObj::MappingProxy { dict }) => {
                let dict = dict.clone();
                self.contains(item, &dict)
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
                // `x in y` on something with neither `__contains__` nor
                // `__iter__` gets a message about CONTAINMENT, not about
                // iteration: pythonrs reported the iteration failure verbatim
                // (`'int' object is not iterable`), which CPython raises for
                // `for _ in y`, not for `x in y`.
                let items = self.iter_items(container).map_err(|e| {
                    if e.ends_with("object is not iterable") {
                        type_error(&format!(
                            "argument of type '{}' is not a container or iterable",
                            self.type_name(container)
                        ))
                    } else {
                        e
                    }
                })?;
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

/// Fallibly reserve `unit * count` slots for a sequence repetition.
///
/// `bytes`/`bytearray` report CPython's own `OverflowError: repeated bytes are
/// too long` when the product does not fit; every other sequence reports
/// `MemoryError`, which is also what a reservation the allocator refuses gives.
trait RepeatBuf {
    fn try_reserve_slots(&mut self, n: usize) -> Result<(), ()>;
}
impl RepeatBuf for String {
    fn try_reserve_slots(&mut self, n: usize) -> Result<(), ()> {
        self.try_reserve_exact(n).map_err(|_| ())
    }
}
impl<T> RepeatBuf for Vec<T> {
    fn try_reserve_slots(&mut self, n: usize) -> Result<(), ()> {
        self.try_reserve_exact(n).map_err(|_| ())
    }
}

fn reserve_repeat(
    buf: &mut impl RepeatBuf,
    unit: usize,
    count: usize,
    bytes_like: bool,
) -> Result<(), String> {
    let total = unit.checked_mul(count).ok_or_else(|| {
        if bytes_like {
            "OverflowError: repeated bytes are too long".to_string()
        } else {
            "MemoryError".to_string()
        }
    })?;
    buf.try_reserve_slots(total).map_err(|_| {
        if bytes_like {
            "OverflowError: repeated bytes are too long".to_string()
        } else {
            "MemoryError".to_string()
        }
    })
}

/// Fill `buf` with as many bytes as the reader has, truncating it to what was
/// actually read. `Read::read` may return short, so it is called until it says
/// zero — `f.read(n)` promises n bytes unless the stream ended.
fn read_up_to(r: &mut impl std::io::Read, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            k => filled += k,
        }
    }
    buf.truncate(filled);
    Ok(())
}

fn slice_bounds(lo: &Value, hi: &Value, step: i64, n: i64, h: &PyHost) -> (i64, i64) {
    let lower = if step < 0 { -1 } else { 0 };
    let upper = if step < 0 { n - 1 } else { n };
    let adjust = |x: i64| -> i64 {
        let x = if x < 0 { x.saturating_add(n) } else { x };
        x.clamp(lower, upper)
    };
    let start = match h.as_slice_index(lo) {
        Some(x) => adjust(x),
        None => {
            if step < 0 {
                n - 1
            } else {
                0
            }
        }
    };
    let stop = match h.as_slice_index(hi) {
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

    /// Reconstruct a base/MRO entry from its name: a user-defined class (present
    /// in `self.classes`) becomes a `Class`; any other name is a builtin type
    /// (`int`, `str`, `Exception`, …) and must become its `Builtin` type object,
    /// so `base.__dict__`, identity, and `isinstance` match the real builtin.
    fn class_or_builtin_type(&mut self, name: String) -> Value {
        if self.classes.contains_key(&name) {
            self.alloc(PyObj::Class(name))
        } else {
            self.alloc(PyObj::Builtin(name))
        }
    }

    /// Build a `TypeVar`/`ParamSpec`/`TypeVarTuple` object (a `_typing`
    /// primitive). The dunder attributes `typing.py` reads (`__name__`,
    /// `__bound__`, `__constraints__`, `__covariant__`, `__contravariant__`,
    /// `__infer_variance__`, `__default__`) are stored in a backing dict; unset
    /// keyword arguments take their CPython defaults.
    pub fn make_type_var(
        &mut self,
        kind: TypeVarKind,
        name: String,
        constraints: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Value {
        let get = |kwargs: &[(String, Value)], k: &str| {
            kwargs
                .iter()
                .find(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
        };
        let name_v = self.new_str(name.clone());
        let bound = get(&kwargs, "bound").unwrap_or(Value::Undef);
        let covariant = get(&kwargs, "covariant").unwrap_or(Value::Bool(false));
        let contravariant = get(&kwargs, "contravariant").unwrap_or(Value::Bool(false));
        let infer_variance = get(&kwargs, "infer_variance").unwrap_or(Value::Bool(false));
        // `default` unset → the `NoDefault` sentinel (same object `_typing` exports).
        let default = get(&kwargs, "default").unwrap_or_else(|| {
            self.cached_module("_typing")
                .and_then(|m| self.get_attr(&m, "NoDefault").ok())
                .unwrap_or(Value::Undef)
        });
        let constraints_tuple = self.new_tuple(constraints);
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
        for (k, v) in [
            ("__name__", name_v),
            ("__bound__", bound),
            ("__constraints__", constraints_tuple),
            ("__covariant__", covariant),
            ("__contravariant__", contravariant),
            ("__infer_variance__", infer_variance),
            ("__default__", default),
        ] {
            let kv = self.new_str(k.to_string());
            d.insert(PKey::Str(k.to_string()), (kv, v));
        }
        let attrs = self.new_dict(d);
        self.alloc(PyObj::TypeVarLike { kind, name, attrs })
    }

    /// If `v` is a class whose metaclass (other than plain `type`) defines the
    /// method `name`, return that method value (unbound). Used to dispatch a
    /// metaclass dunder against the class object itself — e.g. iterating an Enum
    /// subclass runs `type(cls).__iter__(cls)`.
    pub fn metaclass_method(&self, v: &Value, name: &str) -> Option<Value> {
        let cname = match self.get(v) {
            Some(PyObj::Class(c)) => c.clone(),
            _ => return None,
        };
        let meta = self.classes.get(&cname).map(|cd| cd.metaclass.clone())?;
        if meta.is_empty() || meta == "type" {
            return None;
        }
        self.class_lookup(&meta, name)
    }

    /// The MRO of `class`, memoized. Callers that only read it should prefer this
    /// over [`Self::mro_of`], which hands back an owned copy.
    pub fn mro_rc(&self, class: &str) -> std::rc::Rc<Vec<String>> {
        if let Some(hit) = self.mro_cache.borrow().get(class) {
            return hit.clone();
        }
        let computed = std::rc::Rc::new(self.compute_mro(class));
        self.mro_cache
            .borrow_mut()
            .insert(class.to_string(), computed.clone());
        computed
    }

    pub fn mro_of(&self, class: &str) -> Vec<String> {
        (*self.mro_rc(class)).clone()
    }

    fn compute_mro(&self, class: &str) -> Vec<String> {
        let bases: Vec<String> = self
            .classes
            .get(class)
            .map(|cd| cd.bases.clone())
            .unwrap_or_default();
        if bases.is_empty() {
            return vec![class.to_string()];
        }
        let mut seqs: Vec<Vec<String>> = bases.iter().map(|b| (*self.mro_rc(b)).clone()).collect();
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
        // `mro_rc` rather than `mro_of`: this runs on every attribute read, and
        // copying the name vector per lookup was most of its cost.
        for c in self.mro_rc(class).iter() {
            if let Some(cd) = self.classes.get(c) {
                if let Some(v) = cd.ns.get(name) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// `recv.name`, remembering the receiver when the lookup misses so an
    /// uncaught `AttributeError` can render CPython's "Did you mean" hint. The
    /// candidates are the receiver's `dir()`, which is gone by the time the
    /// traceback renders; this keeps only the receiver, and only on the error
    /// path.
    pub fn get_attr(&mut self, recv: &Value, name: &str) -> Result<Value, String> {
        let r = self.get_attr_inner(recv, name);
        if r.is_err() {
            self.note_attr_miss(recv, name);
        }
        r
    }

    /// Record the receiver of a missed attribute lookup for the hint.
    pub fn note_attr_miss(&mut self, recv: &Value, name: &str) {
        let self_obj = self.frames.last().and_then(|f| f.self_obj.clone());
        self.suggest = Some(SuggestCtx::Attr {
            wrong: name.to_string(),
            recv: recv.clone(),
            self_obj,
        });
    }

    /// Remember the scope a bare-name read missed in, for the same hint.
    pub fn note_name_miss(&mut self, name: &str) {
        let Some(frame) = self.frames.last() else {
            return;
        };
        // `locals_set` as well as the env: a slotted local never reaches the
        // environment, so `def f(): counter = 1; print(countr)` would otherwise
        // have no candidate to match against.
        let mut slotted: Vec<String> = frame.locals_set.iter().cloned().collect();
        slotted.sort();
        let (env, self_obj) = (frame.env.clone(), frame.self_obj.clone());
        self.suggest = Some(SuggestCtx::Name {
            wrong: name.to_string(),
            env,
            slotted,
            module: self.cur_module,
            self_obj,
        });
    }

    /// CPython's `_compute_suggestion_error` for a terse `Type: message` line:
    /// the closest name to the one that missed, or `None` when nothing is close
    /// enough (or the recorded context belongs to a different error).
    fn suggestion_for(&self, line: &str) -> Option<String> {
        match self.suggest.as_ref()? {
            SuggestCtx::Name {
                wrong,
                env,
                slotted,
                module,
                self_obj,
            } => {
                if !line.starts_with("NameError:") || !line.contains(&format!("'{wrong}'")) {
                    return None;
                }
                // A bare name that IS an attribute of the running method's
                // instance is reported as `self.<name>` rather than a near miss.
                if let Some(obj) = self_obj {
                    if self.dir_names(obj).iter().any(|n| n == wrong) {
                        return Some(format!("self.{wrong}"));
                    }
                }
                // CPython's candidates: the frame's locals, then its globals,
                // then the builtins — in that order, since ties go to the first.
                let mut candidates: Vec<String> = slotted.clone();
                let mut scope = Some(env.clone());
                while let Some(s) = scope {
                    candidates.extend(s.borrow().vars.keys().cloned());
                    scope = s.borrow().parent.clone();
                }
                candidates.extend(self.module_globals[*module].keys().cloned());
                // Sorted: CPython's `f_builtins` is the `builtins` module dict,
                // whose order pythonrs's tables do not reproduce, and equally
                // close candidates are decided by which comes first (`st`
                // suggests `set`, not `str`).
                let mut builtins: Vec<String> = crate::builtins::builtin_names()
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                builtins.sort();
                candidates.extend(builtins);
                crate::suggest::closest(&candidates, wrong)
            }
            SuggestCtx::Attr {
                wrong,
                recv,
                self_obj,
            } => {
                if !line.starts_with("AttributeError:") || !line.contains(&format!("'{wrong}'")) {
                    return None;
                }
                let mut candidates = self.dir_names(recv);
                // Private names are hidden unless the code asked for one, or the
                // receiver is the running method's own instance.
                let own = matches!(self_obj, Some(o) if o == recv);
                if !wrong.starts_with('_') && !own {
                    candidates.retain(|n| !n.starts_with('_'));
                }
                candidates.sort();
                crate::suggest::closest(&candidates, wrong)
            }
        }
    }

    fn get_attr_inner(&mut self, recv: &Value, name: &str) -> Result<Value, String> {
        // A CPython object (stdlib-ffi) resolves attributes on the CPython side.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::get_attr(self, id, name);
        }
        // Type dunders on a scalar value (None/bool/int/float): `__new__` (the
        // inherited object.__new__) and `__class__`. The stdlib inspects these
        // (enum's `_find_new_` reads `None.__new__`).
        if !matches!(recv, Value::Obj(_)) {
            match name {
                "__new__" => return Ok(self.alloc(PyObj::Builtin("object.__new__".into()))),
                "__class__" => {
                    let tn = self.type_name(recv);
                    return Ok(self.alloc(PyObj::Builtin(tn)));
                }
                _ => {}
            }
        }
        // namedtuple field access: a tagged tuple resolves `.field` to its index,
        // and `._fields` to the field-name tuple.
        if let Value::Obj(i) = recv {
            if let Some(fields) = self.nt_meta.get(i).map(|m| m.fields.clone()) {
                if name == "_fields" {
                    let items: Vec<Value> =
                        fields.iter().map(|f| self.new_str(f.clone())).collect();
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
        // `sys.stdout` / `sys.stderr` resolve to the current redirect target when
        // one is active (`redirect_stdout`, or `sys.stdout = …`), so the attribute
        // reflects the live stream regardless of which `sys` instance is read
        // (`import` is not cached). `sys.__stdout__` keeps the native stream.
        if let Some(PyObj::Module { name: mname, .. }) = self.get(recv) {
            if mname == "sys" {
                if name == "stdout" {
                    if let Some(t) = self.stdout_target.clone() {
                        return Ok(t);
                    }
                } else if name == "stderr" {
                    if let Some(t) = self.stderr_target.clone() {
                        return Ok(t);
                    }
                }
            }
        }
        // A `_csv` reader is its own iterator.
        if matches!(name, "__iter__" | "__next__")
            && matches!(self.get(recv), Some(PyObj::CsvReader { .. }))
        {
            let b = self.alloc(PyObj::Builtin(name.to_string()));
            return Ok(self.alloc(PyObj::BoundMethod {
                recv: recv.clone(),
                func: b,
            }));
        }
        // Every lazy iterator answers the iteration protocol as BOUND METHODS,
        // not only through `next(it)`: `threading` takes
        // `itertools.count().__next__` as its thread-name counter.
        if matches!(name, "__next__" | "__iter__")
            && matches!(
                self.get(recv),
                Some(PyObj::ItertoolsIter { .. })
                    | Some(PyObj::Zip { .. })
                    | Some(PyObj::MapObj { .. })
                    | Some(PyObj::FilterObj { .. })
                    | Some(PyObj::EnumerateObj { .. })
                    | Some(PyObj::Iter(_))
                    | Some(PyObj::CallIter { .. })
            )
        {
            let b = self.alloc(PyObj::Builtin(name.to_string()));
            return Ok(self.alloc(PyObj::BoundMethod {
                recv: recv.clone(),
                func: b,
            }));
        }
        // A `_csv` writer's `.dialect`, or a dialect's parameters.
        if matches!(
            self.get(recv),
            Some(PyObj::CsvWriter { .. })
                | Some(PyObj::CsvDialect(_))
                | Some(PyObj::CsvReader { .. })
        ) {
            if let Some(r) = crate::stdlib::pycsv::attr(self, recv, name) {
                return r;
            }
        }
        // `h.name` / `.digest_size` / `.block_size` on a hash object.
        if matches!(self.get(recv), Some(PyObj::Hasher { .. })) {
            if let Some(r) = crate::stdlib::pyhash::attr(self, recv, name) {
                return r;
            }
        }
        // `f.closed` on an in-memory stream. Resolved before the borrowing match
        // below because the lookup needs `&mut self`.
        if matches!(
            self.get(recv),
            Some(PyObj::BytesIO { .. }) | Some(PyObj::StringIO { .. })
        ) {
            if let Some(r) = crate::stdlib::pyio::stream_attr(self, recv, name) {
                return r;
            }
        }
        // Native-shadowed module: fast-path the native namespace, else defer to
        // the real CPython module over the FFI bridge. Resolved before the
        // borrowing match below because the fallback needs `&mut self`.
        let module_lookup = match self.get(recv) {
            Some(PyObj::Module { slot, name: mname }) => Some((
                self.module_globals[*slot].get(name).cloned(),
                mname.clone(),
                *slot,
            )),
            _ => None,
        };
        if let Some((hit, _, slot)) = &module_lookup {
            // `mod.__dict__` is the live namespace, and it wins over any binding
            // the module happens to have made under that name.
            if name == "__dict__" {
                let slot = *slot;
                return Ok(self.module_dict(slot));
            }
            // Every module answers `__name__` even when its body never set one
            // (native modules do not run a body at all).
            if name == "__name__" && hit.is_none() {
                let n = module_lookup.as_ref().unwrap().1.clone();
                return Ok(self.new_str(n));
            }
        }
        let module_lookup = module_lookup.map(|(hit, mname, _)| (hit, mname));
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
                let inst_payload = inst.payload.clone();
                // Ahead of the MRO lookup on purpose: the implicit
                // `__hash__ = None` SHADOWS an inherited `__hash__`, so a
                // subclass that defines only `__eq__` must report None even
                // though its base defined a real one.
                if name == "__hash__" && self.implicit_hash_none(&class) {
                    return Ok(Value::Undef);
                }
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
                        // A native class method (a Builtin in the class ns, e.g.
                        // `_random.Random.random`) binds to the instance too, so a
                        // stored reference (`r = inst.random; r()`) still receives
                        // it. A TYPE object stored as a class attribute is not a
                        // method and must come back untouched:
                        // `TestCase.failureException = AssertionError` is exactly
                        // that, and binding it made `issubclass(exc_type,
                        // self.failureException)` compare against a bound method —
                        // so `unittest` filed every assertion failure as an ERROR.
                        Some(PyObj::Builtin(n)) if !crate::builtins::is_type_object_name(n) => {
                            return Ok(self.alloc(PyObj::BoundMethod {
                                recv: recv.clone(),
                                func: v,
                            }));
                        }
                        _ => return Ok(v),
                    }
                }
                // A named method inherited from the builtin base (`d.get`,
                // `stack.append`, `s.upper`) reached as an ATTRIBUTE (`g = d.get`)
                // binds to the instance; invoking it routes through the payload.
                // The method-CALL form `d.get(k)` is handled by `call_method`, but
                // binding it to a name (`_count_elements`'s `mapping.get`) lands here.
                if !matches!(inst_payload, Value::Undef) {
                    if let Some(base) = self.builtin_base_of(&class) {
                        if crate::builtins::type_has_method(base, name) {
                            let func =
                                self.alloc(PyObj::Builtin(format!("__base_method__.{name}")));
                            return Ok(self.alloc(PyObj::BoundMethod {
                                recv: recv.clone(),
                                func,
                            }));
                        }
                    }
                }
                // An inherited `object` slot reached on an instance (`obj.__str__`)
                // with no user override is a bound method-wrapper, as in CPython.
                if OBJECT_SLOT_WRAPPERS.contains(&name) {
                    return Ok(self.alloc(PyObj::Descriptor {
                        kind: DescKind::MethodWrapper,
                        qual: format!("{class}.{name}"),
                    }));
                }
                Err(format!(
                    "AttributeError: '{}' object has no attribute '{}'",
                    class, name
                ))
            }
            Some(PyObj::Class(cname)) => {
                let cname = cname.clone();
                // See the instance arm: the implicit `__hash__ = None` shadows
                // an inherited `__hash__`, so this precedes the MRO lookup.
                if name == "__hash__" && self.implicit_hash_none(&cname) {
                    return Ok(Value::Undef);
                }
                if name == "__name__" {
                    // The registry KEY may be disambiguated (`Codec#1`) when a
                    // class shadows one of its bases; the display name is the
                    // one the class was written with.
                    let disp = self
                        .classes
                        .get(&cname)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| cname.clone());
                    return Ok(self.new_str(disp));
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
                // Every class has `__doc__`, `None` when undocumented. A body
                // run by `run_class_body` gets one from its docstring; a class
                // registered natively (the `_io` bases) has no body to read.
                if name == "__doc__" && !self.class_has(&cname, "__doc__") {
                    return Ok(Value::Undef);
                }
                if name == "__module__" {
                    let m = self
                        .classes
                        .get(&cname)
                        .map(|c| c.module.clone())
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| "__main__".to_string());
                    return Ok(self.new_str(m));
                }
                // A class with no annotations still has `__annotations__ == {}`
                // (not an AttributeError), as in CPython — else typing's
                // `_get_protocol_attrs` falls back to `annotationlib` (t-strings).
                if name == "__annotations__" && self.class_lookup(&cname, name).is_none() {
                    return Ok(self.new_dict(IndexMap::new()));
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
                        // A user-defined class stays a Class; a builtin ancestor
                        // (`int`, `Exception`, …) must be its Builtin type object
                        // so `.__dict__`/identity match the real builtin type.
                        .map(|c| self.class_or_builtin_type(c))
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
                            .map(|b| self.class_or_builtin_type(b))
                            .collect()
                    };
                    return Ok(self.new_tuple(vals));
                }
                // `cls.__subclasses__()` — bound zero-arg method (computed on call).
                if name == "__subclasses__" {
                    let cls = self.alloc(PyObj::Class(cname.clone()));
                    let func = self.alloc(PyObj::Builtin("__subclasses__".into()));
                    return Ok(self.alloc(PyObj::BoundMethod { recv: cls, func }));
                }
                // `cls.__new__` — the class's own __new__ if defined, else the
                // inherited `object.__new__` (a callable that builds a bare
                // instance). An implicit staticmethod, so it is returned unbound.
                if name == "__new__" {
                    if let Some(f) = self.class_lookup(&cname, "__new__") {
                        return Ok(f);
                    }
                    return Ok(self.alloc(PyObj::Builtin("object.__new__".into())));
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
                // The `type` surface a class inherits from its metaclass —
                // `A.mro`, `A.__instancecheck__`, `A.__base__`. AFTER both the
                // class namespace and the metaclass lookup above: a user
                // metaclass defining `__instancecheck__` must win, or the
                // default implementation calls `isinstance`, which calls the
                // hook, which lands back here.
                if crate::builtins::TYPE_OBJECT_METHODS.contains(&name) {
                    let cls = self.alloc(PyObj::Class(cname.clone()));
                    let func = self.alloc(PyObj::Builtin(name.to_string()));
                    return Ok(self.alloc(PyObj::BoundMethod { recv: cls, func }));
                }
                if matches!(name, "__base__" | "__type_params__" | "__text_signature__") {
                    let mro = self.mro_of(&cname);
                    return self.type_object_data_attr(&mro, name);
                }
                // An inherited object-slot dunder reached on the class object with
                // no override (`C.__ne__`, `C.__repr__`) is the unbound slot
                // wrapper from `object`, as in CPython. (collections' OrderedDict
                // does `__ne__ = _collections_abc.MutableMapping.__ne__`.)
                if OBJECT_SLOT_WRAPPERS.contains(&name) {
                    return Ok(self.alloc(PyObj::Descriptor {
                        kind: DescKind::WrapperDescriptor,
                        qual: format!("object.{name}"),
                    }));
                }
                Err(format!(
                    "AttributeError: type object '{cname}' has no attribute '{name}'"
                ))
            }
            // Modules are fully resolved up-front (see the block before this
            // match) so the FFI fallback can take `&mut self`; unreachable here.
            Some(PyObj::Module { slot, name: mname }) => {
                let (mname, slot) = (mname.clone(), *slot);
                match self.module_globals[slot].get(name) {
                    Some(v) => Ok(v.clone()),
                    None => Err(format!(
                        "AttributeError: module '{mname}' has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Exception { class, args }) => {
                // An attribute assigned onto this instance wins: exceptions carry
                // arbitrary attributes in CPython, and `unittest` stamps its own
                // bookkeeping onto the exceptions it catches.
                if let Value::Obj(id) = recv {
                    if let Some(v) = self.func_attrs.get(id).and_then(|m| m.get(name)) {
                        return Ok(v.clone());
                    }
                }
                if name == "__class__" {
                    // The exception's type object (`e.__class__ is ValueError`,
                    // `e.__class__.__name__`).
                    let c = class.clone();
                    return Ok(self.alloc(PyObj::Builtin(c)));
                }
                if name == "args" {
                    let a = args.clone();
                    return Ok(self.new_tuple(a));
                }
                // `BaseExceptionGroup.message` / `.exceptions` — the two
                // constructor arguments, the second always exposed as a tuple.
                if matches!(name, "message" | "exceptions") {
                    if let Some((msg, excs)) = crate::excgroup::group_parts(self, recv) {
                        return Ok(if name == "message" {
                            msg
                        } else {
                            self.new_tuple(excs)
                        });
                    }
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
                if name == "__traceback__" {
                    // The captured `(scope, line)` frames as a traceback chain, or
                    // None if the exception was never propagated through a frame.
                    let frames = match recv {
                        Value::Obj(id) => self
                            .exc_tb
                            .get(id)
                            .map(|fs| {
                                fs.iter()
                                    .map(|(s, l, _)| (s.clone(), *l))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    if frames.is_empty() {
                        return Ok(Value::Undef);
                    }
                    return Ok(self.alloc(PyObj::Traceback { frames, idx: 0 }));
                }
                if name == "__suppress_context__" {
                    // True iff raised with any explicit `from` clause (`raise X
                    // from Y` or `raise X from None`).
                    let suppressed =
                        matches!(recv, Value::Obj(id) if self.suppress_context.contains(id));
                    return Ok(Value::Bool(suppressed));
                }
                let class = class.clone();
                // The exception's own methods (`add_note`, `with_traceback`, and
                // PEP 654's `split`/`subgroup`/`derive`) as bound methods, which
                // `dir()` lists and `call_type_method` dispatches. Reaching them
                // only through a call expression made `e.add_note` — the form
                // `traceback` and `unittest` pass around — an AttributeError.
                if crate::builtins::type_has_method(&class, name) {
                    let b = self.alloc(PyObj::Builtin(name.to_string()));
                    return Ok(self.alloc(PyObj::BoundMethod {
                        recv: recv.clone(),
                        func: b,
                    }));
                }
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
            // `func.__closure__` — a tuple of cells over the function's free
            // variables (their current values from the captured env), or None.
            // A function is a (non-data) descriptor: `f.__get__(obj, cls)` binds
            // it as a method. Only `__get__` exists — no `__set__`/`__delete__` —
            // so `_is_descriptor(func)` in enum's `_EnumDict` is True (methods are
            // not recorded as members) while functions stay non-data descriptors.
            Some(PyObj::Func(_)) if name == "__get__" => {
                let func = self.alloc(PyObj::Builtin("function.__get__".into()));
                Ok(self.alloc(PyObj::BoundMethod {
                    recv: recv.clone(),
                    func,
                }))
            }
            Some(PyObj::Func(fv)) if name == "__closure__" => {
                let fv = fv.clone();
                let freevars = self.funcs[fv.def_id].freevars.clone();
                if freevars.is_empty() {
                    return Ok(Value::Undef);
                }
                let mut cells = Vec::with_capacity(freevars.len());
                for fname in &freevars {
                    let val = fv
                        .env
                        .as_ref()
                        .and_then(|e| self.env_lookup(e, fname))
                        .unwrap_or(Value::Undef);
                    cells.push(self.alloc(PyObj::Cell { value: val }));
                }
                Ok(self.new_tuple(cells))
            }
            Some(PyObj::Cell { value }) if name == "cell_contents" => Ok(value.clone()),
            // `staticmethod.__func__` / `classmethod.__func__` — the wrapped
            // function (enum's _EnumDict inspects these).
            Some(PyObj::StaticMethod(inner)) | Some(PyObj::ClassMethod(inner))
                if name == "__func__" || name == "__wrapped__" =>
            {
                Ok(inner.clone())
            }
            // `staticmethod`/`classmethod`/`property` each carry a real
            // `__isabstractmethod__` getset defaulting to False — `abc` reads it
            // through the decorator to decide whether the wrapped function is
            // abstract. A plain function does NOT (see below).
            Some(PyObj::StaticMethod(_))
            | Some(PyObj::ClassMethod(_))
            | Some(PyObj::Property { .. })
                if name == "__isabstractmethod__" =>
            {
                let id = if let Value::Obj(i) = recv { *i } else { 0 };
                Ok(self
                    .func_attrs
                    .get(&id)
                    .and_then(|m| m.get(name))
                    .cloned()
                    .unwrap_or(Value::Bool(false)))
            }
            // A function's writable attribute dict.
            Some(PyObj::Func(_)) if name == "__dict__" => {
                let id = if let Value::Obj(i) = recv { *i } else { 0 };
                let attrs = self.func_attrs.get(&id).cloned().unwrap_or_default();
                let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                for (k, v) in attrs {
                    let kv = self.new_str(k.clone());
                    d.insert(PKey::Str(k), (kv, v));
                }
                Ok(self.new_dict(d))
            }
            // A generator's introspection attributes. `gi_code`, `gi_frame`
            // and `gi_yieldfrom` are NOT here: the first two need a code and
            // frame object pythonrs does not build for a generator body, and
            // the third needs the delegated iterator tracked across
            // `yield from` — reporting `None` for any of them would be a
            // wrong answer rather than a missing one.
            Some(PyObj::Generator { .. })
                if matches!(
                    name,
                    "__name__" | "__qualname__" | "gi_running" | "gi_suspended"
                ) =>
            {
                let Some((fname, running, suspended)) = self.gen_state(recv) else {
                    return Err(format!(
                        "AttributeError: 'generator' object has no attribute '{name}'"
                    ));
                };
                Ok(match name {
                    "gi_running" => Value::Bool(running),
                    "gi_suspended" => Value::Bool(suspended),
                    _ => self.new_str(fname),
                })
            }
            // A property's three accessors and its name. `abc` reads `fget`
            // to wrap an abstract property, and `inspect`/`dataclasses` read
            // all three to tell a data descriptor from a plain one — every one
            // of them was an AttributeError.
            Some(PyObj::Property {
                fget,
                fset,
                fdel,
                name: pname,
            }) if matches!(name, "fget" | "fset" | "fdel" | "__name__") => {
                Ok(match name {
                    "fget" => fget.clone(),
                    "fset" => fset.clone(),
                    "fdel" => fdel.clone(),
                    // The class-namespace key `__set_name__` recorded, falling
                    // back to the getter's own name for a property built
                    // outside any class body.
                    _ => {
                        if pname.is_empty() {
                            let g = fget.clone();
                            return self.get_attr(&g, "__name__");
                        }
                        let n = pname.clone();
                        self.new_str(n)
                    }
                })
            }
            // An attribute previously assigned onto a `property` or descriptor
            // (`_Instruction.opname.__doc__ = "…"`) reads back from the same side
            // table; `__doc__` defaults to None when never set.
            Some(PyObj::Property { .. }) | Some(PyObj::Descriptor { .. })
                if name == "__doc__"
                    || matches!(recv, Value::Obj(id)
                        if self.func_attrs.get(id).is_some_and(|m| m.contains_key(name))) =>
            {
                let id = if let Value::Obj(i) = recv { *i } else { 0 };
                Ok(self
                    .func_attrs
                    .get(&id)
                    .and_then(|m| m.get(name))
                    .cloned()
                    .unwrap_or(Value::Undef))
            }
            // A user-assigned function attribute reads from the side dict. Note
            // that `__isabstractmethod__` gets NO default here: unlike the
            // decorators above, a plain function has no such slot in CPython, so
            // reading one that `abc.abstractmethod` never set is an
            // `AttributeError` — which is exactly what `getattr(f, …, False)`
            // relies on.
            Some(PyObj::Func(_))
                if matches!(recv, Value::Obj(id)
                        if self.func_attrs.get(id).is_some_and(|m| m.contains_key(name))) =>
            {
                let id = if let Value::Obj(i) = recv { *i } else { 0 };
                let v = self.func_attrs.get(&id).and_then(|m| m.get(name)).cloned();
                Ok(v.unwrap_or(Value::Bool(false)))
            }
            // Code-object introspection (`func.__code__.co_*`), derived from the
            // backing `FuncDef`. Native VM object, not a Python reimplementation.
            // A frame's code object: only the identifying fields, which is all
            // any caller reads off one.
            Some(PyObj::FrameCode {
                name: fname,
                lineno,
            }) => {
                let (fname, lineno) = (fname.clone(), *lineno);
                match name {
                    "co_name" | "co_qualname" => Ok(self.new_str(fname)),
                    "co_filename" => {
                        let f = self.tb_filename.clone();
                        Ok(self.new_str(f))
                    }
                    "co_firstlineno" => Ok(Value::Int(lineno as i64)),
                    "co_flags" => Ok(Value::Int(0)),
                    "co_argcount" | "co_kwonlyargcount" | "co_posonlyargcount" | "co_nlocals"
                    | "co_stacksize" => Ok(Value::Int(0)),
                    "co_varnames" | "co_names" | "co_freevars" | "co_cellvars" | "co_consts" => {
                        Ok(self.new_tuple(vec![]))
                    }
                    _ => Err(format!(
                        "AttributeError: 'code' object has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Code { def_id }) => {
                let def_id = *def_id;
                self.code_attr(def_id, name)
            }
            // `(int | str).__args__` -> the member tuple; `__parameters__` is empty
            // (no typevars in a plain union).
            // PEP 750 `Template` / `Interpolation` attributes.
            Some(PyObj::Template {
                strings,
                interpolations,
            }) => {
                let (strings, interps) = (strings.clone(), interpolations.clone());
                match name {
                    "strings" => {
                        let vals: Vec<Value> =
                            strings.into_iter().map(|s| self.new_str(s)).collect();
                        Ok(self.new_tuple(vals))
                    }
                    "interpolations" => Ok(self.new_tuple(interps)),
                    // `values` is the interpolations' evaluated values, in order.
                    "values" => {
                        let vals: Vec<Value> = interps
                            .iter()
                            .map(|i| match self.get(i) {
                                Some(PyObj::Interpolation { value, .. }) => value.clone(),
                                _ => Value::Undef,
                            })
                            .collect();
                        Ok(self.new_tuple(vals))
                    }
                    _ => Err(format!(
                        "AttributeError: 'Template' object has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Interpolation {
                value,
                expression,
                conversion,
                format_spec,
            }) => {
                let (value, expression) = (value.clone(), expression.clone());
                let (conversion, format_spec) = (*conversion, format_spec.clone());
                match name {
                    "value" => Ok(value),
                    "expression" => Ok(self.new_str(expression)),
                    "conversion" => Ok(match conversion {
                        Some(c) => self.new_str(c.to_string()),
                        None => Value::Undef,
                    }),
                    "format_spec" => Ok(self.new_str(format_spec)),
                    _ => Err(format!(
                        "AttributeError: 'Interpolation' object has no attribute '{name}'"
                    )),
                }
            }
            Some(PyObj::Union { args }) if name == "__args__" => {
                let args = args.clone();
                Ok(self.new_tuple(args))
            }
            Some(PyObj::Union { .. }) if name == "__parameters__" => Ok(self.new_tuple(vec![])),
            // A TypeVar/ParamSpec/TypeVarTuple exposes its dunder attributes from
            // the backing dict; `has_default()` reflects whether `__default__` was
            // set (not the `NoDefault` sentinel).
            Some(PyObj::TypeVarLike { attrs, .. }) => {
                let attrs = attrs.clone();
                if let Some(PyObj::Dict(d)) = self.get(&attrs) {
                    if let Some((_, v)) = d.get(&PKey::Str(name.to_string())) {
                        return Ok(v.clone());
                    }
                }
                Err(format!(
                    "AttributeError: '{}' object has no attribute '{name}'",
                    self.type_name(recv)
                ))
            }
            // `f.name` / `f.mode` / `f.closed` / `f.encoding` — the data
            // attributes of a file object (its methods keep going through the
            // builtin method table below).
            Some(PyObj::File { id }) => {
                let id = *id;
                let cls = self.file_class_name(id);
                match name {
                    "name" => {
                        let n = self.io_name(id);
                        return Ok(self.new_str(n));
                    }
                    "mode" => {
                        let m = self.io_mode(id);
                        return Ok(self.new_str(m));
                    }
                    "closed" => return Ok(Value::Bool(self.io_closed(id))),
                    // Only a text stream carries a codec, as in CPython.
                    "encoding" if cls == "TextIOWrapper" => {
                        return Ok(self.new_str("UTF-8".to_string()))
                    }
                    "errors" if cls == "TextIOWrapper" => {
                        return Ok(self.new_str("strict".to_string()))
                    }
                    "newlines" if cls == "TextIOWrapper" => return Ok(Value::Undef),
                    _ => {}
                }
                if crate::builtins::type_has_method("TextIOWrapper", name)
                    || crate::builtins::is_object_dunder_method(cls, name)
                {
                    let b = self.alloc(PyObj::Builtin(name.to_string()));
                    let recv = recv.clone();
                    return Ok(self.alloc(PyObj::BoundMethod { recv, func: b }));
                }
                Err(format!(
                    "AttributeError: '_io.{cls}' object has no attribute '{name}'"
                ))
            }
            // `struct_time.tm_year` etc. — read the named field (including the
            // attribute-only `tm_gmtoff`/`tm_zone`).
            Some(PyObj::StructTime { fields }) => {
                if let Some(i) = STRUCT_TIME_FIELDS.iter().position(|f| *f == name) {
                    if let Some(v) = fields.get(i) {
                        return Ok(v.clone());
                    }
                }
                Err(format!(
                    "AttributeError: 'time.struct_time' object has no attribute '{name}'"
                ))
            }
            // `re.Pattern` attributes; method names bind as callable methods.
            Some(PyObj::Pattern {
                pattern,
                flags,
                groups,
                ..
            }) => {
                let (pattern, flags, groups) = (pattern.clone(), *flags, *groups);
                match name {
                    "pattern" => Ok(self.new_str(pattern)),
                    "flags" => Ok(Value::Int(flags)),
                    "groups" => Ok(Value::Int(groups as i64)),
                    "match" | "search" | "fullmatch" | "findall" | "finditer" | "sub" | "subn"
                    | "split" => {
                        let func = self.alloc(PyObj::Builtin(format!("__base_method__.{name}")));
                        Ok(self.alloc(PyObj::BoundMethod {
                            recv: recv.clone(),
                            func,
                        }))
                    }
                    _ => Err(format!(
                        "AttributeError: 're.Pattern' object has no attribute '{name}'"
                    )),
                }
            }
            // `re.Match` attributes; method names bind as callable methods.
            Some(PyObj::Match {
                text,
                spans,
                pos,
                endpos,
                ..
            }) => match name {
                "string" => Ok(self.new_str(text.clone())),
                "lastindex" => Ok(spans
                    .iter()
                    .rposition(|s| s.is_some())
                    .filter(|&i| i > 0)
                    .map(|i| Value::Int(i as i64))
                    .unwrap_or(Value::Undef)),
                // The search window, in codepoints. `pos` was hard-coded to 0 and
                // `endpos` to the BYTE length, so `p.search(s, 3).pos` reported 0
                // and `m.endpos` counted 'é' twice.
                "pos" => Ok(Value::Int(crate::regexpr::char_index_of(text, *pos) as i64)),
                "endpos" => Ok(Value::Int(
                    crate::regexpr::char_index_of(text, *endpos) as i64
                )),
                "group" | "groups" | "groupdict" | "start" | "end" | "span" => {
                    let func = self.alloc(PyObj::Builtin(format!("__base_method__.{name}")));
                    Ok(self.alloc(PyObj::BoundMethod {
                        recv: recv.clone(),
                        func,
                    }))
                }
                _ => Err(format!(
                    "AttributeError: 're.Match' object has no attribute '{name}'"
                )),
            },
            // Generic alias: expose origin/args; forward anything else to origin.
            Some(PyObj::GenericAlias { origin, args }) => {
                let (origin, args) = (origin.clone(), args.clone());
                match name {
                    "__origin__" => Ok(origin),
                    "__args__" => Ok(self.new_tuple(args)),
                    "__parameters__" => Ok(self.new_tuple(vec![])),
                    _ => self.get_attr(&origin, name),
                }
            }
            // SimpleNamespace attribute reads resolve from its bag.
            // `Struct.size` / `.format` — attributes on a compiled struct.
            Some(PyObj::StructFmt(_)) => {
                match crate::stdlib::pystruct::struct_attr_of(self, recv, name) {
                    Some(r) => r,
                    None => Err(type_error(&format!(
                        "'Struct' object has no attribute '{name}'"
                    ))),
                }
            }
            Some(PyObj::Namespace { attrs }) => match attrs.get(name) {
                Some(v) => Ok(v.clone()),
                None => Err(format!(
                    "AttributeError: 'types.SimpleNamespace' object has no attribute '{name}'"
                )),
            },
            // Traceback chain navigation.
            Some(PyObj::Traceback { frames, idx }) => {
                let (frames, idx) = (frames.clone(), *idx);
                match name {
                    "tb_frame" => {
                        let (n, l) = frames[idx].clone();
                        Ok(self.alloc(PyObj::PyFrame { name: n, lineno: l }))
                    }
                    "tb_lineno" => Ok(Value::Int(frames[idx].1 as i64)),
                    "tb_lasti" => Ok(Value::Int(-1)),
                    "tb_next" => {
                        if idx + 1 < frames.len() {
                            Ok(self.alloc(PyObj::Traceback {
                                frames,
                                idx: idx + 1,
                            }))
                        } else {
                            Ok(Value::Undef)
                        }
                    }
                    _ => Err(format!(
                        "AttributeError: 'traceback' object has no attribute '{name}'"
                    )),
                }
            }
            // Frame object: the module globals are live; locals are not captured.
            Some(PyObj::PyFrame {
                lineno,
                name: fname,
            }) => {
                let lineno = *lineno;
                let fname = fname.clone();
                match name {
                    "f_lineno" => Ok(Value::Int(lineno as i64)),
                    // `f_code` — the frame's code object. `logging.findCaller`
                    // walks frames reading `f_code.co_filename` to locate the
                    // caller, and `warnings` does the same to decide whether a
                    // frame is stdlib-internal; both are unreachable without it.
                    "f_code" => Ok(self.alloc(PyObj::FrameCode {
                        name: fname,
                        lineno,
                    })),
                    "f_trace" | "f_trace_lines" | "f_trace_opcodes" => Ok(Value::Undef),
                    "f_lasti" => Ok(Value::Int(-1)),
                    "f_locals" => Ok(self.new_dict(IndexMap::new())),
                    "f_globals" | "f_builtins" => {
                        let pairs = self.globals_pairs();
                        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                        for (k, v) in pairs {
                            let kv = self.new_str(k.clone());
                            d.insert(PKey::Str(k), (kv, v));
                        }
                        Ok(self.new_dict(d))
                    }
                    "f_back" => Ok(Value::Undef),
                    _ => Err(format!(
                        "AttributeError: 'frame' object has no attribute '{name}'"
                    )),
                }
            }
            // Function introspection dunders. `C.m` yields the raw `Func`; a
            // bound `inst.m` delegates to the same underlying function.
            // `f.__annotations__` — the def-time dict `{param|"return": type}`.
            Some(PyObj::Func(fv)) if name == "__annotations__" => {
                let ann = fv.annotations.clone();
                if matches!(ann, Value::Undef) {
                    Ok(self.new_dict(IndexMap::new()))
                } else {
                    Ok(ann)
                }
            }
            // `f.__annotate__` (PEP 649): the callable that produces the
            // annotations for a requested format, or `None` on an unannotated
            // function. CPython 3.14's `functools.singledispatch.register`
            // gates on its presence before reading the first parameter's type.
            Some(PyObj::Func(fv)) if name == "__annotate__" => {
                let ann = fv.annotations.clone();
                let empty = match self.get(&ann) {
                    Some(PyObj::Dict(d)) => d.is_empty(),
                    _ => true,
                };
                if empty {
                    return Ok(Value::Undef);
                }
                let f = self.alloc(PyObj::Builtin("function.__annotate__".into()));
                Ok(self.alloc(PyObj::Partial {
                    func: f,
                    args: vec![ann],
                    kwargs: vec![],
                }))
            }
            Some(PyObj::BoundMethod { func, .. }) if name == "__annotations__" => {
                let func = func.clone();
                match self.get(&func) {
                    Some(PyObj::Func(fv)) if !matches!(fv.annotations, Value::Undef) => {
                        Ok(fv.annotations.clone())
                    }
                    _ => Ok(self.new_dict(IndexMap::new())),
                }
            }
            Some(PyObj::Func(fv))
                if matches!(
                    name,
                    "__name__"
                        | "__qualname__"
                        | "__module__"
                        | "__defaults__"
                        | "__kwdefaults__"
                        | "__doc__"
                        | "__code__"
                ) =>
            {
                let (def_id, defaults) = (fv.def_id, fv.defaults.clone());
                let kwd = fv.kwonly_defaults.clone();
                self.func_dunder(name, def_id, &defaults, &kwd)
            }
            Some(PyObj::BoundMethod { func, recv })
                if matches!(
                    name,
                    "__name__"
                        | "__qualname__"
                        | "__module__"
                        | "__defaults__"
                        | "__kwdefaults__"
                        | "__doc__"
                        | "__code__"
                ) =>
            {
                let func = func.clone();
                let recv = recv.clone();
                match self.get(&func) {
                    Some(PyObj::Func(fv)) => {
                        let (def_id, defaults) = (fv.def_id, fv.defaults.clone());
                        let kwd = fv.kwonly_defaults.clone();
                        self.func_dunder(name, def_id, &defaults, &kwd)
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
            Some(PyObj::Builtin(n)) if name == "__name__" || name == "__qualname__" => {
                // `type(x).__name__` / `.__qualname__` — the builtin type's BARE
                // name. A module-qualified type object (`re.Pattern`,
                // `string.templatelib.Template`) reports just the trailing
                // component, as CPython does; the module lives in `__module__`.
                let n = n.clone();
                let bare = match type_object_class_name(&n) {
                    Some(_) => n.rsplit('.').next().unwrap_or(&n).to_string(),
                    None => n,
                };
                Ok(self.new_str(bare))
            }
            // A builtin type object / function reports `builtins` as its module
            // (typing's deprecated-alias machinery reads `origin.__module__`).
            // Every builtin function has a `__doc__`; CPython's carry the C
            // docstring, and `None` is what one without a docstring reports.
            // `signal.py` copies it off the accelerator onto each wrapper.
            Some(PyObj::Builtin(_)) if name == "__doc__" => Ok(Value::Undef),
            Some(PyObj::Builtin(n)) if name == "__module__" => {
                // A module-qualified type object reports its own module.
                let n = n.clone();
                let m = match type_object_class_name(&n) {
                    Some(q) => match q.rsplit_once('.') {
                        Some((module, _)) => module.to_string(),
                        None => "builtins".to_string(),
                    },
                    None => "builtins".to_string(),
                };
                Ok(self.new_str(m))
            }
            // The `type` surface on a builtin type object — `int.mro()`,
            // `int.__instancecheck__(5)`, `int | str` as a method. `dir(int)`
            // does NOT list these (CPython lists a type's INSTANCE attributes,
            // not its metaclass's), but `getattr` produces every one.
            Some(PyObj::Builtin(n))
                if crate::builtins::TYPE_OBJECT_METHODS.contains(&name)
                    && crate::builtins::is_type_object_name(n)
                    // A type that defines the name ITSELF shadows the metaclass:
                    // `int.__or__` is the integer bitwise operator reached
                    // unbound, not `type.__or__`, which is why CPython answers
                    // `int.__or__(str)` with a descriptor TypeError while
                    // `int | str` is a union.
                    && !crate::builtins::type_has_method(n, name) =>
            {
                let recv = recv.clone();
                let func = self.alloc(PyObj::Builtin(name.to_string()));
                Ok(self.alloc(PyObj::BoundMethod { recv, func }))
            }
            Some(PyObj::Builtin(n))
                if matches!(name, "__base__" | "__type_params__" | "__text_signature__")
                    && crate::builtins::is_type_object_name(n) =>
            {
                self.type_object_data_attr(&crate::builtins::builtin_mro(n), name)
            }
            // `<type>.__mro__` / `__bases__` on a type object.
            Some(PyObj::Builtin(n))
                if (name == "__mro__" || name == "__bases__")
                    && (crate::builtins::is_type_object_name(n)) =>
            {
                let mut mro = crate::builtins::builtin_mro(n);
                if name == "__bases__" {
                    // Immediate bases: everything after self (usually just object).
                    mro.remove(0);
                }
                let vals: Vec<Value> = mro
                    .into_iter()
                    .map(|c| self.alloc(PyObj::Builtin(c)))
                    .collect();
                Ok(self.new_tuple(vals))
            }
            // `<type>.__dict__` — a mappingproxy over the type's namespace. Sparse
            // for builtin types: only the classmethod descriptors the stdlib
            // reaches via `<type>.__dict__[name]` are populated (pythonrs has no
            // full C method table to enumerate).
            Some(PyObj::Builtin(n))
                if name == "__dict__" && (crate::builtins::is_type_object_name(n)) =>
            {
                let n = n.clone();
                let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                // Every builtin type carries `__new__` in its own `__dict__` in
                // CPython; enum's `_find_data_type_` keys off this membership to
                // recognise a mixin data type (`'__new__' in base.__dict__`).
                {
                    let key = self.new_str("__new__".to_string());
                    let ctor = self.alloc(PyObj::Builtin("object.__new__".into()));
                    d.insert(PKey::Str("__new__".to_string()), (key, ctor));
                }
                // The type-level getset descriptors. Code binds these directly to
                // read an attribute off a class WITHOUT going through
                // `__getattr__` or a metaclass: `annotationlib` does
                // `type.__dict__['__annotations__'].__get__`, and `inspect`'s
                // static introspection is built on `type.__dict__['__mro__']` and
                // `type.__dict__['__dict__']`. Everything downstream of those two
                // modules — `dataclasses`, `traceback`, `logging`, `unittest`,
                // `hashlib` — reaches the interpreter through this table.
                if n == "type" {
                    for attr in [
                        "__annotations__",
                        "__mro__",
                        "__dict__",
                        "__bases__",
                        "__name__",
                        "__qualname__",
                        "__module__",
                        "__doc__",
                    ] {
                        let key = self.new_str(attr.to_string());
                        let desc = self.alloc(PyObj::Descriptor {
                            kind: DescKind::GetSetDescriptor,
                            qual: format!("type.{attr}"),
                        });
                        d.insert(PKey::Str(attr.to_string()), (key, desc));
                    }
                }
                for cm in crate::builtins::type_classmethods(&n) {
                    let key = self.new_str((*cm).to_string());
                    let desc = self.alloc(PyObj::Descriptor {
                        kind: DescKind::ClassMethodDescriptor,
                        qual: format!("{n}.{cm}"),
                    });
                    d.insert(PKey::Str((*cm).to_string()), (key, desc));
                }
                let dict = self.alloc(PyObj::Dict(d));
                Ok(self.alloc(PyObj::MappingProxy { dict }))
            }
            // `<type>.__new__` on a builtin type object — a callable constructor.
            // A data type builds a payload-carrying instance when its `__new__`
            // is invoked on a subclass (enum's `_new_member_ = str.__new__`,
            // `int.__new__`, …); other builtin types fall back to the generic
            // bare-instance `object.__new__`.
            Some(PyObj::Builtin(n))
                if name == "__new__" && crate::builtins::is_type_object_name(n) =>
            {
                let ctor = match n.as_str() {
                    "int" | "str" | "float" | "tuple" | "frozenset" | "list" | "dict" | "set" => {
                        format!("{n}.__new__")
                    }
                    _ => "object.__new__".to_string(),
                };
                Ok(self.alloc(PyObj::Builtin(ctor)))
            }
            // `FunctionType.__code__` / `.__globals__` reached on the TYPE object
            // are the get/set and member descriptors CPython's `types` derives.
            Some(PyObj::Builtin(n)) if n == "function" && name == "__code__" => {
                Ok(self.alloc(PyObj::Descriptor {
                    kind: DescKind::GetSetDescriptor,
                    qual: "function.__code__".into(),
                }))
            }
            Some(PyObj::Builtin(n)) if n == "function" && name == "__globals__" => {
                Ok(self.alloc(PyObj::Descriptor {
                    kind: DescKind::MemberDescriptor,
                    qual: "function.__globals__".into(),
                }))
            }
            // A dunder slot reached on a builtin type object (`object.__init__`)
            // is an unbound slot wrapper (`wrapper_descriptor`).
            Some(PyObj::Builtin(n))
                if crate::builtins::is_type_like_builtin(n)
                    && name.starts_with("__")
                    && name.ends_with("__")
                    && OBJECT_SLOT_WRAPPERS.contains(&name) =>
            {
                Ok(self.alloc(PyObj::Descriptor {
                    kind: DescKind::WrapperDescriptor,
                    qual: format!("{n}.{name}"),
                }))
            }
            // `itertools.chain.from_iterable` — the alternate constructor, read
            // off the `chain` callable itself. `call_itertools` already builds it
            // from the dotted name; only this lookup was missing.
            Some(PyObj::Builtin(n)) if n == "itertools.chain" && name == "from_iterable" => {
                Ok(self.alloc(PyObj::Builtin("itertools.chain.from_iterable".into())))
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
            // `range` read-only attributes. Unlike `slice`, these are always
            // integers — `range` normalizes its omitted bounds at construction,
            // so `range(3).start` is `0` and `range(3).step` is `1`.
            Some(PyObj::Range { start, stop, step })
                if matches!(name, "start" | "stop" | "step") =>
            {
                Ok(Value::Int(match name {
                    "start" => *start,
                    "stop" => *stop,
                    _ => *step,
                }))
            }
            Some(PyObj::BigRange { start, stop, step })
                if matches!(name, "start" | "stop" | "step") =>
            {
                let b = match name {
                    "start" => start.clone(),
                    "stop" => stop.clone(),
                    _ => step.clone(),
                };
                Ok(self.norm_big(b))
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
                // `.numerator`/`.denominator` — an integer is its own numerator
                // over a denominator of 1 (bool normalizes to `int`).
                if name == "numerator" || name == "denominator" {
                    let is_int = matches!(recv, Value::Int(_) | Value::Bool(_))
                        || matches!(self.get(recv), Some(PyObj::BigInt(_)));
                    if is_int {
                        return Ok(if name == "denominator" {
                            Value::Int(1)
                        } else if let Value::Bool(b) = recv {
                            Value::Int(*b as i64)
                        } else {
                            recv.clone()
                        });
                    }
                }
                // `x.__class__` on a builtin-type value is its type object (same
                // as `type(x)`).
                let tn = self.type_name(recv);
                // An unhashable builtin's `__hash__` slot is set to `None`, not
                // removed: `[].__hash__` READS as None (so `hasattr` is True)
                // and only calling it fails. Handing back a bound method here
                // made `if obj.__hash__ is None` — how the stdlib tests
                // hashability — take the wrong branch on every list and dict.
                if name == "__hash__" && crate::builtins::UNHASHABLE_TYPES.contains(&tn.as_str()) {
                    return Ok(Value::Undef);
                }
                if name == "__class__" {
                    return Ok(if self.classes.contains_key(&tn) {
                        self.alloc(PyObj::Class(tn))
                    } else {
                        self.alloc(PyObj::Builtin(tn))
                    });
                }
                // `defaultdict.default_factory` — the zero-arg callable `__missing__`
                // calls, or `None`. It is a writable attribute in CPython (see the
                // setter beside `maxlen`'s in `set_attr`), and code that inspects a
                // defaultdict before extending it reads it constantly.
                if name == "default_factory" {
                    if let Value::Obj(i) = recv {
                        if let Some(m) = self.dict_meta.get(i) {
                            if m.kind == DictKind::DefaultDict {
                                return Ok(m.factory.clone().unwrap_or(Value::Undef));
                            }
                        }
                    }
                }
                // `deque.maxlen` — the read-only length bound (an int) or `None`.
                if name == "maxlen" {
                    if let Some(PyObj::Deque { maxlen, .. }) = self.get(recv) {
                        return Ok(match maxlen {
                            Some(m) => Value::Int(*m as i64),
                            None => Value::Undef,
                        });
                    }
                }
                // Builtin type method, or a universal object dunder (`d.__len__`,
                // `d.__getitem__`, `x.__eq__`) — hand back a bound builtin method
                // that `call_type_method` dispatches to the native operation.
                if crate::builtins::type_has_method(&tn, name)
                    || crate::builtins::is_object_dunder_method(&tn, name)
                {
                    let b = self.alloc(PyObj::Builtin(name.to_string()));
                    return Ok(self.alloc(PyObj::BoundMethod {
                        recv: recv.clone(),
                        func: b,
                    }));
                }
                // The type-level dunders every value inherits (`[].__init__`,
                // `(5).__sizeof__`, `"x".__doc__`). CPython resolves these
                // through the type, so resolve them through the type object here
                // rather than reporting a name `dir()` lists as absent. The
                // instance form is a BOUND wrapper in CPython and an unbound one
                // here, a repr difference only.
                if matches!(name, "__doc__" | "__init__" | "__new__" | "__sizeof__")
                    && crate::builtins::is_type_like_builtin(&tn)
                {
                    let ty = self.alloc(PyObj::Builtin(tn.clone()));
                    return self.get_attr(&ty, name);
                }
                // A TYPE object reports itself by name, not as an anonymous
                // instance of `type`: CPython's `type.__getattribute__` raises
                // `type object 'str' has no attribute 'x'`. pythonrs answered
                // `'type' object has no attribute 'x'` for every builtin type, so
                // the message never said WHICH type was missing the attribute —
                // the user-class path (which does name its class) already had the
                // right wording.
                if let Some(PyObj::Builtin(b)) = self.get(recv) {
                    let b = b.clone();
                    if crate::builtins::is_type_like_builtin(&b) {
                        return Err(format!(
                            "AttributeError: type object '{b}' has no attribute '{name}'"
                        ));
                    }
                }
                Err(format!(
                    "AttributeError: '{}' object has no attribute '{name}'",
                    attr_error_type_name(&tn)
                ))
            }
        }
    }

    /// Whether `class` inherits CPython's implicit `__hash__ = None`.
    ///
    /// Defining `__eq__` without `__hash__` makes a class unhashable, and
    /// CPython implements that by BINDING the slot to `None` rather than
    /// leaving it absent. The difference is observable: `C.__hash__ is None`
    /// answers True, `hasattr(C, '__hash__')` still answers True, and only
    /// CALLING it fails. pythonrs already refused to hash such an instance, but
    /// reported the attribute as an inherited slot wrapper, so the standard
    /// `cls.__hash__ is None` test for "is this type hashable" said the wrong
    /// thing about every class that defines `__eq__`.
    pub fn implicit_hash_none(&self, class: &str) -> bool {
        // The rule is per class BODY, not per MRO: the first class along the
        // MRO that writes either name decides, because that is the class whose
        // slot the subclass inherits. A subclass defining only `__eq__` is
        // therefore unhashable even when its base defined `__hash__` -- looking
        // the two names up across the whole MRO instead would find the base's
        // hash and call the subclass hashable, which CPython does not.
        for c in self.mro_of(class) {
            let Some(cd) = self.classes.get(&c) else {
                continue;
            };
            if cd.ns.contains_key("__hash__") {
                // An explicit `__hash__ = None` lands here too.
                return matches!(cd.ns.get("__hash__"), Some(Value::Undef));
            }
            if cd.ns.contains_key("__eq__") {
                return true;
            }
        }
        false
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
        // A bridged CPython object answers with its own `dir()` — its real
        // attribute list, which no native table could reproduce.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(v) {
            return crate::ffi::dir_names(id);
        }
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
            // `dir(module)` is its namespace, sorted.
            Some(PyObj::Module { slot, .. }) => {
                for k in self.module_globals[*slot].keys() {
                    set.insert(k.clone());
                }
            }
            // `dir(list)` / `dir(str)` — a builtin TYPE object lists the methods
            // of the type it names; anything else falls through to the value
            // branch below, which lists the methods of its own type.
            Some(PyObj::Builtin(n)) if !crate::builtins::type_dir_names(n).is_empty() => {
                let n = n.clone();
                self.collect_builtin_dir(&n, &mut set);
            }
            _ => {
                // `dir(x)` on a builtin VALUE ("abc", [1], 2.5) — the methods
                // its type responds to, plus the `object` surface every value
                // inherits. Every name here is one the value really answers to.
                let tn = self.type_name(v);
                self.collect_builtin_dir(&tn, &mut set);
            }
        }
        set.into_iter().collect()
    }

    /// Add the attribute names of builtin type `tn` to `set` — its own methods
    /// AND the inherited `object` surface, both from `type_dir_names` so a name
    /// can never be listed without dispatching. Empty for a type with no entry.
    fn collect_builtin_dir(&self, tn: &str, set: &mut BTreeSet<String>) {
        let names = crate::builtins::type_dir_names(tn);
        if names.is_empty() {
            return;
        }
        for n in names {
            set.insert(n.to_string());
        }
    }

    /// Add every name defined across `class`'s MRO namespaces (and any
    /// `__slots__` members) to `set`.
    /// The read-only `type` attributes that are a plain value rather than a
    /// method: the first entry of the MRO after the class itself, and the two
    /// signature/type-parameter slots that are empty for everything pythonrs
    /// builds. `mro` is the class's MRO, its own name first.
    fn type_object_data_attr(&mut self, mro: &[String], name: &str) -> Result<Value, String> {
        Ok(match name {
            // `__base__` is the "solid base" — for every class pythonrs models
            // that is simply the next entry in the MRO, and `object` for a class
            // with no bases of its own. Only `object` itself has none.
            "__base__" => match mro.get(1) {
                Some(b) => self.class_or_builtin_type(b.clone()),
                None if mro.first().map(String::as_str) == Some("object") => Value::Undef,
                None => self.alloc(PyObj::Builtin("object".into())),
            },
            // PEP 695 type parameters. pythonrs parses no generic class syntax,
            // so every class is unparameterized — CPython reports `()` for those
            // too, so this is the real answer and not a stand-in.
            "__type_params__" => self.new_tuple(vec![]),
            // The `__text_signature__` a C type carries in its docstring.
            // pythonrs has no C signatures to parse, and CPython answers `None`
            // for every type that has none.
            _ => Value::Undef,
        })
    }

    fn collect_class_dir(&self, class: &str, set: &mut BTreeSet<String>) {
        let mut any_user = false;
        for c in self.mro_of(class) {
            if let Some(cd) = self.classes.get(&c) {
                any_user = true;
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
        // Every class inherits `object`'s surface. Without this, `dir()` on a
        // user class listed only what its own body defined — five names for a
        // class CPython reports 32 for — and `dir(object())`, whose class has no
        // user namespace at all, came back EMPTY.
        for n in crate::builtins::OBJECT_DUNDERS {
            set.insert(n.to_string());
        }
        // A user class also carries the instance-dict machinery `object` itself
        // does not have; `object()` must stay at exactly `object`'s 24 names.
        if any_user {
            set.insert("__dict__".into());
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
            // A slot name is an identifier written inside the class body, so it
            // mangles like one — against the class that DECLARED it, which is
            // `c` here and not the instance's own class. `__slots__` itself
            // keeps the name as written (CPython leaves the tuple alone and
            // mangles only the descriptor it installs).
            let mangle = |s: String| crate::mangle::mangle(&c, &s).unwrap_or(s);
            // A literal `"__dict__"` entry asks for an instance dict ALONGSIDE
            // the slots, so the instance is not restricted at all. It is a
            // documented idiom -- `__slots__ = ("p", "__dict__")` is how a class
            // keeps a fast slot for its hot attribute and still accepts
            // arbitrary others -- and it used to raise
            // `'W' object has no attribute 'other' and no __dict__ for setting
            // new attributes`, i.e. exactly the error the entry exists to
            // prevent. It is not mangled: CPython matches it verbatim.
            let names: Vec<String> = match self.get(v) {
                Some(PyObj::List(items)) | Some(PyObj::Tuple(items)) => {
                    items.iter().filter_map(|it| self.as_str(it)).collect()
                }
                Some(PyObj::Str(s)) => vec![s.clone()],
                _ => Vec::new(),
            };
            if names.iter().any(|n| n == "__dict__") {
                return None;
            }
            for n in names {
                slots.insert(mangle(n));
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
            } else {
                // Not on the class itself: a `property` on the METACLASS is a data
                // descriptor for the class object, invoked with the class as its
                // instance. `EnumType.__members__` is one — `Month.__members__`
                // has to run the getter, not hand back the property object.
                let meta = self
                    .classes
                    .get(&cname)
                    .map(|c| c.metaclass.clone())
                    .unwrap_or_else(|| "type".into());
                if meta != "type" {
                    if let Some(v) = self.class_lookup(&meta, name) {
                        if let Some(PyObj::Property { fget, .. }) = self.get(&v) {
                            return AttrGet::Property {
                                fget: fget.clone(),
                                inst: recv.clone(),
                                owner: method_owner(self, &meta, name),
                            };
                        }
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
        // `functools.cached_property` — a non-data descriptor: it fires only when
        // the name is absent from the instance dict; once computed and cached
        // there, a later access reads the dict (via the `Plain` path below).
        if let Some(PyObj::CachedProperty { func, .. }) = self.get(&cls_attr) {
            if !in_instdict {
                return AttrGet::CachedProperty {
                    func: func.clone(),
                    inst: recv.clone(),
                    name: name.to_string(),
                };
            }
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
        // A live CPython object (`decimal.getcontext().prec = 6`) sets through the
        // bridge.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = self.foreign_id(recv) {
            return crate::ffi::set_attr(self, id, name, &val);
        }
        // SimpleNamespace: attribute writes go into its bag.
        if let Some(PyObj::Namespace { attrs }) = self.get_mut(recv) {
            attrs.insert(name.to_string(), val);
            return Ok(());
        }
        // `obj.__class__ = C` RETYPES the object in place (CPython's
        // `object.__class__` setter) — it is not a normal attribute store, so it
        // must run before the instance dict / `__slots__` paths, which would
        // otherwise stash a shadowing entry that `type(obj)` never reads.
        if name == "__class__" {
            return self.set_class(recv, &val);
        }
        // `defaultdict.default_factory` is writable in CPython: rebinding it
        // changes what `__missing__` produces from that point on, and setting it
        // to `None` turns the defaultdict back into a plain KeyError-raising dict.
        if name == "default_factory" {
            if let Value::Obj(i) = recv {
                if let Some(m) = self.dict_meta.get_mut(i) {
                    if m.kind == DictKind::DefaultDict {
                        m.factory = (!matches!(val, Value::Undef)).then_some(val);
                        return Ok(());
                    }
                }
            }
        }
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
        // `sys.stdout` / `sys.stderr` reassignment records a host-level redirect
        // (see the `stdout_target`/`stderr_target` fields), so `print` writes to
        // the new stream even though `import` is not cached. A reset back to the
        // native stream (`File { id: 0/1 }`) clears the redirect.
        if let Some(PyObj::Module { name: mname, .. }) = self.get(recv) {
            if mname == "sys" && (name == "stdout" || name == "stderr") {
                let is_stdout = name == "stdout";
                let native_id = if is_stdout { 0 } else { 1 };
                let target = match &val {
                    v if matches!(self.get(v), Some(PyObj::File { id }) if *id == native_id) => {
                        None
                    }
                    _ => Some(val.clone()),
                };
                if is_stdout {
                    self.stdout_target = target;
                } else {
                    self.stderr_target = target;
                }
            }
        }
        // An exception instance carries arbitrary attributes, as CPython's do:
        // `unittest`'s runner stamps bookkeeping onto the exceptions it catches,
        // and `raise X from Y` style helpers attach their own state.
        if matches!(self.get(recv), Some(PyObj::Exception { .. })) {
            if let Value::Obj(id) = recv {
                let id = *id;
                self.func_attrs
                    .entry(id)
                    .or_default()
                    .insert(name.to_string(), val);
                return Ok(());
            }
        }
        // A `property` or C-level descriptor carries a writable `__doc__`, and
        // the stdlib uses it: `dis.py` documents each field of its `_Instruction`
        // namedtuple with `_Instruction.opname.__doc__ = "…"`, which is the line
        // `inspect` — and so `traceback`, `logging`, `unittest`, `dataclasses` —
        // fails on if the assignment is refused. The side table functions carry
        // their attributes in serves for these too.
        if matches!(
            self.get(recv),
            Some(PyObj::Property { .. }) | Some(PyObj::Descriptor { .. })
        ) {
            if let Value::Obj(id) = recv {
                let id = *id;
                self.func_attrs
                    .entry(id)
                    .or_default()
                    .insert(name.to_string(), val);
                return Ok(());
            }
        }
        // A function object carries a writable attribute dict (`func.attr = v`,
        // `func.__isabstractmethod__ = True`).
        if matches!(self.get(recv), Some(PyObj::Func(_))) {
            if let Value::Obj(id) = recv {
                let id = *id;
                self.func_attrs
                    .entry(id)
                    .or_default()
                    .insert(name.to_string(), val);
                return Ok(());
            }
        }
        if let Some(slot) = self.module_slot(recv) {
            self.module_globals[slot].insert(name.to_string(), val);
            return Ok(());
        }
        match self.get_mut(recv) {
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

    /// `obj.__class__ = C` — retype an instance in place.
    ///
    /// CPython's `object.__class__` setter, in its order: the value must be a
    /// class at all; then both the old and the new type must be mutable (heap)
    /// types; then their layouts must match. Two pure-Python classes have the
    /// same layout when they add the same slots — so a `__dict__`-carrying class
    /// and a fully `__slots__`-ed one are never interchangeable, and two slotted
    /// classes are only interchangeable when the slot names agree.
    fn set_class(&mut self, recv: &Value, val: &Value) -> Result<(), String> {
        let new = match self.get(val).cloned() {
            Some(PyObj::Class(c)) => c,
            // A builtin type object (`int`, `ValueError`) is a class, but a
            // static one — assignment stops at the mutability check.
            Some(PyObj::Builtin(n))
                if crate::builtins::BUILTIN_TYPES.contains(&n.as_str())
                    || crate::builtins::is_exception_class(&n) =>
            {
                return Err(type_error(
                    "__class__ assignment only supported for mutable types or \
                     ModuleType subclasses",
                ))
            }
            _ => {
                return Err(type_error(&format!(
                    "__class__ must be set to a class, not '{}' object",
                    self.type_name(val)
                )))
            }
        };
        let old = match self.get(recv) {
            Some(PyObj::Instance(inst)) => inst.class.clone(),
            // Everything else (int, str, list, a bridged object, a module) is a
            // static type as far as pythonrs is concerned.
            _ => {
                return Err(type_error(
                    "__class__ assignment only supported for mutable types or \
                     ModuleType subclasses",
                ))
            }
        };
        if self.slots_of(&old) != self.slots_of(&new) {
            return Err(type_error(&format!(
                "__class__ assignment: '{new}' object layout differs from '{old}'"
            )));
        }
        if let Some(PyObj::Instance(inst)) = self.get_mut(recv) {
            inst.class = new;
        }
        Ok(())
    }

    pub fn del_attr(&mut self, recv: &Value, name: &str) -> Result<(), String> {
        // `del obj.__class__` is rejected by the setter slot itself, before any
        // instance-dict lookup — CPython raises TypeError, not AttributeError.
        if name == "__class__" {
            return Err(type_error("can't delete __class__ attribute"));
        }
        if let Some(PyObj::Instance(inst)) = self.get(recv) {
            let dict = inst.dict.clone();
            if self.inst_attr_del(&dict, name) {
                return Ok(());
            }
        }
        // `delattr(SomeClass, name)` removes a class attribute from its namespace.
        if let Some(PyObj::Class(cname)) = self.get(recv) {
            let cname = cname.clone();
            if let Some(cd) = self.classes.get_mut(&cname) {
                if cd.ns.shift_remove(name).is_some() {
                    return Ok(());
                }
            }
            return Err(format!(
                "AttributeError: type object '{cname}' has no attribute '{name}'"
            ));
        }
        // A SimpleNamespace attribute deletion.
        if let Some(PyObj::Namespace { attrs }) = self.get_mut(recv) {
            if attrs.shift_remove(name).is_some() {
                return Ok(());
            }
        }
        Err(format!(
            "AttributeError: '{}' object has no attribute '{name}'",
            self.type_name(recv)
        ))
    }

    /// Register a class built from a run class-body namespace.
    pub fn register_class(&mut self, name: &str, bases: Vec<String>, ns: NameMap) -> Value {
        self.register_class_meta(name, bases, ns, "type")
    }

    /// Register a class whose metaclass (`type(cls)`) is `metaclass` — `"type"`
    /// for an ordinary class, a user metaclass name for `class A(metaclass=M)`.
    pub fn register_class_meta(
        &mut self,
        name: &str,
        bases: Vec<String>,
        ns: NameMap,
        metaclass: &str,
    ) -> Value {
        // Classes live in ONE table keyed by bare name, so a class that shadows
        // one of its own bases would overwrite it and then list itself as its
        // base — `mro_of` recurses on that forever and kills the process. The
        // stdlib does this constantly: every `encodings/*.py` opens with
        // `class Codec(codecs.Codec)`, and `class StreamWriter(codecs.
        // StreamWriter)` right after. Give the shadowing class its own key
        // (display name unchanged) so the base it inherits from stays reachable.
        // Check the whole ancestry, not just the direct bases: `class
        // IncrementalDecoder(codecs.BufferedIncrementalDecoder)` shadows a
        // GRANDparent, and overwriting that entry makes the direct base's own
        // base list point at this new class — the same cycle, one level up.
        // The same collision happens against a BUILTIN type name, and it is worse:
        // `enum.py` opens with `class property(DynamicClassAttribute)`, which
        // replaced the builtin `property` for the whole process. `collections`
        // then built its namedtuple field accessors out of enum's class, and
        // `NT.field` raised through ITS `__get__` — so every namedtuple in the
        // program broke the moment anything imported `enum`.
        let ancestors: Vec<String> = bases.iter().flat_map(|b| self.mro_of(b)).collect();
        let key =
            if ancestors.iter().any(|b| b == name) || crate::builtins::shadows_builtin_type(name) {
                let mut n = 1usize;
                loop {
                    let cand = format!("{name}#{n}");
                    if !self.classes.contains_key(&cand) {
                        break cand;
                    }
                    n += 1;
                }
            } else {
                name.to_string()
            };
        let name_for_key = key.clone();
        // A new class can change what an existing name resolves to (the
        // shadowing disambiguation above rekeys classes), so every memoized
        // linearization is dropped here.
        self.mro_cache.borrow_mut().clear();
        let mro = {
            let mut out = vec![name_for_key.clone()];
            for b in &bases {
                for m in self.mro_of(b) {
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
            }
            out
        };
        // Tag each method defined here with its owning class, so zero-arg
        // `super()` resolves even when the bound method is called through a stored
        // reference (`f = obj.m; f()`), not just `obj.m()` — the latter passes the
        // owner explicitly, the former relies on `FuncVal::owner`.
        for v in ns.values() {
            if let Some(PyObj::Func(fv)) = self.get_mut(v) {
                if fv.owner.is_none() {
                    Rc::make_mut(fv).owner = Some(name_for_key.clone());
                }
            }
        }
        // Emulate `__set_name__` for `functools.cached_property`: it learns the
        // attribute name from its class-namespace key so it can cache into the
        // instance dict on first access.
        let cp_names: Vec<(Value, String)> = ns
            .iter()
            .filter(|(_, v)| {
                matches!(self.get(v), Some(PyObj::CachedProperty { name, .. }) if name.is_empty())
            })
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        for (val, key) in cp_names {
            if let Some(PyObj::CachedProperty { name, .. }) = self.get_mut(&val) {
                *name = key;
            }
        }
        // The same `__set_name__` emulation for `property`: `C.x.__name__` is
        // the class-namespace key, not the getter's name, whenever the two
        // differ (`y = property(get_x)` names itself `y`).
        let prop_names: Vec<(Value, String)> = ns
            .iter()
            .filter(|(_, v)| {
                matches!(self.get(v), Some(PyObj::Property { name, .. }) if name.is_empty())
            })
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        for (val, key) in prop_names {
            if let Some(PyObj::Property { name, .. }) = self.get_mut(&val) {
                *name = key;
            }
        }
        // `__module__` comes from the class BODY namespace (`run_class_body` puts
        // it there from the defining module), falling back to the running module
        // for a bare registration that never ran a body.
        let module = ns
            .get("__module__")
            .and_then(|v| self.as_str(v))
            .or_else(|| {
                self.globals()
                    .get("__name__")
                    .cloned()
                    .and_then(|v| self.as_str(&v))
            })
            .unwrap_or_else(|| "__main__".to_string());
        self.classes.insert(
            key.clone(),
            ClassDef {
                name: name.to_string(),
                // Set by `build_class` once known; a bare registration (or an
                // older cache) leaves it empty, falling back to `name`.
                qualname: String::new(),
                bases,
                ns,
                mro,
                metaclass: metaclass.to_string(),
                module,
            },
        );
        self.alloc(PyObj::Class(key))
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
                    // A named base method bound as an attribute (`g = d.get`):
                    // route back through `call_method` so it reaches the payload
                    // via `base_dispatch`.
                    if let Some(m) = name.strip_prefix("__base_method__.") {
                        return call_method(&recv, m, args, kwargs);
                    }
                    // A native `_random.Random` method bound to an instance.
                    if let Some(m) = name.strip_prefix("_random.Random.") {
                        if let Value::Obj(id) = &recv {
                            return crate::builtins::random_method(*id, m, &args);
                        }
                    }
                    // `func.__get__(obj, cls)` — the descriptor bind: `recv` is the
                    // function, arg0 is the instance (`None` → unbound function).
                    if name == "function.__get__" {
                        let obj = args.into_iter().next().unwrap_or(Value::Undef);
                        return Ok(match obj {
                            Value::Undef => recv,
                            inst => with_host(|h| {
                                h.alloc(PyObj::BoundMethod {
                                    recv: inst,
                                    func: recv.clone(),
                                })
                            }),
                        });
                    }
                    crate::builtins::call_type_method(&recv, &name, args, kwargs)
                }
                Some(PyObj::Func(fv)) => run_user_func(&fv, Some(recv), None, args, kwargs),
                _ => Err(type_error("bound method is not callable")),
            }
        }
        Some(PyObj::Class(name)) => instantiate(&name, args, kwargs),
        // An unbound slot wrapper (`int.__str__`, `object.__repr__`) is callable:
        // `wrapper(self, *rest)` runs the slot on its first argument. Dispatch to
        // the base type directly — re-looking-up the method on `self` could hit
        // this same wrapper (enum stores `member_type.__str__` as the class's
        // `__str__`) and recurse.
        Some(PyObj::Descriptor {
            kind: DescKind::WrapperDescriptor,
            qual,
        }) => {
            let (base, method) = qual.split_once('.').unwrap_or(("", qual.as_str()));
            let (base, method) = (base.to_string(), method.to_string());
            let mut it = args.into_iter();
            let recv = it
                .next()
                .ok_or_else(|| type_error(&format!("unbound method {qual}() needs an argument")))?;
            let rest: Vec<Value> = it.collect();
            // A slot wrapper type-checks its receiver before running the slot,
            // exactly as the method descriptors above do: `str.__eq__(5, 'a')`
            // is a TypeError naming the descriptor, not `NotImplemented` from
            // int's own comparison.
            if let Some(e) = crate::builtins::unbound_receiver_error(&base, &method, &recv) {
                return Err(e);
            }
            // `object.__repr__(x)` is `object`'s OWN slot, so it reports the
            // default `<type object at 0x…>` even for a value whose type
            // overrides `__repr__` — that is the whole reason to reach for it.
            if base == "object" && method == "__repr__" {
                return Ok(with_host(|h| {
                    let name = match h.get(&recv) {
                        // A user class is named by its module path here, the
                        // same way its own default repr names it.
                        Some(PyObj::Instance(i)) => {
                            let c = i.class.clone();
                            h.class_display_path(&c)
                        }
                        _ => h.type_name(&recv),
                    };
                    let s = format!("<{name} object at 0x{:012x}>", h.addr_of(&recv));
                    h.new_str(s)
                }));
            }
            // `object.__str__` is not a renderer of its own: it defers to the
            // object's `__repr__`, so `object.__str__('a')` is `"'a'"`.
            if base == "object" && method == "__str__" {
                return Ok(with_host(|h| {
                    let s = h.repr_of(&recv);
                    h.new_str(s)
                }));
            }
            if crate::builtins::is_type_like_builtin(&base) {
                let payload =
                    with_host(|h| h.base_payload_any(&recv)).unwrap_or_else(|| recv.clone());
                return base_dispatch(&recv, &payload, &base, &method, rest, kwargs);
            }
            call_method(&recv, &method, rest, kwargs)
        }
        // Calling a generic alias constructs its origin (`list[int]()` -> list()).
        Some(PyObj::GenericAlias { origin, .. }) => invoke(&origin, args, kwargs),
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
/// True if `recv` is a type object that parameterizes to a generic alias when
/// subscripted (`list[int]`, `dict[str, int]`, or any user class). Builtin
/// FUNCTIONS (`len`, `print`) are excluded — only type builtins qualify.
pub fn is_generic_subscriptable(recv: &Value) -> bool {
    let cname = with_host(|h| match h.get(recv) {
        Some(PyObj::Class(n)) => Some(n.clone()),
        _ => None,
    });
    // A user class parameterizes ONLY if `__class_getitem__` is somewhere in its
    // MRO — inherited from `Generic`/an ABC, or written on the class. CPython
    // does not hand every type a generic alias: a plain `class Box: pass` makes
    // `Box[int]` a `TypeError: type 'Box' is not subscriptable`, and treating it
    // as parameterization silently accepted programs CPython rejects.
    if let Some(cname) = cname {
        return with_host(|h| h.class_lookup(&cname, "__class_getitem__").is_some());
    }
    with_host(|h| match h.get(recv) {
        // The builtin container/type objects that carry `__class_getitem__` in
        // CPython. `str`/`bytes`/`int`/`float`/`bool` do NOT (subscripting them
        // stays a TypeError), matching CPython.
        Some(PyObj::Builtin(n)) => matches!(
            n.as_str(),
            "list" | "dict" | "tuple" | "set" | "frozenset" | "type"
            // typing's `Generic[T]` builds a generic alias; used as a base
            // (`class C(Generic[T])`), its `__mro_entries__` substitutes `Generic`.
                | "Generic"
            // `re.Pattern[str]` / `re.Match[str]` in annotations.
                | "re.Pattern" | "re.Match"
        ),
        _ => false,
    })
}

/// Build the generic alias for `recv[idx]` where `recv` is a type object. A user
/// class with `__class_getitem__` uses its own hook; every other type builds a
/// `types.GenericAlias(recv, idx)` so the alias is the SAME type the stdlib gets
/// from `from types import GenericAlias`. Runs with NO host borrow held (it calls
/// back into the VM), so the caller must invoke it outside `with_host`.
pub fn generic_alias(recv: &Value, idx: &Value) -> Result<Value, String> {
    // A class that defines `__class_getitem__` (a classmethod) drives its own
    // parameterization — the ABCs bind `classmethod(GenericAlias)` this way.
    let hook = with_host(|h| match h.get(recv) {
        Some(PyObj::Class(cn)) => {
            let cn = cn.clone();
            h.class_lookup(&cn, "__class_getitem__").map(|_| ())
        }
        _ => None,
    });
    if hook.is_some() {
        // `get_attr` binds the classmethod to `recv` as `cls`; invoke with `idx`.
        let bound = with_host(|h| h.get_attr(recv, "__class_getitem__"))?;
        return invoke(&bound, vec![idx.clone()], vec![]);
    }
    // Build a native GenericAlias directly — no module import, so types.py's own
    // `type(list[int])` cannot recurse into the still-loading `types` module.
    Ok(with_host(|h| {
        let args = match h.get(idx) {
            Some(PyObj::Tuple(items)) => items.clone(),
            _ => vec![idx.clone()],
        };
        h.alloc(PyObj::GenericAlias {
            origin: recv.clone(),
            args,
        })
    }))
}

pub fn call_named(
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    if let Some(r) = call_rust_ffi(name, &args) {
        return r;
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
    with_host(|h| h.note_name_miss(name));
    Err(name_error(name))
}

/// The two names that belong to the inline-Rust FFI rather than to any Python
/// namespace: `__rust_compile(b64, line)`, which the `rust { ... }` desugar
/// emits, and every function such a block exported (callable by bareword).
/// `None` when `name` is neither, so the caller falls through.
///
/// Both are reached through a plain CALL, which now resolves its callee to a
/// VALUE first — so they have to answer as builtin objects too, not only by
/// name (`is_rust_ffi_name` is what makes a bare-name read produce one).
pub fn call_rust_ffi(name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if name == "__rust_compile" {
        let b64 = args
            .first()
            .map(|v| with_host(|h| h.str_of(v)))
            .unwrap_or_default();
        return Some(fusevm::ffi::compile_and_register(&b64).map(|_| Value::Undef));
    }
    if fusevm::ffi::is_registered(name) {
        let margs: Vec<Value> = args.iter().map(marshal_ffi_arg).collect();
        return fusevm::ffi::try_call(name, &margs);
    }
    None
}

/// Whether a bare name belongs to the inline-Rust FFI (see [`call_rust_ffi`]).
pub fn is_rust_ffi_name(name: &str) -> bool {
    name == "__rust_compile" || fusevm::ffi::is_registered(name)
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

/// Wrap an unhashable-key `TypeError` with the container context CPython adds.
///
/// CPython reports the bare `unhashable type: 'X'` only from `hash()` itself.
/// Reached through a container it says which role the key was playing:
///
/// ```text
/// {u}        cannot use 'U' as a set element (unhashable type: 'U')
/// {u: 1}     cannot use 'U' as a dict key (unhashable type: 'U')
/// ```
///
/// The two names can differ, and both are load-bearing: the OUTER one is the
/// type of the key the container was handed, the INNER one is the type that
/// actually failed to hash. For `{(u,): 1}` CPython says
/// `cannot use 'tuple' as a dict key (unhashable type: 'U')` -- the tuple is
/// what you tried to key with, the instance inside it is what could not hash.
/// So the outer name comes from `key` here and the inner is left exactly as the
/// hashing code reported it.
///
/// Anything that is not an unhashable-key error passes through untouched.
/// What a key was being used AS, for the message [`wrap_unhashable`] builds.
///
/// A role rather than a sniff of the container, because the two are not the
/// same question: when a `set`/`dict` is being CONSTRUCTED from an iterable
/// there is no container yet to inspect, and `hash(x)` has no container at all.
/// Passing it explicitly also makes the compiler list every site that keys a
/// value, so a new one cannot quietly inherit the wrong wording.
#[derive(Clone, Copy)]
pub enum KeyRole<'a> {
    /// `hash(x)` — CPython reports the bare `unhashable type: 'X'` here.
    Bare,
    Set,
    Dict,
    /// Decide from the container's runtime type: a subscript or `in` whose
    /// receiver may be a list, a dict or a set.
    Of(&'a Value),
}

pub fn wrap_unhashable(h: &PyHost, e: String, role: KeyRole, key: &Value) -> String {
    const PREFIX: &str = "TypeError: unhashable type:";
    if !e.starts_with(PREFIX) {
        return e;
    }
    let role = match role {
        KeyRole::Bare => return e,
        KeyRole::Set => "a set element",
        KeyRole::Dict => "a dict key",
        KeyRole::Of(c) => match h.get(c) {
            Some(PyObj::Set(_) | PyObj::Frozenset(_)) => "a set element",
            Some(PyObj::Dict(_)) => "a dict key",
            _ => return e,
        },
    };
    let inner = e.trim_start_matches("TypeError: ");
    format!(
        "TypeError: cannot use '{}' as {role} ({inner})",
        h.type_name(key)
    )
}

/// Whether `v` could name a heap object at all.
///
/// [`PyHost::get`] resolves ONLY `Value::Obj`, so for every other variant --
/// `Int`, `Float`, `Bool`, `Undef`, and fusevm's inline `Str`/`Arr` -- any
/// question of the form "is this a `PyObj::X`?" is answered `false` without
/// consulting the heap. Asking it before taking the thread-local borrow is
/// therefore exactly equivalent, and skips the borrow entirely for the unboxed
/// scalars that arithmetic runs on.
#[inline]
pub fn is_heap(v: &Value) -> bool {
    matches!(v, Value::Obj(_))
}

/// For an operand in an arithmetic/comparison operation: if `v` is a
/// builtin-subclass instance that does not override the operator `dunder`,
/// return its native payload (so the native operation runs and yields the base
/// type — `C(5) + 3 == 8`, a plain `int`). Otherwise return `v` unchanged.
pub fn subclass_operand(v: &Value, dunder: &str) -> Value {
    // Both operands of every arithmetic op come through here, and the common
    // ones are unboxed numbers that cannot be a subclass instance.
    if !is_heap(v) {
        return v.clone();
    }
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
        h.new_instance(cname.to_string(), NameMap::default())
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
/// Methods on a module's `__dict__`. Writes land in the module's globals slot so
/// the module's own code sees them — `enum.global_enum` publishing an enum's
/// members into `calendar` depends on exactly that. Reads are delegated to the
/// ordinary dict methods over a snapshot.
fn module_dict_method(
    slot: usize,
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    match name {
        "update" => {
            // `update(other, **kw)`: a mapping contributes its items, any other
            // iterable its key/value pairs.
            let mut pairs: Vec<(String, Value)> = Vec::new();
            if let Some(src) = args.first() {
                let items = match with_host(|h| h.get(src).cloned()) {
                    Some(PyObj::Dict(d)) => {
                        d.values().map(|(k, v)| (k.clone(), v.clone())).collect()
                    }
                    _ if with_host(|h| h.module_dict_slot(src)).is_some() => {
                        let s = with_host(|h| h.module_dict_slot(src)).unwrap();
                        with_host(|h| {
                            h.module_globals_pairs(s)
                                .into_iter()
                                .map(|(k, v)| (h.new_str(k), v))
                                .collect::<Vec<_>>()
                        })
                    }
                    // Fall back to the mapping protocol, then to pair-iteration.
                    _ => match call_method(src, "items", vec![], vec![]) {
                        Ok(items) => {
                            let mut out = Vec::new();
                            for it in iter_vec(&items)? {
                                let kv = iter_vec(&it)?;
                                if kv.len() != 2 {
                                    return Err(type_error(
                                        "dictionary update sequence element has length != 2",
                                    ));
                                }
                                out.push((kv[0].clone(), kv[1].clone()));
                            }
                            out
                        }
                        Err(_) => {
                            let mut out = Vec::new();
                            for it in iter_vec(src)? {
                                let kv = iter_vec(&it)?;
                                if kv.len() != 2 {
                                    return Err(type_error(
                                        "dictionary update sequence element has length != 2",
                                    ));
                                }
                                out.push((kv[0].clone(), kv[1].clone()));
                            }
                            out
                        }
                    },
                };
                for (k, v) in items {
                    let ks = with_host(|h| h.as_str(&k))
                        .ok_or_else(|| type_error("module namespace keys must be strings"))?;
                    pairs.push((ks, v));
                }
            }
            for (k, v) in kwargs {
                pairs.push((k, v));
            }
            with_host(|h| {
                for (k, v) in pairs {
                    h.module_globals[slot].insert(k, v);
                }
            });
            Ok(Value::Undef)
        }
        "setdefault" => {
            let k = with_host(|h| args.first().and_then(|a| h.as_str(a)))
                .ok_or_else(|| type_error("module namespace keys must be strings"))?;
            let default = args.get(1).cloned().unwrap_or(Value::Undef);
            Ok(with_host(|h| {
                h.module_globals[slot].entry(k).or_insert(default).clone()
            }))
        }
        "pop" => {
            let k = with_host(|h| args.first().and_then(|a| h.as_str(a)))
                .ok_or_else(|| type_error("module namespace keys must be strings"))?;
            match with_host(|h| h.module_globals[slot].shift_remove(&k)) {
                Some(v) => Ok(v),
                None => match args.get(1) {
                    Some(d) => Ok(d.clone()),
                    None => Err(with_host(|h| {
                        let kv = h.new_str(k);
                        h.key_error(&kv)
                    })),
                },
            }
        }
        "popitem" => match with_host(|h| h.module_globals[slot].pop()) {
            Some((k, v)) => Ok(with_host(|h| {
                let kv = h.new_str(k);
                h.new_tuple(vec![kv, v])
            })),
            None => Err(with_host(|h| {
                let s = h.new_str("popitem(): dictionary is empty");
                h.key_error(&s)
            })),
        },
        "clear" => {
            with_host(|h| h.module_globals[slot].clear());
            Ok(Value::Undef)
        }
        // Pure reads: the snapshot answers exactly as a real dict would.
        _ => {
            let snap = with_host(|h| h.module_dict_snapshot(recv))
                .ok_or_else(|| type_error("not a module namespace"))?;
            call_method(&snap, name, args, kwargs)
        }
    }
}

/// `recv.name(*args, **kwargs)`. The attribute lookup is fused into the call, so
/// a missing method never reaches `get_attr` — record the receiver here too, or
/// an uncaught `obj.mispelled()` would render without CPython's hint.
/// `'X' object does not support the [asynchronous ]context manager protocol
/// (missed __exit__ method)` — CPython's `SETUP_WITH` / `BEFORE_ASYNC_WITH`
/// error, naming whichever half of the protocol is absent.
///
/// The lookup order is CPython's: `__exit__` is checked FIRST, so an object
/// carrying only `__enter__` is rejected before that `__enter__` can run, and
/// an object carrying neither is reported against `__exit__`.
fn context_manager_error(recv: &Value, is_async: bool) -> Option<String> {
    // Only receivers whose protocol membership an attribute probe answers
    // correctly are checked. A user instance resolves its dunders through its
    // class, and none of the core scalars/containers has an enter/exit half —
    // for both, `get_attr` is the same lookup the call would do. The natively
    // shadowed managers (a file, a lock, `contextlib.redirect_stdout`) and any
    // bridged CPython object dispatch `__enter__`/`__exit__` inside
    // `call_method_inner` without exposing them as attributes, so probing them
    // would report a missing method that is in fact there; those keep the
    // pre-check behavior.
    let probeable = with_host(|h| {
        matches!(
            recv,
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Undef
        ) || matches!(
            h.get(recv),
            Some(PyObj::Instance(_))
                | Some(PyObj::Str(_))
                | Some(PyObj::Bytes(_))
                | Some(PyObj::Bytearray(_))
                | Some(PyObj::List(_))
                | Some(PyObj::Tuple(_))
                | Some(PyObj::Dict(_))
                | Some(PyObj::Set(_))
                | Some(PyObj::Frozenset(_))
                | Some(PyObj::Range { .. })
                | Some(PyObj::BigInt(_))
                | Some(PyObj::Complex(_, _))
                | Some(PyObj::Generator { .. })
        )
    });
    if !probeable {
        return None;
    }
    let (enter, exit, adj) = if is_async {
        ("__aenter__", "__aexit__", "asynchronous ")
    } else {
        ("__enter__", "__exit__", "")
    };
    // CPython's order: `__exit__` first, so an object carrying only `__enter__`
    // is rejected before that `__enter__` runs, and one carrying neither is
    // reported against `__exit__`.
    let missing = [exit, enter]
        .into_iter()
        .find(|m| with_host(|h| h.get_attr(recv, m)).is_err())?;
    Some(type_error(&format!(
        "'{}' object does not support the {adj}context manager protocol \
         (missed {missing} method)",
        with_host(|h| h.type_name(recv))
    )))
}

pub fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // The `with` desugar routes its ENTRY call through a dot-prefixed sentinel
    // (`.__enter__` / `.__aenter__`) so the context-manager protocol check runs
    // before the call. A leading dot is unwriteable in Python source — the same
    // trick the desugar's `.ctx` temporaries use — so an explicit user
    // `obj.__enter__()` keeps raising the ordinary `AttributeError` that
    // CPython raises for it.
    if let Some(entry) = name.strip_prefix('.') {
        if let Some(e) = context_manager_error(recv, entry.starts_with("__a")) {
            return Err(e);
        }
        return call_method(recv, entry, args, kwargs);
    }
    let r = call_method_inner(recv, name, args, kwargs);
    if let Err(e) = &r {
        if e.starts_with("AttributeError:") && e.contains(&format!("'{name}'")) {
            with_host(|h| h.note_attr_miss(recv, name));
        }
    }
    r
}

fn call_method_inner(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // A module `__dict__` view: the mutators write through to the module's globals
    // slot; everything else is a pure read and is answered from a snapshot by the
    // ordinary dict methods, so the two can never disagree about semantics.
    if let Some(slot) = with_host(|h| h.module_dict_slot(recv)) {
        return module_dict_method(slot, recv, name, args, kwargs);
    }
    // `_random.Random` (and subclasses) — the RNG methods dispatch against the
    // instance's Mersenne Twister state, not the class namespace.
    if matches!(
        name,
        "random" | "seed" | "getrandbits" | "getstate" | "setstate"
    ) {
        if let Some(id) = with_host(|h| match (recv, h.get(recv)) {
            (Value::Obj(oid), Some(PyObj::Instance(i)))
                if h.mro_of(&i.class).iter().any(|c| c == "_random.Random") =>
            {
                Some(*oid)
            }
            _ => None,
        }) {
            return crate::builtins::random_method(id, name, &args);
        }
    }
    // Only the variants the match below destructures need an owned copy. Cloning
    // unconditionally deep-copied the receiver on EVERY method call, so
    // `a.append(x)` copied the whole list before appending to it — an append loop
    // was quadratic (400k appends: >90s, against CPython's 0.06s). A builtin
    // container goes straight to `call_type_method`, which re-reads through the
    // handle; that is also exactly what the `_` arm below does with it.
    let needs_owned = with_host(|h| {
        // `Foreign` is destructured by the `foreign.method(...)` arm below, so it
        // belongs here too. Its variant is feature-gated, hence the separate test.
        // Leaving it out sent EVERY method call on a CPython object to
        // `call_type_method`, which knows only pythonrs's native types and so
        // reported the method as a missing attribute — `string.Formatter()`,
        // `_thread.RLock()` and `with contextlib.contextmanager(...)` (whose
        // `__enter__` is a method call on the foreign context manager) all failed
        // while the plain attribute READ of the same name resolved fine.
        #[cfg(feature = "stdlib-ffi")]
        if matches!(h.get(recv), Some(PyObj::Foreign(_))) {
            return true;
        }
        matches!(
            h.get(recv),
            Some(
                PyObj::Pattern { .. }
                    | PyObj::Match { .. }
                    | PyObj::Func(_)
                    | PyObj::MappingProxy { .. }
                    | PyObj::Lock { .. }
                    | PyObj::Redirect { .. }
                    | PyObj::Instance(_)
                    | PyObj::Class(_)
                    | PyObj::Module { .. }
                    | PyObj::Super { .. }
                    | PyObj::Builtin(_)
            )
        )
    });
    // `f.attr(...)` where `attr` is an attribute stored ON a function object
    // (`functools.wraps` sets `__wrapped__`, and any code may set its own) is
    // just getattr-then-call. The type-method fallback below only knows the
    // methods of the receiver's TYPE, so it reported "'function' object has no
    // attribute '__wrapped__'" for a call that plain attribute access resolved
    // fine — `(f.__wrapped__)(x)` worked while `f.__wrapped__(x)` did not.
    if let Value::Obj(id) = recv {
        let stored = with_host(|h| {
            matches!(h.get(recv), Some(PyObj::Func(_)))
                .then(|| h.func_attrs.get(id).and_then(|m| m.get(name)).cloned())
                .flatten()
        });
        if let Some(f) = stored {
            return invoke(&f, args, kwargs);
        }
    }
    if !needs_owned {
        return crate::builtins::call_type_method(recv, name, args, kwargs);
    }
    let obj = with_host(|h| h.get(recv).cloned());
    match obj {
        // `re.Pattern` / `re.Match` methods (`p.search(s)`, `m.group(1)`, …).
        Some(PyObj::Pattern { .. }) => {
            crate::builtins::re_pattern_method(recv, name, &args, &kwargs)
        }
        Some(PyObj::Match { .. }) => crate::builtins::re_match_method(recv, name, &args),
        // `f.__annotate__(fmt)` as a fused method call. `__annotate__` is a plain
        // callable attribute rather than a function method, so the attribute has
        // to be resolved first (an unannotated function yields `None`, and
        // calling that raises the same `TypeError` CPython does).
        Some(PyObj::Func(_)) if name == "__annotate__" => {
            let f = with_host(|h| h.get_attr(recv, name))?;
            invoke(&f, args, kwargs)
        }
        // `func.__get__(obj, cls)` invoked directly (not via attribute read):
        // the descriptor bind — `obj` is None → the plain function, else a bound
        // method. Mirrors the `__get__` attribute a function exposes.
        Some(PyObj::Func(_)) if name == "__get__" => {
            let inst = args.into_iter().next().unwrap_or(Value::Undef);
            Ok(match inst {
                Value::Undef => recv.clone(),
                inst => with_host(|h| {
                    h.alloc(PyObj::BoundMethod {
                        recv: inst,
                        func: recv.clone(),
                    })
                }),
            })
        }
        // `types.MappingProxyType` (a type's `__dict__`) is a read-only view:
        // read methods delegate to the backing dict; mutators are rejected.
        Some(PyObj::MappingProxy { dict }) => match name {
            "get" | "keys" | "values" | "items" | "copy" | "__getitem__" | "__contains__"
            | "__len__" | "__iter__" | "__or__" | "__ror__" | "__eq__" | "__ne__"
            | "__reversed__" => call_method(&dict, name, args, kwargs),
            _ => Err(type_error(&format!(
                "'mappingproxy' object has no attribute '{name}'"
            ))),
        },
        // `contextlib.redirect_stdout`/`redirect_stderr` context manager: retarget
        // the stream on `__enter__` (saving the prior target on the instance so
        // nesting restores correctly) and restore it on `__exit__`.
        // `_thread` lock methods. Single-threaded, so acquire always succeeds;
        // a reentrant lock counts nesting.
        Some(PyObj::Lock { reentrant, .. }) => match name {
            "acquire" | "__enter__" => {
                let held = with_host(
                    |h| matches!(h.get(recv), Some(PyObj::Lock { count, .. }) if *count > 0),
                );
                // `acquire(blocking=False)` on a held non-reentrant lock fails,
                // and saying so is what makes `threading.Condition._is_owned`
                // work: its default implementation probes with a non-blocking
                // acquire and reads a refusal as "someone holds this".
                let blocking = match args.first() {
                    Some(v) => with_host(|h| h.truthy(v)),
                    None => true,
                };
                if held && !reentrant {
                    if !blocking {
                        return Ok(Value::Bool(false));
                    }
                    // One thread, so a blocking acquire of a lock this same
                    // thread holds can never be satisfied. CPython would hang;
                    // reporting the deadlock is strictly more useful.
                    return Err(
                        "RuntimeError: deadlock: acquiring a lock already held by this thread"
                            .to_string(),
                    );
                }
                with_host(|h| {
                    if let Some(PyObj::Lock { count, .. }) = h.get_mut(recv) {
                        *count += 1;
                    }
                });
                Ok(Value::Bool(true))
            }
            "release" | "__exit__" => {
                with_host(|h| {
                    if let Some(PyObj::Lock { count, .. }) = h.get_mut(recv) {
                        if *count > 0 {
                            *count -= 1;
                        }
                    }
                });
                Ok(Value::Undef)
            }
            "locked" => Ok(Value::Bool(with_host(
                |h| matches!(h.get(recv), Some(PyObj::Lock { count, .. }) if *count > 0),
            ))),
            // A no-op: there is no fork to re-initialize after.
            "_at_fork_reinit" => Ok(Value::Undef),
            "_is_owned" => Ok(Value::Bool(
                reentrant
                    && with_host(
                        |h| matches!(h.get(recv), Some(PyObj::Lock { count, .. }) if *count > 0),
                    ),
            )),
            _ => Err(type_error(&format!("'lock' object has no method '{name}'"))),
        },
        Some(PyObj::Redirect { stderr, target, .. }) => match name {
            "__enter__" => {
                with_host(|h| {
                    let cur = if stderr {
                        h.stderr_target.clone()
                    } else {
                        h.stdout_target.clone()
                    };
                    if let Some(PyObj::Redirect { saved, .. }) = h.get_mut(recv) {
                        *saved = cur;
                    }
                    let new = Some(target.clone());
                    if stderr {
                        h.stderr_target = new;
                    } else {
                        h.stdout_target = new;
                    }
                });
                Ok(target)
            }
            "__exit__" => {
                with_host(|h| {
                    let saved = match h.get(recv) {
                        Some(PyObj::Redirect { saved, .. }) => saved.clone(),
                        _ => None,
                    };
                    if stderr {
                        h.stderr_target = saved;
                    } else {
                        h.stdout_target = saved;
                    }
                });
                Ok(Value::Bool(false))
            }
            _ => Err(format!(
                "AttributeError: 'redirect' object has no attribute '{name}'"
            )),
        },
        Some(PyObj::Instance(inst)) => {
            // instance attribute that is callable?
            if let Some(v) = with_host(|h| h.inst_attr(&inst.dict, name)) {
                return invoke(&v, args, kwargs);
            }
            let class = inst.class.clone();
            // `obj.__class__(...)` compiles as a method call, but `__class__` is a
            // data attribute (the class); resolve it and construct (enum's
            // `self.__class__(value)` in `Flag.__or__`, etc.).
            if name == "__class__" {
                let cls = with_host(|h| h.alloc(PyObj::Class(class.clone())));
                return invoke(&cls, args, kwargs);
            }
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
                    // An unbound slot wrapper stored as a class method (enum sets
                    // `__str__ = member_type.__str__`) binds to the instance: the
                    // wrapper takes `self` as its first argument.
                    Some(PyObj::Descriptor {
                        kind: DescKind::WrapperDescriptor,
                        ..
                    }) => {
                        let mut a = Vec::with_capacity(args.len() + 1);
                        a.push(recv.clone());
                        a.extend(args);
                        return invoke(&f, a, kwargs);
                    }
                    _ => return invoke(&f, args, kwargs),
                }
            }
            // `__init__` inherited with no user override is `object.__init__` —
            // a no-op returning None. A builtin base (str/int/…) provides no
            // `__init__`, so the hybrid dispatch below would wrongly report a
            // missing attribute (enum member creation calls `member.__init__(*args)`).
            if name == "__init__" {
                return Ok(Value::Undef);
            }
            // Builtin-subclass instance: inherited methods / protocol dunders
            // delegate to the native payload (`stack.append(x)`, `u.upper()`,
            // `d.keys()`, and the `__len__`/`__getitem__`/… protocol).
            if !matches!(inst.payload, Value::Undef) {
                if let Some(base) = with_host(|h| h.builtin_base_of(&class)) {
                    return base_dispatch(recv, &inst.payload, base, name, args, kwargs);
                }
            }
            // The `object` slots the class inherited and did not override.
            // Before `__getattr__` deliberately: in CPython the type lookup
            // finds `object.__eq__` and succeeds, so `__getattr__` is never
            // consulted for one of these.
            if crate::builtins::OBJECT_METHOD_DUNDERS.contains(&name) {
                if let Some(r) = crate::builtins::instance_object_dunder(recv, &class, name, &args)
                {
                    return r;
                }
            }
            // Last resort, as in CPython: `__getattr__` supplies attributes the
            // class does not define, and a METHOD CALL has to consult it too —
            // not just a plain attribute read. `unittest`'s `_WritelnDecorator`
            // is nothing but a `__getattr__` that forwards to a wrapped stream,
            // so `self.write(...)` inside it went straight to AttributeError.
            if with_host(|h| h.class_has(&class, "__getattr__")) {
                let key = with_host(|h| h.new_str(name.to_string()));
                let attr = call_method(recv, "__getattr__", vec![key], vec![])?;
                return invoke(&attr, args, kwargs);
            }
            Err(format!(
                "AttributeError: '{class}' object has no attribute '{name}'"
            ))
        }
        Some(PyObj::Class(cname)) => {
            // Native type-object methods (`cls.__subclasses__()`) don't live in the
            // class namespace; dispatch them to the type-method handler.
            if name == "__subclasses__" {
                let cls = with_host(|h| h.alloc(PyObj::Class(cname.clone())));
                return crate::builtins::call_type_method(&cls, name, args, kwargs);
            }
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
            // `Cls.__new__(subcls, *args)` with no class-defined `__new__` falls
            // back to `object.__new__` — a bare instance of the class argument.
            // `_pydatetime`'s `tzinfo.__new__(cls)` relies on this.
            if name == "__new__" {
                return crate::builtins::call_builtin_function("object.__new__", args, kwargs);
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
            // `object`'s own class-level defaults, reached when nothing in the MRO
            // overrides them. `__subclasshook__` returning NotImplemented is what
            // tells `ABCMeta.__subclasscheck__` to fall through to the ordinary
            // MRO/registry check — `numbers.Complex.register(complex)` is the first
            // thing `decimal` does, and it goes straight through this hook.
            if name == "__subclasshook__" {
                return Ok(with_host(|h| h.alloc(PyObj::NotImplemented)));
            }
            if name == "__init_subclass__" {
                return Ok(Value::Undef);
            }
            // The `type` surface a class inherits — `A.mro()`,
            // `A.__instancecheck__(x)`, `A.__call__()`. LAST, after both the
            // class namespace and the metaclass: a user metaclass that defines
            // `__instancecheck__`/`__subclasscheck__` must win, or the default
            // implementation calls `isinstance`, which calls the hook, which
            // lands back here.
            if crate::builtins::TYPE_OBJECT_METHODS.contains(&name) {
                let cls = with_host(|h| h.alloc(PyObj::Class(cname.clone())));
                return crate::builtins::call_type_method(&cls, name, args, kwargs);
            }
            Err(format!(
                "AttributeError: type object '{cname}' has no attribute '{name}'"
            ))
        }
        Some(PyObj::Module { slot, name: mname }) => {
            match with_host(|h| h.module_globals[slot].get(name).cloned()) {
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
            }
        }
        Some(PyObj::Super { owner, instance }) => {
            let inst_class = match with_host(|h| h.get(&instance).cloned()) {
                Some(PyObj::Instance(i)) => i.class,
                _ => owner.clone(),
            };
            match with_host(|h| super_lookup(h, &owner, &inst_class, name)) {
                Some((f, found)) => {
                    let fobj = with_host(|h| h.get(&f).cloned());
                    if let Some(PyObj::Func(fv)) = fobj {
                        // `__new__` is an implicit staticmethod — the class is passed
                        // explicitly in `args`, so `super().__new__(cls, …)` must NOT
                        // also bind the instance as a receiver.
                        let recv = if name == "__new__" {
                            None
                        } else {
                            Some(instance)
                        };
                        return run_user_func(&fv, recv, Some(found), args, kwargs);
                    }
                    // A native class method resolved through super (e.g.
                    // `_random.Random.seed`) still needs the instance.
                    if found == "_random.Random" {
                        if let Value::Obj(id) = &instance {
                            return crate::builtins::random_method(*id, name, &args);
                        }
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
                        // `type.__new__(mcls, name, bases, ns, **kwds)` builds the
                        // class. The `kwds` the metaclass `__new__` forwarded here
                        // (past its own consumed keywords) go to `__init_subclass__`.
                        "__new__" if args.len() >= 4 => {
                            let mcls_name = with_host(|h| match h.get(&args[0]) {
                                Some(PyObj::Class(n)) => n.clone(),
                                _ => owner.clone(),
                            });
                            crate::builtins::type_new_meta(
                                &args[1], &args[2], &args[3], &mcls_name, kwargs,
                            )
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
                // `super().__setattr__/__delattr__/__getattribute__(...)` bottom out
                // at `object`'s implementations — the plain instance-dict ops
                // (typing's `_GenericAlias.__setattr__` calls `super().__setattr__`).
                None if name == "__setattr__" => {
                    let mut it = args.into_iter();
                    let attr = it.next().unwrap_or(Value::Undef);
                    let val = it.next().unwrap_or(Value::Undef);
                    let attr_s = with_host(|h| h.as_str(&attr)).unwrap_or_default();
                    with_host(|h| h.set_attr(&instance, &attr_s, val)).map(|_| Value::Undef)
                }
                None if name == "__delattr__" => {
                    let attr = args.into_iter().next().unwrap_or(Value::Undef);
                    let attr_s = with_host(|h| h.as_str(&attr)).unwrap_or_default();
                    with_host(|h| h.del_attr(&instance, &attr_s)).map(|_| Value::Undef)
                }
                None if name == "__getattribute__" => {
                    let attr = args.into_iter().next().unwrap_or(Value::Undef);
                    let attr_s = with_host(|h| h.as_str(&attr)).unwrap_or_default();
                    with_host(|h| h.get_attr(&instance, &attr_s))
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
                Some(PyObj::Class(cname)) => {
                    Ok(with_host(|h| h.new_instance(cname, NameMap::default())))
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
        // `<ExcClass>.__init__(self, *args)` — `BaseException.__init__` sets
        // `self.args`. tomli's `TOMLDecodeError` calls `ValueError.__init__(self,
        // msg)` explicitly (rather than via `super()`).
        Some(PyObj::Builtin(bname))
            if name == "__init__" && crate::builtins::is_exception_class(&bname) =>
        {
            let mut it = args.into_iter();
            if let Some(inst) = it.next() {
                let rest: Vec<Value> = it.collect();
                with_host(|h| {
                    let t = h.alloc(PyObj::Tuple(rest));
                    let _ = h.set_attr(&inst, "args", t);
                });
            }
            Ok(Value::Undef)
        }
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
    // Clone the class NAME, never the object: `get(..).cloned()` here deep-copied
    // whatever the receiver was, so every `a[i]` on a list copied the entire list
    // before discarding it — O(n) per subscript, quadratic over a loop.
    let cname = with_host(|h| match h.get(cls) {
        Some(PyObj::Class(n)) => Some(n.clone()),
        _ => None,
    })?;
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
            let mut attrs = NameMap::default();
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
        // Default `type.__new__(meta, name, bases, ns)`; the class keywords reach
        // `__init_subclass__` (the metaclass has no `__new__` to consume any).
        crate::builtins::type_new_meta(&args[0], &args[1], &args[2], meta, kwargs.clone())?
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
/// Maximum Python call depth before a `RecursionError` (CPython's default
/// `sys.getrecursionlimit()`). Sized to stay well within the interpreter
/// thread's 512 MiB stack (see `main`).
const RECURSION_LIMIT: usize = 1000;

pub fn run_user_func(
    fv: &FuncVal,
    self_opt: Option<Value>,
    owner_opt: Option<String>,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // Clone the function's METADATA, never its bytecode. `FuncDef::clone` copies
    // the whole `Chunk` (ops, constants, names), and this runs on every single
    // Python call — `fib(27)` cloned 400k copies of a 30-op chunk it then only
    // read through. The chunk is fetched by hand below, on the two paths that
    // genuinely need to own one.
    // Two `Rc` bumps: no copy of the signature, the local set, or the body.
    let (def, locals_rc, frame_name) = with_host(|h| {
        (
            h.funcs[fv.def_id].clone(),
            h.func_locals[fv.def_id].clone(),
            h.func_names[fv.def_id].clone(),
        )
    });
    let def_id = fv.def_id;
    let self_val = self_opt.or_else(|| fv.bound.clone());
    let mut pos = args;
    if let Some(s) = &self_val {
        pos.insert(0, s.clone());
    }
    // `__new__` is an implicit staticmethod (no bound receiver), but zero-arg
    // `super()` inside it resolves against the class passed as the first argument.
    // Expose that as the frame's `self` without prepending it to the parameters.
    let frame_self = self_val.clone().or_else(|| {
        if def.name == "__new__" {
            pos.first().cloned()
        } else {
            None
        }
    });
    let env = new_env(fv.env.clone());
    bind_params(&env, &def, &fv.defaults, &fv.kwonly_defaults, pos, kwargs)?;
    let owner = owner_opt.or_else(|| fv.owner.clone());
    // `async def`: calling it returns a coroutine object (or, if the body
    // contains `yield`, an async generator); the body runs only when the event
    // loop drives it (CPython does not execute it eagerly). The generator/
    // coroutine object captures the module to restore on each resume, so swap to
    // the callee's module while it is built (`make_gen_kind` reads `cur_module`),
    // then restore the caller's before handing the object back.
    if def.is_async || def.is_generator {
        let saved_mod = with_host(|h| h.swap_module(fv.module));
        // These park the body on a coroutine stack, so they need an owned chunk.
        let body = def.chunk.clone();
        let obj = if def.is_async {
            if def.is_generator {
                make_async_generator(
                    body,
                    env,
                    self_val,
                    owner,
                    def.name.clone(),
                    def.locals.clone(),
                )
            } else {
                make_coroutine(
                    body,
                    env,
                    self_val,
                    owner,
                    def.name.clone(),
                    def.locals.clone(),
                )
            }
        } else {
            make_generator(
                body,
                env,
                self_val,
                owner,
                def.name.clone(),
                def.locals.clone(),
            )
        };
        with_host(|h| h.swap_module(saved_mod));
        return Ok(obj);
    }
    // Recursion guard: raise a catchable `RecursionError` before the deep Rust
    // call chain per Python frame exhausts the (large but finite) native stack.
    // The limit matches CPython's default (1000); the interpreter runs on a
    // 512 MiB-stack thread, which comfortably holds that many frames.
    if with_host(|h| h.frames.len() >= RECURSION_LIMIT) {
        return Err("RecursionError: maximum recursion depth exceeded".to_string());
    }
    let saved_mod = with_host(|h| {
        let saved = h.swap_module(fv.module);
        h.frames.push(Frame {
            env,
            globals_decl: HashSet::new(),
            nonlocals_decl: HashSet::new(),
            locals_set: locals_rc,
            is_class_body: false,
            self_obj: frame_self,
            owner,
            name: frame_name,
            line: 0,
            span: Span::NONE,
        });
        saved
    });
    let r = run_chunk_cached(def_id as u64, || def.chunk.clone());
    let sig = with_host(|h| {
        if r.is_err() {
            h.push_tb_frame();
        }
        h.frames.pop();
        h.swap_module(saved_mod);
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
    // Argument-count errors name the callable by its `__qualname__` (CPython:
    // `outer.<locals>.f() takes …`), falling back to the bare name.
    let fname = if def.qualname.is_empty() {
        def.name.as_str()
    } else {
        def.qualname.as_str()
    };
    // A named `*args` (`Some(non-empty)`) soaks up extra positionals; a bare `*`
    // (`Some("")`, keyword-only marker) does not — extras are an error there.
    let has_vararg = def.star.as_deref().is_some_and(|s| !s.is_empty());
    let mut vars: NameMap = NameMap::default();
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
                    fname, k
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
                fname,
                bad_posonly.join(", ")
            )));
        }
        return Err(type_error(&format!(
            "{}() got an unexpected keyword argument '{}'",
            fname, leftover[0].0
        )));
    }

    // 4. Too many positionals (no `*args` to catch them).
    if npos > np && !has_vararg {
        return Err(type_error(&format!(
            "{}() {}",
            fname,
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
            fname,
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
            fname,
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
    /// A `co_*` attribute of a function's code object, derived from its
    /// `FuncDef`. Covers the surface the faithful stdlib reads: `types` (only
    /// needs the type + `co_flags`), `inspect.signature`
    /// (argcounts/varnames/flags), `functools`, `dataclasses`.
    fn code_attr(&mut self, def_id: usize, name: &str) -> Result<Value, String> {
        // CPython 3.14 co_flags bits (`Include/cpython/code.h`), named exactly as
        // `dis.COMPILER_FLAG_NAMES` lists them.
        //
        // `CO_NOFREE` (0x0040) is deliberately absent: 3.14's compiler never sets
        // it. `dis.COMPILER_FLAG_NAMES` still *names* the bit, so it is easy to
        // mistake for live — but `def f(): pass` reports `co_flags == 3`, not 67.
        const CO_OPTIMIZED: i64 = 0x0001;
        const CO_NEWLOCALS: i64 = 0x0002;
        const CO_VARARGS: i64 = 0x0004;
        const CO_VARKEYWORDS: i64 = 0x0008;
        /// Defined inside another function's scope (at any depth).
        const CO_NESTED: i64 = 0x0010;
        const CO_GENERATOR: i64 = 0x0020;
        const CO_COROUTINE: i64 = 0x0080;
        const CO_ASYNC_GENERATOR: i64 = 0x0200;
        /// The body opens with a string literal.
        const CO_HAS_DOCSTRING: i64 = 0x0400_0000;
        /// Defined directly in a class body (3.14 only).
        const CO_METHOD: i64 = 0x0800_0000;
        // Pull every field under one short immutable borrow so the alloc/new_str
        // below (which need `&mut self`) don't conflict with it.
        let (co_name, co_qualname, params, posonly, kwonly, star, kwargs, locals, flags) = {
            let d = &self.funcs[def_id];
            let q = if d.qualname.is_empty() {
                d.name.clone()
            } else {
                d.qualname.clone()
            };
            let mut f = CO_OPTIMIZED | CO_NEWLOCALS;
            // The enclosing scope is whatever `__qualname__` names before the final
            // component: `outer.<locals>.inner` → `outer.<locals>` (a function),
            // `C.m` → `C` (a class body), `f` → `` (the module). A `<locals>`
            // anywhere in that path means some enclosing scope is a function
            // (CO_NESTED); a final component that is NOT `<locals>` means the
            // *immediate* scope is a class body (CO_METHOD). `outer.<locals>.D.m`
            // is both, exactly as CPython reports it.
            if let Some((scope, _)) = q.rsplit_once('.') {
                if scope.split('.').any(|c| c == "<locals>") {
                    f |= CO_NESTED;
                }
                if !scope.ends_with("<locals>") {
                    f |= CO_METHOD;
                }
            }
            if d.doc.is_some() {
                f |= CO_HAS_DOCSTRING;
            }
            if d.star.is_some() {
                f |= CO_VARARGS;
            }
            if d.kwargs.is_some() {
                f |= CO_VARKEYWORDS;
            }
            if d.is_generator && !d.is_async {
                f |= CO_GENERATOR;
            }
            if d.is_async && !d.is_generator {
                f |= CO_COROUTINE;
            }
            if d.is_async && d.is_generator {
                f |= CO_ASYNC_GENERATOR;
            }
            (
                d.name.clone(),
                q,
                d.params.clone(),
                d.posonly,
                d.kwonly.clone(),
                d.star.clone(),
                d.kwargs.clone(),
                d.locals.clone(),
                f,
            )
        };
        // co_varnames: parameters (positional then keyword-only), then
        // `*args`/`**kwargs`, then any remaining body locals.
        let varnames = || -> Vec<String> {
            let mut names = params.clone();
            names.extend(kwonly.iter().cloned());
            if let Some(s) = &star {
                names.push(s.clone());
            }
            if let Some(k) = &kwargs {
                names.push(k.clone());
            }
            for l in &locals {
                if !names.contains(l) {
                    names.push(l.clone());
                }
            }
            names
        };
        match name {
            "co_name" => Ok(self.new_str(co_name)),
            "co_qualname" => Ok(self.new_str(co_qualname)),
            "co_argcount" => Ok(Value::Int(params.len() as i64)),
            "co_posonlyargcount" => Ok(Value::Int(posonly as i64)),
            "co_kwonlyargcount" => Ok(Value::Int(kwonly.len() as i64)),
            "co_flags" => Ok(Value::Int(flags)),
            "co_varnames" => {
                let vals: Vec<Value> = varnames().into_iter().map(|n| self.new_str(n)).collect();
                Ok(self.new_tuple(vals))
            }
            "co_nlocals" => Ok(Value::Int(varnames().len() as i64)),
            "co_filename" => {
                let f = self.tb_filename.clone();
                Ok(self.new_str(if f.is_empty() {
                    "<string>".to_string()
                } else {
                    f
                }))
            }
            "co_firstlineno" => Ok(Value::Int(1)),
            "co_freevars" => {
                let fv = self.funcs[def_id].freevars.clone();
                let vals: Vec<Value> = fv.into_iter().map(|n| self.new_str(n)).collect();
                Ok(self.new_tuple(vals))
            }
            // pythonrs does not expose the constant/name/cell tables yet;
            // report them empty rather than fabricate contents.
            "co_names" | "co_consts" | "co_cellvars" => Ok(self.new_tuple(vec![])),
            "co_stacksize" => Ok(Value::Int(0)),
            _ => Err(format!(
                "AttributeError: 'code' object has no attribute '{name}'"
            )),
        }
    }

    fn func_dunder(
        &mut self,
        name: &str,
        def_id: usize,
        defaults: &[Value],
        kwonly_defaults: &[Value],
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
            "__doc__" => match self.funcs[def_id].doc.clone() {
                Some(d) => Ok(self.new_str(d)),
                None => Ok(Value::Undef),
            },
            "__code__" => Ok(self.alloc(PyObj::Code { def_id })),
            // `__kwdefaults__`: the defaults of KEYWORD-ONLY parameters, as a
            // dict, or `None` when there are none. `inspect.signature` reads it
            // for every function it describes.
            "__kwdefaults__" => {
                // `kwonly_defaults` holds one value per keyword-only parameter
                // that HAS a default, in `kwonly` order, so the two are zipped
                // through `kwonly_required` rather than positionally.
                let d = &self.funcs[def_id];
                let names: Vec<String> = d.kwonly.clone();
                let required: Vec<bool> = d.kwonly_required.clone();
                let mut map: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                let mut next = 0usize;
                for (i, n) in names.iter().enumerate() {
                    if required.get(i).copied().unwrap_or(true) {
                        continue;
                    }
                    let Some(v) = kwonly_defaults.get(next).cloned() else {
                        break;
                    };
                    next += 1;
                    let kv = self.new_str(n.clone());
                    map.insert(PKey::Str(n.clone()), (kv, v));
                }
                if map.is_empty() {
                    return Ok(Value::Undef);
                }
                Ok(self.new_dict(map))
            }
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
        // `BaseExceptionGroup.__str__` counts its members rather than rendering
        // the `(message, exceptions)` argument tuple.
        if crate::excgroup::class_is_group(self, class) && args.len() == 2 {
            if let Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) = self.get(&args[1]) {
                let n = l.len();
                let plural = if n > 1 { "s" } else { "" };
                return format!("{} ({n} sub-exception{plural})", self.str_of(&args[0]));
            }
        }
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
fn run_class_body(name: &str, body_func: &Value) -> Result<NameMap, String> {
    let fv = match with_host(|h| h.get(body_func).cloned()) {
        Some(PyObj::Func(fv)) => fv,
        _ => return Err(type_error("internal: class body is not a function")),
    };
    let def = with_host(|h| h.funcs[fv.def_id].clone());
    let env = new_env(fv.env.clone());
    let saved_mod = with_host(|h| {
        let saved = h.swap_module(fv.module);
        h.frames.push(Frame {
            env: env.clone(),
            globals_decl: HashSet::new(),
            nonlocals_decl: HashSet::new(),
            // A class body resolves names dynamically (LOAD_NAME), so an unbound
            // read is a `NameError`, not `UnboundLocalError` — leave this empty.
            locals_set: Rc::new(HashSet::new()),
            is_class_body: true,
            self_obj: None,
            owner: Some(name.to_string()),
            name: Rc::from(name),
            line: 0,
            span: Span::NONE,
        });
        saved
    });
    let r = run_chunk_on(def.chunk.clone());
    with_host(|h| {
        if r.is_err() {
            h.push_tb_frame();
        }
        h.frames.pop();
        h.swap_module(saved_mod);
        h.signal.take();
    });
    r?;
    let mut vars = env.borrow().vars.clone();
    // CPython puts the class body's docstring in the namespace as `__doc__`, and
    // `None` there when the body has none — so `Cls.__doc__` always resolves.
    // Without it `contextlib.contextmanager` dies on its own
    // `_GeneratorContextManager.__doc__`, taking every `@contextmanager` with it.
    // …unless the body slots `__doc__` itself. CPython's compiler emits the
    // `__doc__` store ONLY for a real docstring, so `__slots__ = ('__doc__',)`
    // beside no docstring is legal and installs a slot descriptor. Seeding the
    // default unconditionally made that namespace look like it carried a
    // `__doc__` class variable, and `typing._SpecialForm` — which slots exactly
    // that — died with `'__doc__' in __slots__ conflicts with class variable`,
    // taking the whole `typing` module with it.
    if def.doc.is_some() || !slots_mention(vars.get("__slots__"), "__doc__") {
        vars.entry("__doc__".to_string()).or_insert_with(|| {
            with_host(|h| match &def.doc {
                Some(d) => h.new_str(d.clone()),
                None => Value::Undef,
            })
        });
    }
    // `__module__` is the `__name__` of the module the body was DEFINED in, which
    // is `fv.module` — not whatever module happens to be running when a metaclass
    // finally registers the class. `class Month(IntEnum)` in calendar.py is
    // registered from inside `EnumType.__new__`, so reading the live scope
    // labelled every enum `enum`, and `global_enum` then published `JANUARY` into
    // the `enum` module instead of `calendar`.
    vars.entry("__module__".to_string()).or_insert_with(|| {
        with_host(|h| {
            h.module_globals[fv.module]
                .get("__name__")
                .cloned()
                .unwrap_or_else(|| h.new_str("__main__".to_string()))
        })
    });
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

/// Fire `__set_name__(owner, attr)` on each namespace value whose type defines
/// it, in definition order — the descriptor-naming step CPython runs inside
/// `type.__new__`. Enum members are created here (each `_proto_member`'s
/// `__set_name__` builds the real member and records it in `_member_map_`).
pub fn fire_set_name(class_name: &str, ns: &NameMap) -> Result<(), String> {
    for (attr_name, val) in ns {
        let fires = with_host(|h| match h.get(val) {
            Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__set_name__").is_some(),
            _ => false,
        });
        if fires {
            let owner = with_host(|h| h.alloc(PyObj::Class(class_name.to_string())));
            let nm = with_host(|h| h.new_str(attr_name.clone()));
            call_method(val, "__set_name__", vec![owner, nm], vec![])?;
        }
    }
    Ok(())
}

/// CPython's `__slots__` validation (`Objects/typeobject.c::type_new_slots_impl`),
/// run on the class-body namespace before the class is created. Two passes, in
/// CPython's order:
///
/// 1. every slot must be a string (`TypeError: __slots__ items must be strings,
///    not '<type>'`) and a valid identifier (`TypeError: __slots__ must be
///    identifiers`); `__dict__` and `__weakref__` may each appear at most once
///    (`TypeError: __dict__ slot disallowed: we already got one`).
/// 2. a remaining slot name that is also bound in the class body collides with
///    the descriptor the slot would install:
///    `ValueError: 'x' in __slots__ conflicts with class variable`. The three
///    names class creation itself inserts (`__qualname__`, `__classcell__`,
///    `__classdictcell__`) are exempt.
///
/// Each slot name is mangled against the class name before the pass-2 lookup,
/// so `__slots__ = ("__x",)` beside a `_C__x = 1` class variable is the
/// collision CPython reports it as.
/// True if a class body's `__slots__` value names `want`. Deliberately silent
/// about malformed values — [`check_slots`] is where those are diagnosed; this
/// only answers whether a slot descriptor is about to claim the name.
fn slots_mention(slots: Option<&Value>, want: &str) -> bool {
    let Some(slots) = slots else { return false };
    with_host(|h| match h.get(slots) {
        Some(PyObj::Str(s)) => s == want,
        Some(PyObj::Tuple(xs)) | Some(PyObj::List(xs)) => {
            let xs = xs.clone();
            xs.iter()
                .any(|x| matches!(h.get(x), Some(PyObj::Str(s)) if s == want))
        }
        _ => false,
    })
}

fn check_slots(class: &str, ns: &NameMap) -> Result<(), String> {
    let slots = match ns.get("__slots__") {
        Some(v) => v.clone(),
        None => return Ok(()),
    };
    // `__slots__ = 'x'` names one slot; anything else is taken as a sequence
    // (CPython `PySequence_Tuple`), so a tuple/list/set/dict all work.
    let items: Vec<Value> = match with_host(|h| match h.get(&slots) {
        Some(PyObj::Str(s)) => Some(s.clone()),
        _ => None,
    }) {
        Some(s) => vec![with_host(|h| h.new_str(s))],
        None => iter_vec(&slots)?,
    };
    let mut names: Vec<String> = Vec::with_capacity(items.len());
    let (mut add_dict, mut add_weak) = (false, false);
    for it in &items {
        let name = with_host(|h| match h.get(it) {
            Some(PyObj::Str(s)) => Ok(s.clone()),
            _ => Err(type_error(&format!(
                "__slots__ items must be strings, not '{}'",
                h.type_name(it)
            ))),
        })?;
        if !crate::builtins::is_identifier(&name) {
            return Err(type_error("__slots__ must be identifiers"));
        }
        match name.as_str() {
            "__dict__" if add_dict => {
                return Err(type_error("__dict__ slot disallowed: we already got one"))
            }
            "__dict__" => add_dict = true,
            "__weakref__" if add_weak => {
                return Err(type_error(
                    "__weakref__ slot disallowed: we already got one",
                ))
            }
            "__weakref__" => add_weak = true,
            // Mangled here, not at the identifier check above: `__slots__ must
            // be identifiers` is judged on the name as written.
            _ => names.push(crate::mangle::mangle(class, &name).unwrap_or(name)),
        }
    }
    for name in names {
        if matches!(
            name.as_str(),
            "__qualname__" | "__classcell__" | "__classdictcell__"
        ) {
            continue;
        }
        if ns.contains_key(&name) {
            return Err(format!(
                "ValueError: '{name}' in __slots__ conflicts with class variable"
            ));
        }
    }
    Ok(())
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
    let ns: NameMap = run_class_body(name, body_func)?;
    check_slots(name, &ns)?;
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
        Some(m) => metaclass_create(m, name, &bases, &ns, &class_kwargs)?,
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
    // Descriptor naming (`__set_name__`) and PEP 487 (`__init_subclass__`) run
    // inside `type.__new__`. For the metaclass path that is `type_new_meta`,
    // reached via the metaclass's `super().__new__(...)` — which sees the
    // possibly-rewritten classdict (enum's members) and the leftover keywords
    // after the metaclass `__new__` consumed its own (e.g. enum's `boundary=`).
    // Firing here again would misname descriptors and pass consumed keywords.
    if effective_meta.is_none() {
        fire_set_name(name, &ns)?;
        fire_init_subclass(name, class_kwargs)?;
    }
    Ok(cls)
}

/// PEP 487: fire the nearest ancestor's `__init_subclass__` (an implicit
/// classmethod) with the leftover class-header keywords, resolved along the MRO
/// strictly after the new class. Extra keywords when only the default
/// `object.__init_subclass__` remains are an error, matching CPython.
pub fn fire_init_subclass(
    class_name: &str,
    class_kwargs: Vec<(String, Value)>,
) -> Result<(), String> {
    let hook = with_host(|h| {
        h.mro_of(class_name).into_iter().skip(1).find_map(|c| {
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
                let clsobj = with_host(|h| h.alloc(PyObj::Class(class_name.to_string())));
                run_user_func(&fv, Some(clsobj), Some(owner), vec![], class_kwargs)?;
            }
        }
        None if !class_kwargs.is_empty() => {
            return Err(type_error(&format!(
                "{class_name}.__init_subclass__() takes no keyword arguments"
            )));
        }
        None => {}
    }
    Ok(())
}

/// Construct a class through its metaclass: `M(name, (bases...), {ns...})`. This
/// runs `M`'s `__call__` (or the default `type.__call__` → `__new__`/`__init__`),
/// exactly like any `M(...)` call, and returns the new class object.
fn metaclass_create(
    meta: &str,
    name: &str,
    bases: &[String],
    ns: &NameMap,
    class_kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let name_v = with_host(|h| h.new_str(name.to_string()));
    let base_vals: Vec<Value> = with_host(|h| {
        // A builtin base (`Generic`, `int`, `Exception`) must reach the metaclass
        // as its Builtin object, not a synthetic Class, so identity checks in the
        // metaclass (`bases == (Generic,)` in typing's `_ProtocolMeta`) hold.
        bases
            .iter()
            .map(|b| h.class_or_builtin_type(b.clone()))
            .collect()
    });
    let bases_v = with_host(|h| h.new_tuple(base_vals));
    let meta_v = with_host(|h| h.alloc(PyObj::Class(meta.to_string())));
    // PEP 3115 `__prepare__`: if the metaclass provides a custom namespace (e.g.
    // enum's `_EnumDict`), build the class dict THROUGH it — replaying each body
    // assignment via its `__setitem__` so its bookkeeping (member tracking) runs.
    let has_prepare = with_host(|h| h.class_lookup(meta, "__prepare__").is_some());
    let ns_v = if has_prepare {
        let prepared = call_method(
            &meta_v,
            "__prepare__",
            vec![name_v.clone(), bases_v.clone()],
            class_kwargs.to_vec(),
        )?;
        for (k, v) in ns {
            let kv = with_host(|h| h.new_str(k.clone()));
            call_method(&prepared, "__setitem__", vec![kv, v.clone()], vec![])?;
        }
        prepared
    } else {
        let ns_map: IndexMap<PKey, (Value, Value)> = with_host(|h| {
            ns.iter()
                .map(|(k, v)| {
                    let kv = h.new_str(k.clone());
                    (PKey::Str(k.clone()), (kv, v.clone()))
                })
                .collect()
        });
        with_host(|h| h.new_dict(ns_map))
    };
    invoke(&meta_v, vec![name_v, bases_v, ns_v], class_kwargs.to_vec())
}

/// The most-derived metaclass inherited from `bases` (CPython's rule for a class
/// with no explicit `metaclass=`): the metaclass that is a subclass of every
/// base's metaclass. `"type"` when no base carries a user metaclass.
pub fn default_metaclass(h: &PyHost, bases: &[String]) -> String {
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
                let mut attrs = NameMap::default();
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
        // A `SystemExit` raised on the CPython side of the bridge — `argparse`
        // ending the program from `--help` or a usage error, `unittest.main()`,
        // `runpy` — never becomes a `PyObj::Exception`: it crosses as the error
        // string plus the `foreign_exc` record. Without this it was reported as an
        // ordinary uncaught exception, so `--help` exited 1 instead of 0 and a
        // usage error exited 1 instead of 2, both with a traceback CPython does
        // not print. Matched against the error being classified (rather than on
        // `foreign_exc` alone) so a stale record from an earlier, caught bridge
        // call cannot claim an unrelated failure.
        if let Some(fe) = &h.foreign_exc {
            if fe.line == err && (err == "SystemExit" || err.starts_with("SystemExit:")) {
                let args = fe.args.clone();
                return system_exit_outcome(h, &args);
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

/// The inputs CPython's `_compute_suggestion_error` needs, captured at the point
/// the error is raised (see [`PyHost::suggest`]).
pub enum SuggestCtx {
    /// A `NameError`: the name that missed, the scope chain and module it missed
    /// in, and the frame's `self` — CPython suggests `self.x` when the instance
    /// carries the name the code used bare.
    Name {
        wrong: String,
        env: Env,
        /// Names local to the scope that live in FRAME SLOTS rather than in
        /// `env` (see `FuncDef::locals`) — invisible to the env walk.
        slotted: Vec<String>,
        module: usize,
        self_obj: Option<Value>,
    },
    /// An `AttributeError`: the receiver whose `dir()` supplies the candidates,
    /// plus the frame's `self` (an underscored candidate stays visible when the
    /// code was reading an attribute of its own instance).
    Attr {
        wrong: String,
        recv: Value,
        self_obj: Option<Value>,
    },
}

/// `traceback.TracebackException`'s `max_group_width` / `max_group_depth`: how
/// many members of one exception group are listed, and how deep the nesting is
/// rendered, before the output is elided.
const MAX_GROUP_WIDTH: usize = 15;
const MAX_GROUP_DEPTH: usize = 10;

/// `traceback._ExceptionPrintContext` — the indentation and margin state that
/// draws an exception group's `| ` gutter as the renderer descends into members.
#[derive(Default)]
struct GroupCtx {
    /// Nesting depth inside exception groups; 0 outside any group.
    depth: usize,
    /// Whether the current member still owes a `+------` closing frame. A nested
    /// group draws it on its own innermost level and clears the flag.
    need_close: bool,
}

impl GroupCtx {
    fn indent(&self) -> String {
        " ".repeat(2 * self.depth)
    }

    /// Write `text` with the current gutter prefixed to EVERY line — including
    /// blank ones, as `textwrap.indent(..., lambda line: True)` does.
    fn emit(&self, out: &mut String, text: &str, margin: char) {
        let mut prefix = self.indent();
        if self.depth > 0 {
            prefix.push(margin);
            prefix.push(' ');
        }
        for line in text.split_inclusive('\n') {
            out.push_str(&prefix);
            out.push_str(line);
        }
    }
}

impl PyHost {
    /// Render a CPython `Traceback (most recent call last):` block for an uncaught
    /// exception, ending with `err`, including any `__cause__`/`__context__` chain
    /// (oldest exception first, joined by CPython's connector lines). Frames run
    /// outermost (module) first; source lines are shown unless the program came
    /// from stdin. Caret markers are omitted (approximate for a first pass).
    pub fn render_traceback(&self, err: &str) -> String {
        // The final (uncaught) exception's frames: the module frame still on the
        // stack, then the function frames it unwound past (innermost-first),
        // reversed to outermost-first.
        let mut final_frames: Vec<(String, u32, Span)> = Vec::new();
        if let Some(f) = self.frames.first() {
            final_frames.push((f.name.to_string(), f.line, f.span));
        }
        for f in self.traceback.iter().rev() {
            final_frames.push(f.clone());
        }
        // A group built by an `except*` reconstruction owns no frame of its own:
        // it came into being after the handler in the innermost frame finished,
        // so that frame is dropped and only its callers remain.
        if matches!(&self.exc, Some(Value::Obj(id)) if self.tb_starts_empty.contains(id)) {
            final_frames.pop();
        }
        // Walk the chain backwards from the final exception. Each ancestor carries
        // the connector line that introduces the *next-newer* exception. Prefer an
        // explicit `__cause__` (`raise X from Y`, which also suppresses context);
        // otherwise an implicit `__context__`.
        const CAUSE: &str =
            "\nThe above exception was the direct cause of the following exception:\n\n";
        const CONTEXT: &str =
            "\nDuring handling of the above exception, another exception occurred:\n\n";
        let mut ancestors: Vec<(Value, &'static str)> = Vec::new();
        if let Some(final_exc) = &self.exc {
            let mut cur = final_exc.clone();
            let mut seen: HashSet<u32> = HashSet::new();
            loop {
                if let Value::Obj(id) = cur {
                    if !seen.insert(id) {
                        break;
                    }
                }
                let (cause, context) = self.exc_link(&cur);
                let suppressed =
                    matches!(&cur, Value::Obj(id) if self.suppress_context.contains(id));
                let (pred, connector) = if !matches!(cause, Value::Undef) {
                    (cause, CAUSE)
                } else if !suppressed && !matches!(context, Value::Undef) {
                    (context, CONTEXT)
                } else {
                    break;
                };
                ancestors.push((pred.clone(), connector));
                cur = pred;
            }
        }
        // Ancestors are collected newest-first; render oldest-first, each followed
        // by its connector, then the final exception's own block.
        let mut out = String::new();
        let mut ctx = GroupCtx::default();
        for (exc, connector) in ancestors.iter().rev() {
            let frames = self.frames_of(exc);
            self.render_exc_block(
                Some(exc),
                &frames,
                &self.exc_final_line(exc),
                &mut ctx,
                &mut out,
            );
            ctx.emit(&mut out, connector, '|');
        }
        // CPython's "Did you mean" hint is part of the RENDERED traceback, not of
        // the exception — `str(e)` for a NameError never carries it — so it is
        // appended here rather than baked into the error string.
        let err = crate::suggest::with_hint(err, self.suggestion_for(err));
        let err = crate::suggest::with_import_hint(err, |n| stdlib_module_names().contains(n));
        self.render_exc_block(self.exc.as_ref(), &final_frames, &err, &mut ctx, &mut out);
        out
    }

    /// The `__cause__`/`__context__` chain of `exc` (oldest first) followed by
    /// its own block. Used for a group's members, each of which can carry a
    /// chain of its own.
    fn render_exc_chain(&self, exc: &Value, ctx: &mut GroupCtx, out: &mut String) {
        const CAUSE: &str =
            "\nThe above exception was the direct cause of the following exception:\n\n";
        const CONTEXT: &str =
            "\nDuring handling of the above exception, another exception occurred:\n\n";
        let mut ancestors: Vec<(Value, &'static str)> = Vec::new();
        let mut cur = exc.clone();
        let mut seen: HashSet<u32> = HashSet::new();
        loop {
            if let Value::Obj(id) = cur {
                if !seen.insert(id) {
                    break;
                }
            }
            let (cause, context) = self.exc_link(&cur);
            let suppressed = matches!(&cur, Value::Obj(id) if self.suppress_context.contains(id));
            let (pred, connector) = if !matches!(cause, Value::Undef) {
                (cause, CAUSE)
            } else if !suppressed && !matches!(context, Value::Undef) {
                (context, CONTEXT)
            } else {
                break;
            };
            ancestors.push((pred.clone(), connector));
            cur = pred;
        }
        for (anc, connector) in ancestors.iter().rev() {
            let frames = self.frames_of(anc);
            self.render_exc_block(Some(anc), &frames, &self.exc_final_line(anc), ctx, out);
            ctx.emit(out, connector, '|');
        }
        let frames = self.frames_of(exc);
        self.render_exc_block(Some(exc), &frames, &self.exc_final_line(exc), ctx, out);
    }

    /// The traceback frames captured for an already-caught exception.
    fn frames_of(&self, exc: &Value) -> Vec<(String, u32, Span)> {
        match exc {
            Value::Obj(id) => self.exc_tb.get(id).cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// One exception's own block: its `Traceback …` header and frames (when it
    /// has any) then its terse line — or, for a PEP 654 exception group, the
    /// `+-+---- n ----` tree of its members. A port of `traceback.py`'s
    /// `TracebackException.format` (the `exc.exceptions is None` branch and the
    /// group branch below it), including the `_ExceptionPrintContext` margins.
    fn render_exc_block(
        &self,
        exc: Option<&Value>,
        frames: &[(String, u32, Span)],
        final_line: &str,
        ctx: &mut GroupCtx,
        out: &mut String,
    ) {
        let members = exc.and_then(|e| crate::excgroup::group_parts(self, e));
        let Some((_, members)) = members else {
            if !frames.is_empty() {
                ctx.emit(out, "Traceback (most recent call last):\n", '|');
                ctx.emit(out, &self.render_frames(frames), '|');
            }
            ctx.emit(out, &format!("{final_line}\n"), '|');
            return;
        };
        if ctx.depth > MAX_GROUP_DEPTH {
            ctx.emit(
                out,
                &format!("... (max_group_depth is {MAX_GROUP_DEPTH})\n"),
                '|',
            );
            return;
        }
        let is_toplevel = ctx.depth == 0;
        if is_toplevel {
            ctx.depth = 1;
        }
        if !frames.is_empty() {
            ctx.emit(
                out,
                "Exception Group Traceback (most recent call last):\n",
                if is_toplevel { '+' } else { '|' },
            );
            ctx.emit(out, &self.render_frames(frames), '|');
        }
        ctx.emit(out, &format!("{final_line}\n"), '|');
        // Only the first `MAX_GROUP_WIDTH` members are shown; the slot after them
        // reports how many were elided.
        let total = members.len();
        let shown = total.min(MAX_GROUP_WIDTH + 1);
        ctx.need_close = false;
        for (i, member) in members.iter().enumerate().take(shown) {
            let last = i == shown - 1;
            if last {
                ctx.need_close = true;
            }
            let truncated = i >= MAX_GROUP_WIDTH;
            let title = if truncated {
                "...".to_string()
            } else {
                (i + 1).to_string()
            };
            out.push_str(&ctx.indent());
            out.push_str(if i == 0 { "+-" } else { "  " });
            out.push_str(&format!("+---------------- {title} ----------------\n"));
            ctx.depth += 1;
            if truncated {
                let remaining = total - MAX_GROUP_WIDTH;
                let plural = if remaining > 1 { "s" } else { "" };
                ctx.emit(
                    out,
                    &format!("and {remaining} more exception{plural}\n"),
                    '|',
                );
            } else {
                self.render_exc_chain(member, ctx, out);
            }
            // A nested group emits its own closing frame; only the innermost
            // level that still needs one draws it.
            if last && ctx.need_close {
                out.push_str(&ctx.indent());
                out.push_str("+------------------------------------\n");
                ctx.need_close = false;
            }
            ctx.depth -= 1;
        }
        if is_toplevel {
            ctx.depth = 0;
        }
    }

    /// The `File "…", line N, in scope` lines (plus source and carets) for
    /// `frames`, outermost-first — CPython's `StackSummary.format`.
    fn render_frames(&self, frames: &[(String, u32, Span)]) -> String {
        let mut out = String::new();
        let src_lines: Vec<&str> = self.prog_source.lines().collect();
        for (name, line, span) in frames {
            out.push_str(&format!(
                "  File \"{}\", line {}, in {}\n",
                self.tb_filename, line, name
            ));
            if self.tb_show_source && *line > 0 {
                if let Some(text) = src_lines.get((*line as usize).saturating_sub(1)) {
                    let stripped = text.trim();
                    if !stripped.is_empty() {
                        out.push_str(&format!("    {stripped}\n"));
                        if span.line == *line {
                            if let Some(carets) = caret_line(text, *span) {
                                out.push_str(&carets);
                                out.push('\n');
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The terse `ExcType: message` line for a chained exception object (a bare
    /// `ExcType` when it has no message). Empty for a non-exception value.
    fn exc_final_line(&self, exc: &Value) -> String {
        match self.get(exc) {
            Some(PyObj::Exception { class, args }) => {
                join_exc(class, &self.exc_message(class, args))
            }
            Some(PyObj::Instance(i)) if self.class_is_exception(&i.class) => {
                let a = self.exc_instance_args(&i.dict);
                join_exc(&i.class, &self.exc_message(&i.class, &a))
            }
            _ => String::new(),
        }
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
        std::mem::swap(&mut self.cur_module, &mut c.module);
        c
    }
}

/// Build the CPython-style caret line underlining `span` beneath the displayed
/// (stripped, 4-space-indented) source `text`, or `None` when CPython omits it.
///
/// Columns in `span` are character offsets into the ORIGINAL `text`; the display
/// strips leading whitespace, so offsets shift left by the indent. Rules ported
/// from CPython `traceback.py`:
/// - a `suppress` span (an `x = f(...)` / `return f(...)` call that raised) is
///   hidden;
/// - a span with a sub-anchor (`~^~` binop, `~~~^^^` subscript/call) is shown;
/// - a plain span is shown only if something precedes or follows it on the line
///   (so a span covering the whole statement — e.g. `None.foo` — is hidden);
/// - the anchor sub-range renders `^`, the rest of the span `~`; a plain span
///   renders entirely `^`.
fn caret_line(text: &str, span: Span) -> Option<String> {
    if !span.is_some() || span.suppress {
        return None;
    }
    let lead = text.chars().take_while(|c| c.is_whitespace()).count() as u32;
    let stripped_len = text.trim().chars().count() as u32;
    // Offsets relative to the stripped (displayed) line.
    let start = span.start.saturating_sub(lead);
    let end = span.end.saturating_sub(lead).min(stripped_len);
    if end <= start {
        return None;
    }
    let has_anchor = span.has_anchor();
    let a0 = span.anchor_start.saturating_sub(lead);
    let a1 = span.anchor_end.saturating_sub(lead);
    // Show/hide: anchored spans always show; a plain span shows only when it does
    // not cover the whole stripped line.
    let show = has_anchor || start > 0 || end < stripped_len;
    if !show {
        return None;
    }
    let primary = if has_anchor { '~' } else { '^' };
    let mut carets = String::from("    ");
    for col in 0..end {
        let c = if col < start {
            ' '
        } else if has_anchor && col >= a0 && col < a1 {
            '^'
        } else {
            primary
        };
        carets.push(c);
    }
    Some(carets)
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
impl PyHost {
    /// The immediate subclasses of `cname` — user classes that list it as a base
    /// (`cls.__subclasses__()`).
    pub fn subclasses_of(&self, cname: &str) -> Vec<String> {
        self.classes
            .values()
            .filter(|cd| cd.bases.iter().any(|b| b == cname))
            .map(|cd| cd.name.clone())
            .collect()
    }

    /// A frame object for `sys._getframe(depth)` — `depth` levels up from the
    /// caller. Minimal (scope name + current line); `f_globals` is live.
    pub fn current_frame_object(&mut self, depth: usize) -> Value {
        let idx = self.frames.len().saturating_sub(1 + depth);
        let (name, lineno) = self
            .frames
            .get(idx)
            .map(|f| (f.name.to_string(), f.line))
            .unwrap_or_else(|| ("<module>".to_string(), 0));
        self.alloc(PyObj::PyFrame { name, lineno })
    }

    /// If `gen` is an un-started generator/coroutine, mark it closed without
    /// running its body and return true (CPython's `close()` on an un-started
    /// coroutine; also clears the "never awaited" warning). False otherwise.
    pub fn close_unstarted_gen(&mut self, gen: &Value) -> bool {
        if let Some(PyObj::Generator { id }) = self.get(gen) {
            let id = *id as usize;
            if !self.generators[id].started {
                self.generators[id].done = true;
                return true;
            }
        }
        false
    }
}

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

/// Run every `atexit`-registered callback in reverse registration order (LIFO),
/// at interpreter shutdown. An exception from one callback is reported to stderr
/// and the rest still run, matching CPython's `atexit` teardown. The list is
/// drained so a re-entrant `_run_exitfuncs` does not run a callback twice.
pub fn run_atexit_callbacks() {
    let callbacks: Vec<AtexitCallback> = with_host(|h| std::mem::take(&mut h.atexit_callbacks));
    for (func, args, kwargs) in callbacks.into_iter().rev() {
        if let Err(e) = invoke(&func, args, kwargs) {
            // Clear the pending error so a later callback (and the caller) is not
            // aborted by this one's failure.
            with_host(|h| h.exc = None);
            eprintln!("Error in atexit._run_exitfuncs:\n{e}");
        }
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
        locals_set: Rc::new(locals.into_iter().collect()),
        is_class_body: false,
        self_obj: self_val,
        owner: owner.clone(),
        name: owner.map_or_else(|| Rc::from("<genexpr>"), |o| Rc::from(o.as_str())),
        line: 0,
        span: Span::NONE,
    };
    let id = with_host(|h| {
        let id = h.generators.len() as u32;
        h.generators.push(GenCell {
            kind,
            coro: None,
            yielder: std::ptr::null(),
            ctx: GenContext {
                frames: vec![frame],
                // Capture the defining module so every resume restores it.
                module: h.cur_module,
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
    // A generator body runs on its OWN stack, and everything it calls runs there
    // too — so the size that matters is not "one generator frame" but the whole
    // Python call chain the body can reach. corosensei's 1 MiB default is far too
    // small for that: `traceback.format_exception_only` is a generator that calls
    // through `_format_final_exc_line` into `_colorize`, and it overflowed. The
    // interpreter thread itself runs on 512 MiB (see `main`) for the same reason;
    // 64 MiB per generator is the same trade at a size that stays affordable when
    // many generators are live at once.
    const GENERATOR_STACK: usize = 64 * 1024 * 1024;
    let stack =
        corosensei::stack::DefaultStack::new(GENERATOR_STACK).expect("allocate generator stack");
    let coro = corosensei::Coroutine::with_stack(
        stack,
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

/// The exception OBJECT a generator's body left in flight, or `None` if it is
/// not raising.
///
/// [`gen_resume`] restores the caller's volatile context before returning, so
/// once it hands back `Err(msg)` the host's own `exc` is the CALLER's exception,
/// not the generator's — the raised object survives only in the generator's
/// parked `GenContext`. A caller that needs more than the terse `"Class: msg"`
/// string (the FFI bridge rebuilds the CPython exception from the class and
/// `args`, because `KeyError('k')` renders as `KeyError: 'k'` and re-parsing
/// that string yields `KeyError("'k'")`) has to read it from here.
pub fn gen_pending_exc(gen: &Value) -> Option<Value> {
    match with_host(|h| h.get(gen).cloned()) {
        Some(PyObj::Generator { id }) => with_host(|h| h.generators[id as usize].ctx.exc.clone()),
        _ => None,
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
/// PEP 479: a `StopIteration` escaping a generator BODY becomes a `RuntimeError`.
///
/// Without it an accidental `StopIteration` inside a generator -- most often a
/// bare `next(it)` on an exhausted iterator -- is indistinguishable from the
/// generator returning, so it silently ends whatever loop is driving it instead
/// of reporting a bug. pythonrs propagated it unchanged, so `list(gen())`
/// raised `StopIteration` where CPython raises
/// `RuntimeError: generator raised StopIteration`.
///
/// The original is kept as `__cause__`, as CPython does, so the traceback still
/// shows where it came from.
fn pep479_replace(e: String) -> String {
    if e != "StopIteration" && !e.starts_with("StopIteration:") && !e.starts_with("StopIteration\n")
    {
        return e;
    }
    with_host(|h| {
        // The live exception is the original `StopIteration` when the unwind
        // left it there; when it did not, it is rebuilt from the message so the
        // `__cause__` chain is never empty.
        let live = h.exc.clone().filter(|x| {
            matches!(h.get(x), Some(PyObj::Exception { class, .. }) if class == "StopIteration")
        });
        let cause = match live {
            Some(x) => x,
            None => {
                let detail = e
                    .strip_prefix("StopIteration:")
                    .map(str::trim)
                    .filter(|m| !m.is_empty());
                let args = match detail {
                    Some(m) => vec![h.new_str(m)],
                    None => vec![],
                };
                h.alloc(PyObj::Exception {
                    class: "StopIteration".into(),
                    args,
                })
            }
        };
        let msg = h.new_str("generator raised StopIteration");
        let rt = h.alloc(PyObj::Exception {
            class: "RuntimeError".into(),
            args: vec![msg],
        });
        h.set_exc_link(&rt, cause, Value::Undef);
        h.exc = Some(rt);
    });
    "RuntimeError: generator raised StopIteration".to_string()
}

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
                Err(e) => Err(pep479_replace(e)),
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
                | Some(PyObj::ItertoolsIter { .. })
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
    if let Some(id) = with_host(|h| h.foreign_id(v)) {
        // `make_iter_cb` runs the object's `__iter__` OUTSIDE the borrow, so a
        // `@dataclass` with a user `__iter__` can re-enter; `iter_step` then
        // advances borrow-free too.
        let it = crate::ffi::make_iter_cb(id)?;
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
    // A class whose metaclass defines `__iter__` (Enum subclasses) materializes
    // via `type(cls).__iter__(cls)` — reached by `list(Color)`, `[*Color]`, etc.
    if let Some(m) = with_host(|h| h.metaclass_method(v, "__iter__")) {
        let it = invoke(&m, vec![v.clone()], vec![])?;
        return iter_vec(&it);
    }
    with_host(|h| h.iter_items(v))
}

/// CPython's optimization level: `sys.flags.optimize`.
///
/// `-O`/`-OO` are folded into `PYTHONOPTIMIZE` by the CLI, so this one reader
/// serves both spellings. CPython's parse is deliberately lax — an empty value
/// is level 0, an integer is that integer, and anything else non-empty is 1
/// (`PYTHONOPTIMIZE=x` runs optimized).
pub fn optimize_level() -> u8 {
    match std::env::var("PYTHONOPTIMIZE") {
        Err(_) => 0,
        Ok(s) if s.is_empty() => 0,
        Ok(s) => s.parse::<u8>().unwrap_or(1),
    }
}

/// True if `v` is an iterator — something [`iter_step`] can advance and CPython
/// would accept from `next()`. That is a *narrower* question than "is iterable":
/// a `list` is iterable but is not an iterator, and a class defining `__next__`
/// is an iterator even without `__iter__`.
pub fn is_iterator(v: &Value) -> bool {
    with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__next__").is_some(),
        Some(PyObj::Iter(_))
        | Some(PyObj::Generator { .. })
        | Some(PyObj::Zip { .. })
        | Some(PyObj::MapObj { .. })
        | Some(PyObj::FilterObj { .. })
        | Some(PyObj::EnumerateObj { .. })
        | Some(PyObj::ItertoolsIter { .. })
        | Some(PyObj::CallIter { .. })
        | Some(PyObj::CsvReader { .. }) => true,
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::is_iterator(*id),
        _ => false,
    })
}

/// Run a user class's `__iter__` and require that it handed back an ITERATOR.
///
/// CPython enforces this where `iter()` is applied, so `for x in obj` and
/// `list(obj)` report it too — not just the `iter()` builtin. pythonrs checked
/// it only in the builtin, and the other two paths fell through to a generic
/// `not an iterator` from whatever tried to step the result. One definition now
/// serves all three; the message is CPython's.
pub fn call_iter_dunder(v: &Value) -> Result<Value, String> {
    let it = call_method(v, "__iter__", vec![], vec![])?;
    if is_iterator(&it) {
        return Ok(it);
    }
    Err(type_error(&format!(
        "iter() returned non-iterator of type '{}'",
        with_host(|h| h.type_name(&it))
    )))
}

/// Turn `v` into something [`iter_step`] can drive, running any user-level
/// `__iter__` OUTSIDE the host borrow.
///
/// `PyHost::make_iter` cannot do this itself: it holds `&mut self`, so it can
/// never call back into Python, and it therefore has no way to honor a user
/// class's iteration protocol. Every lazy iterator (`zip`, `map`, `filter`,
/// `enumerate`, all of `itertools`) stores its sources through here, so a custom
/// iterable works as a source for any of them — `pathlib.relative_to` chains
/// over a `_PathParents`, whose only protocol is `__len__`/`__getitem__`.
pub fn make_iterator(v: &Value) -> Result<Value, String> {
    let protocol = with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => Some((
            h.class_lookup(&i.class, "__iter__").is_some(),
            h.class_lookup(&i.class, "__getitem__").is_some(),
        )),
        _ => None,
    });
    match protocol {
        // `__iter__` hands back a real iterator, so iteration stays lazy.
        Some((true, _)) => {
            let it = call_iter_dunder(v)?;
            // `call_iter_dunder` has already established that `it` IS an
            // iterator, and `iter_step` drives a user instance through its own
            // `__next__`. `make_iter` knows only the native iterables and
            // rejected it — so `zip`/`map`/`enumerate`/`itertools` over a
            // hand-written iterator class all raised
            // `TypeError: 'C' object is not iterable` while `list(C())` and
            // `for x in C()` (which never come through here) worked.
            if with_host(|h| matches!(h.get(&it), Some(PyObj::Instance(_)))) {
                return Ok(it);
            }
            with_host(|h| h.make_iter(&it))
        }
        // The old-style `__getitem__(0..)` sequence protocol has no iterator
        // object to hand back, so it is materialized once here.
        Some((false, true)) => {
            let items = iter_instance_items(v)?;
            Ok(with_host(|h| h.new_iter_seq(items)))
        }
        _ => with_host(|h| h.make_iter(v)),
    }
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
        // Validated, so `list(obj)`/`for x in obj` report a non-iterator
        // `__iter__` the way CPython does instead of failing later, inside
        // whatever tried to step the result.
        let it = call_iter_dunder(v)?;
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

/// One step of a lazy `itertools` iterator. State is cloned out so the source
/// pulls / predicate calls run with no host borrow held, then the mutated state
/// (and any exhaustion latch) is written back.
fn itertools_step(it: &Value) -> Result<Option<Value>, String> {
    let (kind, sources, func, mut nums, mut buf, mut flag, done) =
        match with_host(|h| h.get(it).cloned()) {
            Some(PyObj::ItertoolsIter {
                kind,
                sources,
                func,
                nums,
                buf,
                flag,
                done,
            }) => (kind, sources, func, nums, buf, flag, done),
            _ => return Err(type_error("not an iterator")),
        };
    if done {
        return Ok(None);
    }
    let mut finished = false;
    let result: Option<Value> = match kind {
        ItKind::Count => {
            // buf = [current, step]; advance with the numeric `+` so ints stay
            // ints, floats stay floats, and a bignum start never truncates.
            let cur = buf[0].clone();
            buf[0] = with_host(|h| h.arith(NumOp::Add, &cur, &buf[1]))?;
            Some(cur)
        }
        ItKind::Repeat => {
            // nums[0] = remaining (-1 = infinite).
            if nums[0] == 0 {
                finished = true;
                None
            } else {
                if nums[0] > 0 {
                    nums[0] -= 1;
                }
                Some(buf[0].clone())
            }
        }
        ItKind::Cycle => {
            if !flag {
                // First pass: pull from the source, remembering each item.
                match iter_step(&sources[0])? {
                    Some(v) => {
                        buf.push(v.clone());
                        Some(v)
                    }
                    None => {
                        flag = true; // source exhausted; replay the buffer
                        if buf.is_empty() {
                            finished = true;
                            None
                        } else {
                            let v = buf[0].clone();
                            nums = vec![1 % buf.len() as i64];
                            Some(v)
                        }
                    }
                }
            } else if buf.is_empty() {
                finished = true;
                None
            } else {
                let idx = nums[0] as usize;
                let v = buf[idx].clone();
                nums[0] = ((idx + 1) % buf.len()) as i64;
                Some(v)
            }
        }
        ItKind::Chain => {
            let mut out = None;
            while (nums[0] as usize) < sources.len() {
                match iter_step(&sources[nums[0] as usize])? {
                    Some(v) => {
                        out = Some(v);
                        break;
                    }
                    None => nums[0] += 1,
                }
            }
            if out.is_none() {
                finished = true;
            }
            out
        }
        // `flag` latches a pending `initial=` seed: it is yielded verbatim before
        // the source is touched at all, which is why `accumulate([], initial=5)`
        // still yields `[5]`.
        ItKind::Accumulate if flag => {
            flag = false;
            Some(buf[0].clone())
        }
        ItKind::Accumulate => match iter_step(&sources[0])? {
            None => {
                finished = true;
                None
            }
            Some(v) => {
                let acc = if buf.is_empty() {
                    v
                } else {
                    let prev = buf[0].clone();
                    if matches!(func, Value::Undef) {
                        with_host(|h| h.arith(NumOp::Add, &prev, &v))?
                    } else {
                        invoke(&func, vec![prev, v], vec![])?
                    }
                };
                buf = vec![acc.clone()];
                Some(acc)
            }
        },
        ItKind::StarMap => match iter_step(&sources[0])? {
            None => {
                finished = true;
                None
            }
            Some(tup) => {
                let call_args = iter_vec(&tup)?;
                Some(invoke(&func, call_args, vec![])?)
            }
        },
        ItKind::Compress => {
            // sources = [data, selectors]; yield data where selector is truthy.
            let mut out = None;
            loop {
                let d = iter_step(&sources[0])?;
                let s = iter_step(&sources[1])?;
                match (d, s) {
                    (Some(dv), Some(sv)) => {
                        if with_host(|h| h.truthy(&sv)) {
                            out = Some(dv);
                            break;
                        }
                    }
                    _ => {
                        finished = true;
                        break;
                    }
                }
            }
            out
        }
        ItKind::DropWhile => {
            // flag = still dropping.
            let mut out = None;
            loop {
                match iter_step(&sources[0])? {
                    None => {
                        finished = true;
                        break;
                    }
                    Some(v) => {
                        if flag {
                            let keep = invoke(&func, vec![v.clone()], vec![])?;
                            if with_host(|h| h.truthy(&keep)) {
                                continue; // still dropping
                            }
                            flag = false;
                        }
                        out = Some(v);
                        break;
                    }
                }
            }
            out
        }
        ItKind::TakeWhile => match iter_step(&sources[0])? {
            None => {
                finished = true;
                None
            }
            Some(v) => {
                let keep = invoke(&func, vec![v.clone()], vec![])?;
                if with_host(|h| h.truthy(&keep)) {
                    Some(v)
                } else {
                    finished = true;
                    None
                }
            }
        },
        ItKind::FilterFalse => {
            let mut out = None;
            loop {
                match iter_step(&sources[0])? {
                    None => {
                        finished = true;
                        break;
                    }
                    Some(v) => {
                        let truthy = if matches!(func, Value::Undef) {
                            with_host(|h| h.truthy(&v))
                        } else {
                            let r = invoke(&func, vec![v.clone()], vec![])?;
                            with_host(|h| h.truthy(&r))
                        };
                        if !truthy {
                            out = Some(v);
                            break;
                        }
                    }
                }
            }
            out
        }
        ItKind::ISlice => {
            // nums = [next_yield_index, stop(-1=inf), step, cursor].
            let mut out = None;
            loop {
                if nums[1] >= 0 && nums[3] >= nums[1] {
                    finished = true;
                    break;
                }
                match iter_step(&sources[0])? {
                    None => {
                        finished = true;
                        break;
                    }
                    Some(v) => {
                        let cur = nums[3];
                        nums[3] += 1;
                        if cur == nums[0] {
                            nums[0] += nums[2];
                            out = Some(v);
                            break;
                        }
                    }
                }
            }
            out
        }
        ItKind::ZipLongest => {
            // func = fillvalue. Yield a tuple until every source is exhausted.
            let mut row = Vec::with_capacity(sources.len());
            let mut any = false;
            for s in &sources {
                match iter_step(s)? {
                    Some(v) => {
                        any = true;
                        row.push(v);
                    }
                    None => row.push(func.clone()),
                }
            }
            if any {
                Some(with_host(|h| h.new_tuple(row)))
            } else {
                finished = true;
                None
            }
        }
        ItKind::Pairwise => {
            if buf.is_empty() {
                match iter_step(&sources[0])? {
                    Some(v) => buf.push(v),
                    None => {
                        finished = true;
                    }
                }
            }
            if finished {
                None
            } else {
                match iter_step(&sources[0])? {
                    Some(v) => {
                        let prev = buf[0].clone();
                        buf = vec![v.clone()];
                        Some(with_host(|h| h.new_tuple(vec![prev, v])))
                    }
                    None => {
                        finished = true;
                        None
                    }
                }
            }
        }
        // `itertools.batched(iterable, n, *, strict=False)` — consecutive
        // n-tuples. `nums[0]` is the batch size, `flag` is `strict`. The last
        // batch is short when the input does not divide evenly; under `strict`
        // that short batch is a ValueError instead, and the iterator is left
        // exhausted so the error is not re-raised on the next pull.
        ItKind::Batched => {
            let n = nums[0] as usize;
            let mut items = Vec::with_capacity(n);
            while items.len() < n {
                match iter_step(&sources[0])? {
                    Some(v) => items.push(v),
                    None => break,
                }
            }
            if items.is_empty() {
                finished = true;
                None
            } else if items.len() < n && flag {
                with_host(|h| {
                    if let Some(PyObj::ItertoolsIter { done, .. }) = h.get_mut(it) {
                        *done = true;
                    }
                });
                return Err("ValueError: batched(): incomplete batch".into());
            } else {
                Some(with_host(|h| h.new_tuple(items)))
            }
        }
    };
    with_host(|h| {
        if let Some(PyObj::ItertoolsIter {
            nums: n,
            buf: b,
            flag: f,
            done: d,
            ..
        }) = h.get_mut(it)
        {
            *n = nums;
            *b = buf;
            *f = flag;
            if finished {
                *d = true;
            }
        }
    });
    Ok(result)
}

/// Advance any iterator — including a generator or a lazy composite iterator
/// (`zip`/`map`/`filter`/`enumerate`) — by one step. Composite iterators pull
/// from their sources with the host borrow released, so an infinite source
/// (e.g. `itertools.count()`) never materializes.
pub fn iter_step(it: &Value) -> Result<Option<Value>, String> {
    // Dispatch on the variant WITHOUT cloning it. Every arm below re-reads
    // through `it` anyway, and the clone copied the iterator's whole state — for
    // a list iterator, the entire list — on each step, which made iterating a
    // list (and so every comprehension over one) quadratic.
    enum Step {
        Generator,
        Zip,
        Map,
        Filter,
        Enumerate,
        CallIter,
        Itertools,
        /// A user object driving its own `__next__` (the class defines it).
        UserNext,
        #[cfg(feature = "stdlib-ffi")]
        Foreign(u32),
        Plain,
    }
    let step = with_host(|h| match h.get(it) {
        Some(PyObj::Instance(i)) if h.class_lookup(&i.class, "__next__").is_some() => {
            Step::UserNext
        }
        Some(PyObj::Generator { .. }) => Step::Generator,
        Some(PyObj::Zip { .. }) => Step::Zip,
        Some(PyObj::MapObj { .. }) => Step::Map,
        Some(PyObj::FilterObj { .. }) => Step::Filter,
        Some(PyObj::EnumerateObj { .. }) => Step::Enumerate,
        Some(PyObj::CallIter { .. }) => Step::CallIter,
        Some(PyObj::CsvReader { .. }) => Step::Plain,
        Some(PyObj::ItertoolsIter { .. }) => Step::Itertools,
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => Step::Foreign(*id),
        _ => Step::Plain,
    });
    match step {
        Step::Generator => gen_resume(it, Value::Undef),
        Step::Zip => zip_step(it),
        Step::Map => map_step(it),
        Step::Filter => filter_step(it),
        Step::Enumerate => enumerate_step(it),
        Step::CallIter => calliter_step(it),
        Step::Itertools => itertools_step(it),
        // A user iterator steps by calling its own `__next__`, which must run
        // outside the host borrow; `StopIteration` is exhaustion, every other
        // exception propagates. Never materialize here — the object may be
        // infinite (`itertools.count`-style), and CPython never drains it.
        Step::UserNext => match call_method(it, "__next__", vec![], vec![]) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.contains("StopIteration") => Ok(None),
            Err(e) => Err(e),
        },
        // A foreign (CPython) iterator advances with the host borrow released so
        // a lazy stdlib iterator running a pythonrs callback can re-enter.
        #[cfg(feature = "stdlib-ffi")]
        Step::Foreign(id) => crate::ffi::iter_next_cb(id),
        Step::Plain => with_host(|h| h.iter_next(it)),
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

thread_local! {
    /// Modules whose body is currently executing. CPython publishes a module in
    /// `sys.modules` BEFORE running it, so a circular import sees the partial
    /// module; this runtime caches only on completion, so the same cycle would
    /// re-enter the body forever. `encodings/__init__` does `from . import
    /// aliases`, whose parent is the very module still running — that overflowed
    /// the stack.
    static IMPORTING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether `name`'s body is on the current import stack.
fn import_in_progress(name: &str) -> bool {
    IMPORTING.with(|s| s.borrow().iter().any(|n| n == name))
}

/// Import a module by name, memoized through the host's `sys.modules` cache: the
/// first import runs (native arm, vendored `.py`, or bridge), later imports of the
/// same name return the identical cached object — CPython's run-once semantics.
pub fn import_module(name: &str) -> Result<Value, String> {
    if let Some(m) = with_host(|h| h.cached_module(name)) {
        return Ok(m);
    }
    // Import parent packages first, as CPython does (`import a.b.c` imports `a`,
    // then `a.b`, then `a.b.c`). A package `__init__` may register the child as a
    // submodule alias — os/__init__ runs `sys.modules['os.path'] = posixpath`,
    // collections/__init__ aliases `collections.abc` — so once the parent runs,
    // the child resolves from the cache. A parent that fails is non-fatal here;
    // the direct resolution below reports the real error.
    if let Some((parent, _)) = name.rsplit_once('.') {
        // A parent already on the import stack is mid-body: importing it again
        // would re-run it. Resolve the child directly instead.
        if with_host(|h| h.cached_module(parent)).is_none() && !import_in_progress(parent) {
            let _ = import_module(parent);
            if let Some(m) = with_host(|h| h.cached_module(name)) {
                return Ok(m);
            }
        }
    }
    IMPORTING.with(|s| s.borrow_mut().push(name.to_string()));
    let module = import_module_inner(name);
    IMPORTING.with(|s| {
        let mut b = s.borrow_mut();
        if let Some(p) = b.iter().rposition(|n| n == name) {
            b.remove(p);
        }
    });
    let module = module?;
    with_host(|h| h.cache_module(name, module.clone()));
    // Bind a submodule as an attribute of its parent package, as CPython's import
    // system does: after `import a.b` (or a relative `from .b import *`), `a.b`
    // stays reachable and `from a import b` finds it.
    if let Some((parent, leaf)) = name.rsplit_once('.') {
        if let Some(pmod) = with_host(|h| h.cached_module(parent)) {
            with_host(|h| h.set_module_attr(&pmod, leaf, module.clone()));
        }
    }
    Ok(module)
}

/// Resolve a relative import (`from <dots><modpart> import <name>`) against the
/// currently-running module's `__package__`. `level` is the number of leading
/// dots; `modpart` is the (possibly empty) dotted path after them. Returns the
/// value to bind under `name`. Mirrors CPython's `importlib._bootstrap`:
///   - `from . import sub`   → import (and return) the submodule `pkg.sub`,
///     else the attribute `sub` defined in the package `__init__`.
///   - `from .mod import x`  → import `pkg.mod`, return its attribute `x`.
///   - `from . import *`     → return the anchor package for `IMPORT_STAR`.
pub fn import_relative(level: usize, modpart: &str, name: &str) -> Result<Value, String> {
    let pkg = with_host(|h| h.current_package());
    let anchor = resolve_relative_anchor(&pkg, level)?;
    let base = match (anchor.is_empty(), modpart.is_empty()) {
        (_, true) => anchor.clone(),
        (true, false) => modpart.to_string(),
        (false, false) => format!("{anchor}.{modpart}"),
    };
    // `from .pkg import *` (or `from . import *`) — hand the source package back
    // for the `IMPORT_STAR` op that follows.
    if name == "*" {
        return import_module(&base);
    }
    if modpart.is_empty() {
        // `from . import name`: prefer the submodule `base.name`; if there is no
        // such module, `name` is an attribute defined in the package body.
        let sub = if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}.{name}")
        };
        match import_module(&sub) {
            Ok(m) => {
                // Bind the submodule as an attribute of its package, as CPython does.
                // Only if the package is already in the cache: `from . import x`
                // runs INSIDE the package body, so importing it here to get a
                // handle would re-execute the very module that is running
                // (`encodings/__init__` does exactly this) and recurse forever.
                if let Some(base_mod) = with_host(|h| h.cached_module(&base)) {
                    with_host(|h| h.set_module_attr(&base_mod, name, m.clone()));
                }
                return Ok(m);
            }
            Err(e) => {
                // Only fall back to a package attribute when the SUBMODULE itself
                // is absent. A submodule that exists but failed to execute (e.g.
                // a missing C-accelerator dependency) propagates its real error.
                let missing_sub = e.contains("No module named")
                    && (e.contains(&format!("'{sub}'")) || e.contains(&format!("'{name}'")));
                if !missing_sub {
                    return Err(e);
                }
                let base_mod = import_module(&base)?;
                return with_host(|h| h.get_attr(&base_mod, name));
            }
        }
    }
    // `from .mod import name`: import the source module, read its attribute.
    let base_mod = import_module(&base)?;
    with_host(|h| h.get_attr(&base_mod, name))
}

/// Strip `level - 1` trailing components from the anchor package `pkg` (CPython's
/// relative-import base: one dot = the current package, each extra dot climbs
/// one parent). Errors past the top-level package.
fn resolve_relative_anchor(pkg: &str, level: usize) -> Result<String, String> {
    if level == 0 {
        return Ok(pkg.to_string());
    }
    let mut bits: Vec<&str> = if pkg.is_empty() {
        Vec::new()
    } else {
        pkg.split('.').collect()
    };
    let strip = level - 1;
    if strip > bits.len() {
        return Err("ImportError: attempted relative import beyond top-level package".to_string());
    }
    bits.truncate(bits.len() - strip);
    Ok(bits.join("."))
}

/// The uncached import resolution: native inline arms, then the vendored CPython
/// stdlib `.py` shipped in `pylib/` (run on pythonrs itself), then — only in the
/// default build and only until the native C-accelerator floor is complete — the
/// `stdlib-ffi` bridge.
/// Build a CPython "struct sequence" — a tuple that also answers by field name
/// and reprs as `name(field=value, …)`. `sys.version_info`, `sys.float_info` and
/// friends are all this shape, and code both indexes and attribute-reads them.
fn struct_seq(h: &mut PyHost, type_name: &str, fields: Vec<(&str, Value)>) -> Value {
    let names: Vec<String> = fields.iter().map(|(k, _)| k.to_string()).collect();
    let vals: Vec<Value> = fields.into_iter().map(|(_, v)| v).collect();
    let t = h.new_tuple(vals);
    if let Value::Obj(i) = t {
        h.nt_meta.insert(
            i,
            NtMeta {
                type_name: type_name.to_string(),
                fields: names,
            },
        );
    }
    t
}

/// pythonrs's live `sys.path` as plain strings: whatever the `sys` module holds
/// if the program has imported it (so a `sys.path.insert(0, …)` is honored),
/// otherwise the startup value — the script's directory, or `""` (the current
/// directory) for `-c`/`-m`/stdin, exactly as CPython seeds `sys.path[0]`.
/// Non-string entries (a `PathLike`, a custom finder) are skipped rather than
/// stringified, since the bridge can only pass a path across.
#[cfg(feature = "stdlib-ffi")]
fn current_search_paths() -> Vec<String> {
    with_host(|h| {
        if let Some(sys) = h.cached_module("sys") {
            if let Ok(path) = h.get_attr(&sys, "path") {
                if let Some(PyObj::List(items)) = h.get(&path) {
                    let items = items.clone();
                    return items.iter().filter_map(|v| h.as_str(v)).collect();
                }
            }
        }
        vec![match &h.main_file {
            Some(f) => std::path::Path::new(f)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None => String::new(),
        }]
    })
}

/// pythonrs's live `sys.argv` as plain strings, for mirroring into the embedded
/// interpreter. Read from the `sys` module rather than `PyHost::argv` so a
/// program that rewrote `sys.argv` before importing argparse gets what it set;
/// `PyHost::argv` (what `init_runtime` installed) is the fallback.
#[cfg(feature = "stdlib-ffi")]
fn current_argv() -> Vec<String> {
    with_host(|h| {
        if let Some(sys) = h.cached_module("sys") {
            if let Ok(argv) = h.get_attr(&sys, "argv") {
                if let Some(PyObj::List(items)) = h.get(&argv) {
                    let items = items.clone();
                    return items.iter().filter_map(|v| h.as_str(v)).collect();
                }
            }
        }
        h.argv.clone()
    })
}

/// Resolve `name` against pythonrs's `sys.path` (the program's own directory and
/// anything it inserted) and run it on pythonrs itself. `None` = no such file on
/// those roots, so the caller falls through to the stdlib resolvers.
///
/// An empty entry means the current directory, exactly as CPython reads
/// `sys.path[0] == ''` for `-c`/`-m`/stdin.
#[cfg(feature = "stdlib-ffi")]
fn try_import_user_path(name: &str) -> Option<Result<Value, String>> {
    let rel = name.replace('.', "/");
    for root in current_search_paths() {
        let root = std::path::Path::new(if root.is_empty() { "." } else { &root });
        for cand in [
            root.join(format!("{rel}.py")),
            root.join(&rel).join("__init__.py"),
        ] {
            if !cand.is_file() {
                continue;
            }
            return Some(match std::fs::read_to_string(&cand) {
                Ok(src) => run_vendored_module(name, &src, &cand),
                Err(e) => Err(format!("ImportError: cannot read {}: {e}", cand.display())),
            });
        }
    }
    None
}

/// Seed the dunders a natively-backed module presents, then let its own entries
/// override any of them.
///
/// A pythonrs native module is compiled INTO the interpreter, which makes it a
/// builtin module in CPython's sense -- the same category as `sys` -- and this
/// is the set a CPython builtin carries. Before this, a native module's
/// namespace held nothing but its functions: `vars(re)` was `[]` against
/// CPython's 11 entries, and `re.__doc__`/`__package__`/`__spec__` were all
/// `AttributeError`. (`__name__` alone worked, resolved from the module object
/// rather than the namespace, which is why the gap was easy to miss.)
///
/// **There is deliberately no `__file__`.** CPython gives one to a module that
/// was LOADED from a file and withholds it from a builtin, and `re` here is the
/// Rust `regex` engine, not `pylib/re/__init__.py`. Pointing `__file__` at that
/// file would name source that does not run -- it would send a reader, a
/// traceback, or `inspect.getsource` to the wrong code. Absent is the accurate
/// answer, and it is the same answer CPython gives for `sys`.
///
/// `__loader__`/`__spec__` are `None` rather than import-machinery objects.
/// `None` is a value CPython itself uses for a module created without a spec, so
/// it reports "no import machinery here" without inventing a loader that does
/// not exist.
fn seed_module_dunders(h: &mut PyHost, ns: &mut NameMap, name: &str) {
    let doc = Value::Undef;
    // A native module is never a package, so its `__package__` is the parent
    // path -- empty for a top-level name, as CPython's `sys.__package__` is.
    let package = match name.rsplit_once('.') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    };
    let name_v = h.new_str(name.to_string());
    let package_v = h.new_str(package);
    ns.insert("__name__".to_string(), name_v);
    ns.insert("__doc__".to_string(), doc);
    ns.insert("__package__".to_string(), package_v);
    ns.insert("__loader__".to_string(), Value::Undef);
    ns.insert("__spec__".to_string(), Value::Undef);
}

fn import_module_inner(name: &str) -> Result<Value, String> {
    // `collections.abc` is an alias for the pure-Python `_collections_abc`
    // module; CPython wires it with `sys.modules['collections.abc'] =
    // _collections_abc` in collections/__init__. pythonrs serves `collections`
    // from a native arm (which never runs that line), so alias it here.
    if name == "collections.abc" {
        return import_module("_collections_abc");
    }
    // `_ast` — the node types `ast.py` is built on. They are pure data (a name, a
    // base, a `_fields` tuple), so the module is DECLARED by a table in Rust and
    // DEFINED by running the Python that table expands to — the same relationship
    // CPython's generated C file has to `Parser/Python.asdl`.
    // `_thread` — the Python-shaped half of the threading primitives (the handle
    // object, the shutdown bookkeeping). The host-backed pieces stay in the
    // native `_thread_core` arm this module imports from.
    //
    // Native in BOTH builds, unlike the other shadows: this is not a subset of
    // CPython's `_thread` but a semantic OVERRIDE. pythonrs's object heap is a
    // `thread_local` and generators suspend by switching stacks on the same OS
    // thread, so a target handed to CPython's `_thread.start_new_thread` would
    // run on a foreign thread whose heap is empty. Serving the native module
    // keeps `get_ident`/`start_joinable_thread` describing the single thread
    // user code actually runs on.
    if name == "_thread" {
        let src = crate::stdlib::pythread::module_source();
        return run_vendored_module("_thread", src, std::path::Path::new("<_thread>"));
    }
    #[cfg(not(feature = "stdlib-ffi"))]
    if name == "_ast" {
        let src = crate::stdlib::pyast::module_source();
        return run_vendored_module("_ast", &src, std::path::Path::new("<_ast>"));
    }
    // Native stdlib modules under src/stdlib. Their `entries` return owned-String
    // keys (vs the `&str` keys of the inline arms below), so build the namespace
    // here and return before the `&str` match. These are pure-Python subsets
    // (e.g. the native `textwrap` covers only `width`, not the full keyword-
    // option surface), so with the FFI bridge on they are skipped in favor of
    // the real CPython modules; they serve only the `--no-default-features` build.
    #[cfg(not(feature = "stdlib-ffi"))]
    let stdlib_entries: Option<Vec<(String, Value)>> = match name {
        // `_struct` — ported from RustPython (MIT); `struct.py` is `from _struct
        // import *`, so this is the whole module.
        "_struct" => Some(with_host(crate::stdlib::pystruct::entries)),
        // `binascii` — ported from RustPython (MIT); `base64` is built on it.
        "binascii" => Some(with_host(crate::stdlib::binascii::entries)),
        // `_codecs` — ported from RustPython (MIT). `codecs.py` is
        // `from _codecs import *`, and the interpreter cannot start text I/O
        // without it.
        "_codecs" => Some(with_host(crate::stdlib::codecs::entries)),
        // `_io` — the concrete streams `io.py` declares its ABCs over. Six
        // modules (io, pathlib, logging, unittest, hashlib, pprint) start here.
        "_io" => Some(with_host(crate::stdlib::pyio::entries)),
        // `_tokenize` — the tokenizer `tokenize.py` drives, and through
        // `traceback`, what `logging`/`unittest`/`hashlib` reach first.
        "_tokenize" => Some(with_host(crate::stdlib::pytokenize::entries)),
        // `_opcode` — CPython instruction metadata. `opcode.py` turns it into the
        // `hasarg`/`hasjump`/… lists, `dis` reads those, and `inspect` imports
        // `dis`; that chain is why the module has to exist.
        "_opcode" => Some(with_host(crate::stdlib::pyopcode::entries)),
        // `_imp` — the import primitives `importlib` is handed at startup.
        "_imp" => Some(with_host(crate::stdlib::pyimp::entries)),
        // `_signal` — the POSIX signal numbers `signal.py` re-exports as enums.
        "_signal" => Some(with_host(crate::stdlib::pysignal::entries)),
        // `_csv` — the CSV parser and formatter `csv.py` builds its readers,
        // writers and Sniffer on.
        "_csv" => Some(with_host(crate::stdlib::pycsv::entries)),
        // The hash accelerators `hashlib` dispatches to, one per algorithm family.
        "_md5" | "_sha1" | "_sha2" | "_sha3" | "_blake2" => {
            with_host(|h| crate::stdlib::pyhash::entries(h, name))
        }
        "marshal" => Some(with_host(crate::stdlib::pyimp::marshal_entries)),
        _ => None,
    };
    #[cfg(feature = "stdlib-ffi")]
    let stdlib_entries: Option<Vec<(String, Value)>> = None;
    if let Some(entries) = stdlib_entries {
        return Ok(with_host(|h| {
            let mut ns = NameMap::default();
            seed_module_dunders(h, &mut ns, name);
            for (k, v) in entries {
                ns.insert(k, v);
            }
            let slot = h.new_module_slot(ns);
            h.alloc(PyObj::Module {
                name: name.to_string(),
                slot,
            })
        }));
    }

    let entries: Vec<(&str, Value)> = match name {
        // `builtins` as an importable module: every builtin function/type/exception
        // resolves to the same `PyObj::Builtin` a bare-name lookup would, plus the
        // singletons. The self-contained build needs this — `functools`, `operator`,
        // `enum`, `re` all `import builtins`. On the ffi build CPython's richer
        // builtins module (with `open`/`compile`/`vars`/… pythonrs lacks) is used.
        #[cfg(not(feature = "stdlib-ffi"))]
        "builtins" => with_host(|h| {
            let mut v: Vec<(&str, Value)> = Vec::new();
            for n in crate::builtins::builtin_names() {
                v.push((n, h.alloc(PyObj::Builtin(n.to_string()))));
            }
            v.push(("None", Value::Undef));
            v.push(("True", Value::Bool(true)));
            v.push(("False", Value::Bool(false)));
            v.push(("NotImplemented", h.alloc(PyObj::NotImplemented)));
            v.push(("Ellipsis", h.alloc(PyObj::Ellipsis)));
            v.push(("__debug__", Value::Bool(true)));
            v
        }),
        // `copy` is native (a CPython round-trip would deep-copy by value, losing
        // shallow-copy sharing and instance identity).
        "copy" => with_host(|h| {
            vec![
                ("copy", h.alloc(PyObj::Builtin("copy.copy".into()))),
                ("deepcopy", h.alloc(PyObj::Builtin("copy.deepcopy".into()))),
            ]
        }),
        // `functools` is native-shadowed only for `total_ordering` (which must run
        // natively so the class stays a native pythonrs class — a CPython round
        // trip would return a Foreign class whose native `__init__` can't set
        // attributes). Every other member (`reduce`, `partial`, `lru_cache`,
        // `wraps`, `cmp_to_key`, …) misses this namespace and defers to the real
        // CPython `functools` via `module_ffi_fallback`.
        #[cfg(feature = "stdlib-ffi")]
        "functools" => with_host(|h| {
            vec![
                (
                    "total_ordering",
                    h.alloc(PyObj::Builtin("functools.total_ordering".into())),
                ),
                (
                    "cached_property",
                    h.alloc(PyObj::Builtin("functools.cached_property".into())),
                ),
            ]
        }),
        // `contextlib` is native-shadowed for `redirect_stdout`/`redirect_stderr`,
        // which must retarget pythonrs's own `print` stream (a CPython
        // redirect_stdout retargets CPython's `sys.stdout`, which pythonrs's print
        // doesn't consult). Every other member defers to the real CPython module.
        #[cfg(feature = "stdlib-ffi")]
        "contextlib" => with_host(|h| {
            vec![
                (
                    "redirect_stdout",
                    h.alloc(PyObj::Builtin("contextlib.redirect_stdout".into())),
                ),
                (
                    "redirect_stderr",
                    h.alloc(PyObj::Builtin("contextlib.redirect_stderr".into())),
                ),
            ]
        }),
        // `collections` is native-shadowed for the four MUTABLE container types.
        // They have to stay native under the bridge: a CPython `defaultdict`/
        // `Counter`/`deque` hands its values back through `py_to_value`, which
        // marshals an exact `list`/`dict` BY VALUE — so `dd['k'].append(1)` would
        // mutate a throwaway copy and the write would be lost. `namedtuple` is
        // deliberately NOT shadowed: its instances are immutable (nothing to write
        // back) and CPython's builds real `_tuplegetter` field descriptors, which
        // carry the writable `__doc__` that `dis.py` sets on every field.
        // `ChainMap`, `UserDict`, `UserList`, `UserString` and `abc` miss this
        // namespace and defer to the real CPython module via `module_ffi_fallback`.
        // The self-contained build instead runs the full vendored
        // `collections/__init__.py` over the native `_collections` accelerators,
        // so it needs no shadow.
        #[cfg(feature = "stdlib-ffi")]
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
            ]
        }),
        // `_random` — the Mersenne Twister RNG. `Random` is a subclassable class
        // (random.py does `class Random(_random.Random)`); its methods dispatch
        // natively against per-instance MT state.
        "_random" => with_host(|h| {
            if !h.classes.contains_key("_random.Random") {
                let mut ns: NameMap = NameMap::default();
                for m in ["random", "seed", "getrandbits", "getstate", "setstate"] {
                    ns.insert(
                        m.to_string(),
                        h.alloc(PyObj::Builtin(format!("_random.Random.{m}"))),
                    );
                }
                h.register_class_meta("_random.Random", vec![], ns, "type");
            }
            let cls = h.alloc(PyObj::Class("_random.Random".to_string()));
            vec![("Random", cls)]
        }),
        // `posix` — the Unix syscall surface `os` sits on. Backed by std::fs/
        // std::env/libc.
        "posix" => with_host(|h| {
            const FNS: &[&str] = &[
                "getcwd",
                "getcwdb",
                "chdir",
                "listdir",
                "scandir",
                "stat",
                "lstat",
                "fstat",
                "mkdir",
                "makedirs",
                "rmdir",
                "remove",
                "unlink",
                "rename",
                "replace",
                "getpid",
                "getppid",
                "getuid",
                "geteuid",
                "getgid",
                "getegid",
                "urandom",
                "umask",
                "system",
                "strerror",
                "getenv",
                "putenv",
                "unsetenv",
                "access",
                "fspath",
                "_exit",
                "abort",
                "getpgrp",
                "cpu_count",
                "device_encoding",
                "get_terminal_size",
                "isatty",
                "pipe",
                "dup",
                "close",
                "read",
                "write",
                "open",
                "lseek",
                "fsync",
                "kill",
                "waitpid",
                "_create_environ",
                "readlink",
                "symlink",
                "link",
                "chmod",
                "utime",
                "truncate",
                "sync",
                "get_inheritable",
                "set_inheritable",
                "ftruncate",
                "sched_yield",
            ];
            let mut v: Vec<(&str, Value)> = FNS
                .iter()
                .map(|f| (*f, h.alloc(PyObj::Builtin(format!("posix.{f}")))))
                .collect();
            // Constants (from libc where platform-specific).
            let consts: &[(&str, i64)] = &[
                ("F_OK", libc::F_OK as i64),
                ("R_OK", libc::R_OK as i64),
                ("W_OK", libc::W_OK as i64),
                ("X_OK", libc::X_OK as i64),
                ("SEEK_SET", libc::SEEK_SET as i64),
                ("SEEK_CUR", libc::SEEK_CUR as i64),
                ("SEEK_END", libc::SEEK_END as i64),
                ("O_RDONLY", libc::O_RDONLY as i64),
                ("O_WRONLY", libc::O_WRONLY as i64),
                ("O_RDWR", libc::O_RDWR as i64),
                ("O_APPEND", libc::O_APPEND as i64),
                ("O_CREAT", libc::O_CREAT as i64),
                ("O_EXCL", libc::O_EXCL as i64),
                ("O_TRUNC", libc::O_TRUNC as i64),
                ("O_NONBLOCK", libc::O_NONBLOCK as i64),
                ("O_CLOEXEC", libc::O_CLOEXEC as i64),
                ("WNOHANG", libc::WNOHANG as i64),
                ("EX_OK", 0),
            ];
            for (k, val) in consts {
                v.push((k, Value::Int(*val)));
            }
            // `environ` as a {bytes: bytes} dict — CPython's posix.environ is
            // bytes-keyed on Unix, and os wraps it in a str-decoding mapping.
            let environ = {
                let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
                for (k, val) in std::env::vars() {
                    let kb = k.clone().into_bytes();
                    let kv = h.alloc(PyObj::Bytes(kb.clone()));
                    let vv = h.alloc(PyObj::Bytes(val.into_bytes()));
                    d.insert(PKey::Bytes(kb), (kv, vv));
                }
                h.alloc(PyObj::Dict(d))
            };
            // `stat_result` as a type object. `os.stat()` already returns values
            // tagged with this type name; `pathlib` reaches the TYPE itself to
            // feature-test for platform-specific fields
            // (`hasattr(os.stat_result, 'st_flags')`).
            let stat_result = h.alloc(PyObj::Builtin("os.stat_result".into()));
            v.push(("stat_result", stat_result));
            v.push(("environ", environ));
            // Capability list `os` reads (empty = no optional dir-fd/etc. features).
            let have = h.new_list(vec![]);
            v.push(("_have_functions", have));
            v
        }),
        // `_thread` — low-level threading primitives. pythonrs runs user code on
        // one thread, so the locks are functional but uncontended.
        "_thread_core" => with_host(|h| {
            vec![
                (
                    "allocate_lock",
                    h.alloc(PyObj::Builtin("_thread_core.allocate_lock".into())),
                ),
                (
                    "RLock",
                    h.alloc(PyObj::Builtin("_thread_core.RLock".into())),
                ),
                (
                    "get_ident",
                    h.alloc(PyObj::Builtin("_thread_core.get_ident".into())),
                ),
                (
                    "get_native_id",
                    h.alloc(PyObj::Builtin("_thread_core.get_native_id".into())),
                ),
                (
                    "start_new_thread",
                    h.alloc(PyObj::Builtin("_thread_core.start_new_thread".into())),
                ),
                ("error", h.alloc(PyObj::Builtin("RuntimeError".into()))),
                ("TIMEOUT_MAX", Value::Float(f64::MAX)),
            ]
        }),
        // `itertools` — the iterator toolkit. Lazy iterators build an
        // `ItertoolsIter`; the combinatorics build eagerly.
        "itertools" => with_host(|h| {
            const FNS: &[&str] = &[
                "count",
                "repeat",
                "cycle",
                "chain",
                "accumulate",
                "starmap",
                "compress",
                "dropwhile",
                "takewhile",
                "filterfalse",
                "islice",
                "zip_longest",
                "pairwise",
                "batched",
                "product",
                "permutations",
                "combinations",
                "combinations_with_replacement",
                "tee",
                "groupby",
            ];
            FNS.iter()
                .map(|f| (*f, h.alloc(PyObj::Builtin(format!("itertools.{f}")))))
                .collect()
        }),
        // `_string` — the C helpers behind `string.Formatter`: the format-string
        // field parser and the field-name splitter.
        "_string" => with_host(|h| {
            vec![
                (
                    "formatter_parser",
                    h.alloc(PyObj::Builtin("_string.formatter_parser".into())),
                ),
                (
                    "formatter_field_name_split",
                    h.alloc(PyObj::Builtin("_string.formatter_field_name_split".into())),
                ),
            ]
        }),
        // `_typing` — the C accelerator `typing.py` builds on: the type-parameter
        // constructors (`TypeVar`/`ParamSpec`/`TypeVarTuple`), the `Generic` base,
        // the `Union` special form, `_idfunc`, and the `NoDefault` sentinel. The
        // rich type-system logic (`_GenericAlias`, `_SpecialForm`, `_type_check`)
        // stays in the vendored `typing.py`.
        "_typing" => with_host(|h| {
            let no_default = h.alloc(PyObj::Builtin("_typing.NoDefault".into()));
            vec![
                ("_idfunc", h.alloc(PyObj::Builtin("_typing._idfunc".into()))),
                ("TypeVar", h.alloc(PyObj::Builtin("_typing.TypeVar".into()))),
                (
                    "ParamSpec",
                    h.alloc(PyObj::Builtin("_typing.ParamSpec".into())),
                ),
                (
                    "TypeVarTuple",
                    h.alloc(PyObj::Builtin("_typing.TypeVarTuple".into())),
                ),
                (
                    "ParamSpecArgs",
                    h.alloc(PyObj::Builtin("_typing.ParamSpecArgs".into())),
                ),
                (
                    "ParamSpecKwargs",
                    h.alloc(PyObj::Builtin("_typing.ParamSpecKwargs".into())),
                ),
                (
                    "TypeAliasType",
                    h.alloc(PyObj::Builtin("_typing.TypeAliasType".into())),
                ),
                ("Generic", h.alloc(PyObj::Builtin("Generic".into()))),
                ("Union", h.alloc(PyObj::Builtin("_typing.Union".into()))),
                ("NoDefault", no_default),
            ]
        }),
        // `atexit` — cleanup callbacks run (LIFO) at interpreter shutdown. Native
        // because CPython's is a C module; the callbacks fire from `run_program`
        // after the top-level program finishes (see `run_atexit_callbacks`).
        "atexit" => with_host(|h| {
            vec![
                (
                    "register",
                    h.alloc(PyObj::Builtin("atexit.register".into())),
                ),
                (
                    "unregister",
                    h.alloc(PyObj::Builtin("atexit.unregister".into())),
                ),
                (
                    "_run_exitfuncs",
                    h.alloc(PyObj::Builtin("atexit._run_exitfuncs".into())),
                ),
                ("_clear", h.alloc(PyObj::Builtin("atexit._clear".into()))),
                (
                    "_ncallbacks",
                    h.alloc(PyObj::Builtin("atexit._ncallbacks".into())),
                ),
            ]
        }),
        // `time` — wall-clock, monotonic, and broken-down-time functions. The
        // calendar conversions (`gmtime`/`localtime`/`strftime`/`mktime`) delegate
        // to libc (the real C accelerator); `time()`/`monotonic()`/`sleep()` use
        // Rust's `std::time`. Native because CPython's `time` is a C module.
        "time" => with_host(|h| {
            const FNS: &[&str] = &[
                "time",
                "time_ns",
                "monotonic",
                "monotonic_ns",
                "perf_counter",
                "perf_counter_ns",
                "process_time",
                "process_time_ns",
                "sleep",
                "gmtime",
                "localtime",
                "mktime",
                "strftime",
                "struct_time",
                "asctime",
                "ctime",
            ];
            let mut out: Vec<(&str, Value)> = FNS
                .iter()
                .map(|f| (*f, h.alloc(PyObj::Builtin(format!("time.{f}")))))
                .collect();
            let (tz, alt, daylight, name_std, name_dst) = crate::builtins::tz_info();
            out.push(("timezone", Value::Int(tz)));
            out.push(("altzone", Value::Int(alt)));
            out.push(("daylight", Value::Int(daylight)));
            let n0 = h.new_str(name_std);
            let n1 = h.new_str(name_dst);
            let tzname = h.new_tuple(vec![n0, n1]);
            out.push(("tzname", tzname));
            out
        }),
        // `re` — regular expressions, backed natively by the Rust `regex` crate
        // (a linear-time NFA engine). Faithful for the common syntax
        // (`\d \w \s`, groups, named groups, alternation, anchors, quantifiers,
        // inline flags); features the engine lacks (backreferences, lookaround)
        // raise `re.error` at compile, as documented.
        "re" => with_host(|h| {
            const FNS: &[&str] = &[
                "compile",
                "match",
                "search",
                "fullmatch",
                "findall",
                "finditer",
                "sub",
                "subn",
                "split",
                "escape",
                "purge",
            ];
            let mut out: Vec<(&str, Value)> = FNS
                .iter()
                .map(|f| (*f, h.alloc(PyObj::Builtin(format!("re.{f}")))))
                .collect();
            // Flag constants (both long and short names).
            for (name, bit) in [
                ("IGNORECASE", 2i64),
                ("I", 2),
                ("LOCALE", 4),
                ("L", 4),
                ("MULTILINE", 8),
                ("M", 8),
                ("DOTALL", 16),
                ("S", 16),
                ("UNICODE", 32),
                ("U", 32),
                ("VERBOSE", 64),
                ("X", 64),
                ("ASCII", 256),
                ("A", 256),
                ("NOFLAG", 0),
            ] {
                out.push((name, Value::Int(bit)));
            }
            // `re.error` is a subclass of Exception; expose the class object.
            out.push(("error", h.alloc(PyObj::Builtin("re.error".into()))));
            // The `Pattern`/`Match` type objects (`isinstance(m, re.Match)`).
            out.push(("Pattern", h.alloc(PyObj::Builtin("re.Pattern".into()))));
            out.push(("Match", h.alloc(PyObj::Builtin("re.Match".into()))));
            out
        }),
        // `errno` — the platform error numbers (from libc) plus the `errorcode`
        // {number: name} map. A pure constants C-ext, correct natively on any
        // build.
        "errno" => with_host(|h| {
            let names: &[(&str, i32)] = &[
                ("EPERM", libc::EPERM),
                ("ENOENT", libc::ENOENT),
                ("ESRCH", libc::ESRCH),
                ("EINTR", libc::EINTR),
                ("EIO", libc::EIO),
                ("ENXIO", libc::ENXIO),
                ("E2BIG", libc::E2BIG),
                ("ENOEXEC", libc::ENOEXEC),
                ("EBADF", libc::EBADF),
                ("ECHILD", libc::ECHILD),
                ("EAGAIN", libc::EAGAIN),
                ("ENOMEM", libc::ENOMEM),
                ("EACCES", libc::EACCES),
                ("EFAULT", libc::EFAULT),
                ("EBUSY", libc::EBUSY),
                ("EEXIST", libc::EEXIST),
                ("EXDEV", libc::EXDEV),
                ("ENODEV", libc::ENODEV),
                ("ENOTDIR", libc::ENOTDIR),
                ("EISDIR", libc::EISDIR),
                ("EINVAL", libc::EINVAL),
                ("ENFILE", libc::ENFILE),
                ("EMFILE", libc::EMFILE),
                ("ENOTTY", libc::ENOTTY),
                ("EFBIG", libc::EFBIG),
                ("ENOSPC", libc::ENOSPC),
                ("ESPIPE", libc::ESPIPE),
                ("EROFS", libc::EROFS),
                ("EMLINK", libc::EMLINK),
                ("EPIPE", libc::EPIPE),
                ("EDOM", libc::EDOM),
                ("ERANGE", libc::ERANGE),
                ("EDEADLK", libc::EDEADLK),
                ("ENAMETOOLONG", libc::ENAMETOOLONG),
                ("ENOLCK", libc::ENOLCK),
                ("ENOSYS", libc::ENOSYS),
                ("ENOTEMPTY", libc::ENOTEMPTY),
                ("ELOOP", libc::ELOOP),
                ("EWOULDBLOCK", libc::EWOULDBLOCK),
                ("ENOMSG", libc::ENOMSG),
                ("EIDRM", libc::EIDRM),
                ("EOVERFLOW", libc::EOVERFLOW),
                ("EBADMSG", libc::EBADMSG),
                ("EILSEQ", libc::EILSEQ),
                ("ENOTSOCK", libc::ENOTSOCK),
                ("EDESTADDRREQ", libc::EDESTADDRREQ),
                ("EMSGSIZE", libc::EMSGSIZE),
                ("EPROTOTYPE", libc::EPROTOTYPE),
                ("ENOPROTOOPT", libc::ENOPROTOOPT),
                ("EPROTONOSUPPORT", libc::EPROTONOSUPPORT),
                ("EOPNOTSUPP", libc::EOPNOTSUPP),
                ("EAFNOSUPPORT", libc::EAFNOSUPPORT),
                ("EADDRINUSE", libc::EADDRINUSE),
                ("EADDRNOTAVAIL", libc::EADDRNOTAVAIL),
                ("ENETDOWN", libc::ENETDOWN),
                ("ENETUNREACH", libc::ENETUNREACH),
                ("ECONNABORTED", libc::ECONNABORTED),
                ("ECONNRESET", libc::ECONNRESET),
                ("ENOBUFS", libc::ENOBUFS),
                ("EISCONN", libc::EISCONN),
                ("ENOTCONN", libc::ENOTCONN),
                ("ETIMEDOUT", libc::ETIMEDOUT),
                ("ECONNREFUSED", libc::ECONNREFUSED),
                ("EHOSTUNREACH", libc::EHOSTUNREACH),
                ("EALREADY", libc::EALREADY),
                ("EINPROGRESS", libc::EINPROGRESS),
                ("ECANCELED", libc::ECANCELED),
                ("ENOTSUP", libc::ENOTSUP),
            ];
            let mut v: Vec<(&str, Value)> = Vec::with_capacity(names.len() + 1);
            let mut ec: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for (n, val) in names {
                v.push((n, Value::Int(*val as i64)));
                let key = PKey::Int(*val as i64);
                if !ec.contains_key(&key) {
                    let nv = h.new_str((*n).to_string());
                    ec.insert(key, (Value::Int(*val as i64), nv));
                }
            }
            let errorcode = h.alloc(PyObj::Dict(ec));
            v.push(("errorcode", errorcode));
            v
        }),
        "math" => with_host(|h| {
            // Every function implemented by `builtins::call_math`; each resolves to
            // a `math.<name>` builtin. Kept as a flat list so adding a function is a
            // one-line change here plus its arm in `call_math`.
            const MATH_FNS: &[&str] = &[
                "sqrt",
                "floor",
                "ceil",
                "fabs",
                "pow",
                "log",
                "log2",
                "log10",
                "log1p",
                "exp",
                "exp2",
                "expm1",
                "cbrt",
                "sin",
                "cos",
                "tan",
                "asin",
                "acos",
                "atan",
                "atan2",
                "sinh",
                "cosh",
                "tanh",
                "asinh",
                "acosh",
                "atanh",
                "degrees",
                "radians",
                "hypot",
                "trunc",
                "copysign",
                "fmod",
                "ldexp",
                "isqrt",
                "isnan",
                "isinf",
                "isfinite",
                "gcd",
                "factorial",
                "comb",
                "perm",
                "fsum",
                "sumprod",
                "prod",
                "lgamma",
                "gamma",
                "erf",
                "erfc",
                "isclose",
                "remainder",
            ];
            let mut out: Vec<(&str, Value)> = vec![
                ("pi", Value::Float(std::f64::consts::PI)),
                ("e", Value::Float(std::f64::consts::E)),
                ("tau", Value::Float(std::f64::consts::TAU)),
                ("inf", Value::Float(f64::INFINITY)),
                ("nan", Value::Float(f64::NAN)),
            ];
            for &f in MATH_FNS {
                out.push((f, h.alloc(PyObj::Builtin(format!("math.{f}")))));
            }
            out
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
            // `sys.__stdout__` / `__stderr__` / `__stdin__` — the original streams,
            // unaffected by a `sys.stdout` reassignment or `redirect_stdout`.
            let orig_stdout = h.alloc(PyObj::File { id: 0 });
            let orig_stderr = h.alloc(PyObj::File { id: 1 });
            let orig_stdin = h.alloc(PyObj::File { id: 2 });
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
            let exe_path = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let executable = h.new_str(exe_path.clone());
            let exec_path = h.new_str(exe_path.clone());
            // The install root: the binary's parent directory's parent, mirroring
            // CPython's `<prefix>/bin/python` layout.
            let prefix = h.new_str(
                std::path::Path::new(&exe_path)
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            let flags = {
                let mut a: NameMap = NameMap::default();
                for (k, v) in [
                    ("debug", 0),
                    ("inspect", 0),
                    ("interactive", 0),
                    ("optimize", 0),
                    ("dont_write_bytecode", 0),
                    ("no_user_site", 0),
                    ("no_site", 0),
                    ("ignore_environment", 0),
                    ("verbose", 0),
                    ("bytes_warning", 0),
                    ("quiet", 0),
                    ("hash_randomization", 0),
                    ("isolated", 0),
                    ("dev_mode", 0),
                    ("utf8_mode", 0),
                    ("safe_path", 0),
                    ("int_max_str_digits", -1),
                    ("warn_default_encoding", 0),
                    ("gil", 1),
                    ("thread_inherit_context", 1),
                    ("context_aware_warnings", 1),
                ] {
                    a.insert(k.to_string(), Value::Int(v));
                }
                h.alloc(PyObj::Namespace { attrs: a })
            };
            let modules = h.new_dict(IndexMap::new());
            // Publish the live sys.modules handle + seed it with already-imported
            // modules, so Python-level assignment/reads stay consistent with the
            // internal import cache.
            h.sys_modules = Some(modules.clone());
            let cached: Vec<(String, Value)> = h
                .modules
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, v) in cached {
                let kv = h.new_str(k.clone());
                if let Some(PyObj::Dict(d)) = h.get_mut(&modules) {
                    d.insert(PKey::Str(k), (kv, v));
                }
            }
            let version = h.new_str(format!("{PY_MAJOR}.{PY_MINOR}.{PY_MICRO} (pythonrs)"));
            let platform = h.new_str(py_platform());
            // `sys.implementation` — a SimpleNamespace describing the interpreter
            // (its type is what the faithful `types.py` binds as SimpleNamespace).
            let implementation = {
                let mut a: NameMap = NameMap::default();
                let name = h.new_str("pythonrs");
                let cache_tag = h.new_str(format!("pythonrs-{PY_MAJOR}{PY_MINOR}"));
                let hexversion = ((PY_MAJOR) << 24) | ((PY_MINOR) << 16) | ((PY_MICRO) << 8) | 0xf0;
                a.insert("name".to_string(), name);
                a.insert("cache_tag".to_string(), cache_tag);
                a.insert("version".to_string(), version_info.clone());
                a.insert("hexversion".to_string(), Value::Int(hexversion));
                h.alloc(PyObj::Namespace { attrs: a })
            };
            // `sys.hash_info` — the numeric-hash parameters. `fractions` and
            // `_pydecimal` hash a rational by reducing it modulo `modulus`, so these
            // are the actual constants CPython's `hash()` is defined against, not
            // decoration.
            let hash_info = {
                let algorithm = h.new_str("siphash13".to_string());
                struct_seq(
                    h,
                    "sys.hash_info",
                    vec![
                        ("width", Value::Int(64)),
                        // 2**61 - 1, the Mersenne prime CPython reduces against.
                        ("modulus", Value::Int(2_305_843_009_213_693_951)),
                        ("inf", Value::Int(314_159)),
                        ("nan", Value::Int(0)),
                        ("imag", Value::Int(1_000_003)),
                        ("algorithm", algorithm),
                        ("hash_bits", Value::Int(64)),
                        ("seed_bits", Value::Int(128)),
                        ("cutoff", Value::Int(0)),
                    ],
                )
            };
            // `sys.float_info` — IEEE-754 double limits, straight off Rust's `f64`
            // so they cannot drift from what arithmetic actually does. `statistics`
            // reads `mant_dig` and `max` to pick its summation strategy.
            let float_info = struct_seq(
                h,
                "sys.float_info",
                vec![
                    ("max", Value::Float(f64::MAX)),
                    ("max_exp", Value::Int(f64::MAX_EXP as i64)),
                    ("max_10_exp", Value::Int(f64::MAX_10_EXP as i64)),
                    ("min", Value::Float(f64::MIN_POSITIVE)),
                    ("min_exp", Value::Int(f64::MIN_EXP as i64)),
                    ("min_10_exp", Value::Int(f64::MIN_10_EXP as i64)),
                    ("dig", Value::Int(f64::DIGITS as i64)),
                    ("mant_dig", Value::Int(f64::MANTISSA_DIGITS as i64)),
                    ("epsilon", Value::Float(f64::EPSILON)),
                    ("radix", Value::Int(f64::RADIX as i64)),
                    // FLT_ROUNDS == 1: round to nearest.
                    ("rounds", Value::Int(1)),
                ],
            );
            // `sys.int_info` — CPython's bignum layout. pythonrs stores bignums as
            // `num-bigint` limbs rather than 30-bit digits, so these describe the
            // LIMITS code reads them for (`default_max_str_digits`), matching
            // CPython's values so `int(...)`/`str(...)` guards behave the same.
            let int_info = struct_seq(
                h,
                "sys.int_info",
                vec![
                    ("bits_per_digit", Value::Int(30)),
                    ("sizeof_digit", Value::Int(4)),
                    ("default_max_str_digits", Value::Int(4300)),
                    ("str_digits_check_threshold", Value::Int(640)),
                ],
            );
            // `sys.builtin_module_names` — modules compiled into the interpreter.
            // `os` reads this to pick the platform (`'posix' in ...`), so on a Unix
            // host it must contain `posix`.
            let builtin_module_names = {
                // One authoritative list, shared with `_imp.is_builtin`: a name
                // here MUST have a native arm behind it (see `pyimp`).
                let names = crate::stdlib::pyimp::BUILTIN_MODULES;
                let vals: Vec<Value> = names.iter().map(|n| h.new_str(*n)).collect();
                h.new_tuple(vals)
            };
            // `sys.stdlib_module_names` — a frozenset of every importable stdlib
            // module name. `traceback`'s "did you forget to import" hint and any
            // code doing "is this a stdlib module?" both read it.
            let stdlib_module_names = {
                let mut items = IndexMap::new();
                for n in stdlib_module_names() {
                    items.insert(PKey::Str(n.clone()), h.new_str(n.clone()));
                }
                h.new_frozenset(items)
            };
            vec![
                ("argv", argv),
                ("stdlib_module_names", stdlib_module_names),
                ("maxsize", Value::Int(i64::MAX)),
                ("version", version),
                ("version_info", version_info),
                ("implementation", implementation),
                ("builtin_module_names", builtin_module_names),
                ("platform", platform),
                ("path", path),
                ("modules", modules),
                ("executable", executable),
                ("stdout", stdout),
                ("stderr", stderr),
                ("stdin", stdin),
                ("__stdout__", orig_stdout),
                ("__stderr__", orig_stderr),
                ("__stdin__", orig_stdin),
                ("exit", h.alloc(PyObj::Builtin("sys.exit".into()))),
                (
                    "getrecursionlimit",
                    h.alloc(PyObj::Builtin("sys.getrecursionlimit".into())),
                ),
                (
                    "setrecursionlimit",
                    h.alloc(PyObj::Builtin("sys.setrecursionlimit".into())),
                ),
                ("_getframe", h.alloc(PyObj::Builtin("sys._getframe".into()))),
                (
                    "getfilesystemencoding",
                    h.alloc(PyObj::Builtin("sys.getfilesystemencoding".into())),
                ),
                (
                    "getfilesystemencodeerrors",
                    h.alloc(PyObj::Builtin("sys.getfilesystemencodeerrors".into())),
                ),
                (
                    "getdefaultencoding",
                    h.alloc(PyObj::Builtin("sys.getdefaultencoding".into())),
                ),
                ("intern", h.alloc(PyObj::Builtin("sys.intern".into()))),
                // Exception reporting. `threading` captures `sys.excepthook` and
                // `sys.exc_info` when a `Thread` is constructed, so every
                // `Thread(...)` reaches these.
                (
                    "excepthook",
                    h.alloc(PyObj::Builtin("sys.excepthook".into())),
                ),
                (
                    "__excepthook__",
                    h.alloc(PyObj::Builtin("sys.excepthook".into())),
                ),
                (
                    "unraisablehook",
                    h.alloc(PyObj::Builtin("sys.unraisablehook".into())),
                ),
                ("exc_info", h.alloc(PyObj::Builtin("sys.exc_info".into()))),
                ("exception", h.alloc(PyObj::Builtin("sys.exception".into()))),
                ("audit", h.alloc(PyObj::Builtin("sys.audit".into()))),
                (
                    "is_finalizing",
                    h.alloc(PyObj::Builtin("sys.is_finalizing".into())),
                ),
                // Installation layout. `argparse` reads `base_prefix` (to detect a
                // venv) at import time, so its absence made the module
                // unimportable. There is no separate base install here — pythonrs
                // is one binary — so all four point at the runtime's own root.
                ("prefix", prefix.clone()),
                ("base_prefix", prefix.clone()),
                ("exec_prefix", prefix.clone()),
                ("base_exec_prefix", prefix),
                ("dont_write_bytecode", Value::Bool(false)),
                (
                    "byteorder",
                    h.new_str(
                        if cfg!(target_endian = "big") {
                            "big"
                        } else {
                            "little"
                        }
                        .to_string(),
                    ),
                ),
                // `sys.flags` — the command-line/environment switches. Read at
                // import time by `_py_warnings` (`flags.context_aware_warnings`),
                // so `warnings` and everything importing it needs the whole bag,
                // not just the flag it happens to read. Values reflect how this
                // runtime actually behaves: no -O, no -B, no isolation.
                ("flags", flags),
                ("warnoptions", h.new_list(Vec::new())),
                ("hexversion", Value::Int(0x030E_00F0)),
                ("hash_info", hash_info),
                ("float_info", float_info),
                ("int_info", int_info),
                ("api_version", Value::Int(1013)),
                ("_base_executable", exec_path),
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
        // `_collections` — the C container accelerators. Exposing the native
        // `deque`/`defaultdict`/`OrderedDict` here lets the FULL vendored
        // `collections/__init__.py` run (rather than a native subset), so
        // `ChainMap`, `Counter`, `UserDict`/`UserList`/`UserString`, and
        // `namedtuple` all come from the faithful pure-Python source. The other
        // `_collections` helpers (`_tuplegetter`, `_count_elements`,
        // `_deque_iterator`) have pure-Python fallbacks in `collections`.
        // `_contextvars` — the C accelerator behind `contextvars.py` (PEP 567).
        // `_py_warnings` builds a `ContextVar` at import time, so `warnings` and
        // everything that imports it (`traceback`, …) needs this to exist.
        "_contextvars" => with_host(|h| {
            vec![
                (
                    "ContextVar",
                    h.alloc(PyObj::Builtin("_contextvars.ContextVar".into())),
                ),
                (
                    "Token",
                    h.alloc(PyObj::Builtin("_contextvars.Token".into())),
                ),
                (
                    "Context",
                    h.alloc(PyObj::Builtin("_contextvars.Context".into())),
                ),
                (
                    "copy_context",
                    h.alloc(PyObj::Builtin("_contextvars.copy_context".into())),
                ),
            ]
        }),
        "_collections" => with_host(|h| {
            vec![
                ("deque", h.alloc(PyObj::Builtin("collections.deque".into()))),
                (
                    "defaultdict",
                    h.alloc(PyObj::Builtin("collections.defaultdict".into())),
                ),
                (
                    "OrderedDict",
                    h.alloc(PyObj::Builtin("collections.OrderedDict".into())),
                ),
            ]
        }),
        _ => {
            // Native build (`--no-default-features`): the vendored CPython stdlib
            // `.py` shipped in `pylib/` is the ONLY source — compiled and executed
            // on pythonrs's OWN interpreter, no libpython. This is the endgame
            // path; `brew install pythonrs` lays `pylib/` down beside the binary
            // and CPython is never in the dependency graph.
            #[cfg(not(feature = "stdlib-ffi"))]
            {
                return match try_import_vendored(name) {
                    Some(res) => res,
                    None => Err(format!("ModuleNotFoundError: No module named '{name}'")),
                };
            }
            // Default build: the `stdlib-ffi` bridge (libpython) stays primary so
            // behavior is drop-in while the native C-accelerator floor
            // (`posix`/`_io`/`_sre`/…) that the vendored `.py` needs is still being
            // built out. The vendored path is exercised by the native build.
            #[cfg(feature = "stdlib-ffi")]
            {
                // The program's OWN modules run on pythonrs, never over the
                // bridge: a sibling `.py` executed by CPython would hand back
                // `Foreign` classes, and a user exception class defined that way
                // is not a class pythonrs can `raise`. Resolved from pythonrs's
                // `sys.path`, which CPython's importer cannot see.
                if let Some(res) = try_import_user_path(name) {
                    return res;
                }
                // CPython's importer searches the EMBEDDED `sys.path`, which knows
                // nothing about the running program. Hand it pythonrs's own
                // `sys.path` anyway so an installed third-party package reachable
                // only through a program-inserted path still resolves.
                crate::ffi::queue_search_paths(current_search_paths());
                // …and the program's arguments. `argparse`, `getopt`, `pdb` and
                // `unittest` all read `sys.argv` on the CPython side, where it is
                // `['']` unless it is mirrored across.
                crate::ffi::queue_argv(current_argv());
                let id = crate::ffi::import(name)?;
                return Ok(with_host(|h| h.alloc(PyObj::Foreign(id))));
            }
        }
    };
    Ok(with_host(|h| {
        let mut ns = NameMap::default();
        seed_module_dunders(h, &mut ns, name);
        for (k, v) in entries {
            ns.insert(k.to_string(), v);
        }
        let slot = h.new_module_slot(ns);
        h.alloc(PyObj::Module {
            name: name.to_string(),
            slot,
        })
    }))
}

// ── vendored stdlib importer (`pylib/`) ──────────────────────────────────────
// pythonrs ships CPython's pure-Python stdlib `.py` files under `pylib/` and runs
// them on its OWN lexer/parser/compiler/fusevm — no libpython. This is what makes
// pythonrs a real Python implementation rather than a pyo3 wrapper.

/// Locate the vendored stdlib root (`pylib/`). Search order:
///   1. `$PYTHONRS_LIB` (an explicit override / install-time path).
///   2. `<exe_dir>/../lib/pythonrs/pylib` and `<exe_dir>/pylib` (install layout,
///      e.g. what a Homebrew formula lays down beside the binary).
///   3. `<CARGO_MANIFEST_DIR>/pylib` (the in-repo tree, for `cargo run`/tests).
///
/// Compiled into both builds: only the native build IMPORTS from this tree, but
/// `stdlib_module_names` enumerates it either way — the ffi build serves the
/// same module names from CPython, so the listing stays truthful there too.
fn pylib_dir() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("PYTHONRS_LIB") {
        let p = std::path::PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [
                dir.join("../lib/pythonrs/pylib"),
                dir.join("../pylib"),
                dir.join("pylib"),
            ] {
                if cand.is_dir() {
                    return Some(cand);
                }
            }
        }
    }
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pylib");
    dev.is_dir().then_some(dev)
}

/// Third-party package root: `~/.pythonrs/pip` (where pip-installed pure-Python
/// packages live). Searched AFTER the vendored stdlib so a package can never
/// shadow a stdlib module, mirroring CPython's stdlib-before-site-packages order.
#[cfg(not(feature = "stdlib-ffi"))]
fn pip_dir() -> Option<std::path::PathBuf> {
    let d = dirs::home_dir()?.join(".pythonrs").join("pip");
    d.is_dir().then_some(d)
}

/// The stdlib modules that exist ONLY as a native arm of `import_module_inner` —
/// CPython ships each of these as a C extension with no Python source, so no
/// `pylib/` file names them and (unlike `itertools`) they are not
/// `sys.builtin_module_names` entries either. Mirrors those arms; the
/// `stdlib_module_names_all_import` test fails if an entry stops resolving.
const NATIVE_ONLY_MODULES: &[&str] = &[
    "_blake2",
    "_collections",
    "_csv",
    "_md5",
    "_random",
    "_sha1",
    "_sha2",
    "_sha3",
    "_signal",
    "_string",
    "math",
];

/// Names that live in the stdlib tree but that CPython deliberately keeps OUT of
/// `sys.stdlib_module_names`. Two groups, both from CPython's generator
/// (`Tools/build/generate_stdlib_module_names.py`):
///
///   * its `IGNORE` set — the frozen-module and C-API test fixtures, which are
///     shipped but are not stdlib API;
///   * files that are not in CPython's `Lib/` source tree at all and only appear
///     in an INSTALLED layout (the build-generated `_sysconfigdata_*` and the
///     `sitecustomize`/`usercustomize` site hooks). The generator never sees
///     them, so the shipped table never lists them.
fn ignored_stdlib_module(name: &str) -> bool {
    const IGNORE: &[&str] = &[
        "__hello__",
        "__hello_alias__",
        "__hello_only__",
        "__phello__",
        "__phello_alias__",
        "_ctypes_test",
        "_testbuffer",
        "_testcapi",
        "_testclinic",
        "_testconsole",
        "_testimportmultiple",
        "_testinternalcapi",
        "_testmultiphase",
        "_testsinglephase",
        "_xxtestfuzz",
        "sitecustomize",
        "test",
        "usercustomize",
        "xxlimited",
        "xxlimited_35",
        "xxsubtype",
    ];
    IGNORE.contains(&name) || name.starts_with("_sysconfigdata_")
}

/// `sys.stdlib_module_names` — every top-level module name this interpreter can
/// import. CPython ships it as a generated static table; pythonrs COMPUTES it
/// from the three places a stdlib module can actually come from (the native
/// builtins, the native-only arms above, and the bundled `pylib/` tree), so the
/// set can never advertise a module the interpreter would fail to import.
///
/// `traceback`'s NameError hint reads it: an unbound name that IS a stdlib
/// module gets "Did you forget to import 'x'?" instead of a bare NameError.
pub fn stdlib_module_names() -> &'static std::collections::BTreeSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut out: std::collections::BTreeSet<String> = crate::stdlib::pyimp::BUILTIN_MODULES
            .iter()
            .chain(NATIVE_ONLY_MODULES.iter())
            .map(|s| (*s).to_string())
            .collect();
        // A `pylib/` entry is a module either as `<name>.py` or as a package
        // directory holding `__init__.py`; anything else in the tree (a data
        // file, a test fixture) is not importable and must not be listed.
        if let Some(root) = pylib_dir() {
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    let p = e.path();
                    let stem = match p.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    let importable = if p.is_dir() {
                        p.join("__init__.py").is_file()
                    } else {
                        p.extension().and_then(|s| s.to_str()) == Some("py")
                    };
                    if importable && !ignored_stdlib_module(stem) {
                        out.insert(stem.to_string());
                    }
                }
            }
        }
        out
    })
}

/// Resolve a dotted module name to a source file on the search path, if present:
/// `json` → `<root>/json.py` or `<root>/json/__init__.py`; `os.path` →
/// `<root>/os/path.py` or `<root>/os/path/__init__.py`. Roots are searched in
/// order: the vendored stdlib (`pylib/`) first, then `~/.pythonrs/pip`.
#[cfg(not(feature = "stdlib-ffi"))]
fn resolve_vendored_path(name: &str) -> Option<std::path::PathBuf> {
    let rel = name.replace('.', "/");
    for root in [pylib_dir(), pip_dir()].into_iter().flatten() {
        let module_file = root.join(format!("{rel}.py"));
        if module_file.is_file() {
            return Some(module_file);
        }
        let package_init = root.join(&rel).join("__init__.py");
        if package_init.is_file() {
            return Some(package_init);
        }
    }
    None
}

/// Try to import `name` from the vendored stdlib. `None` = no such `.py` file
/// (caller tries the next resolver); `Some(Ok)` = ran on pythonrs and produced a
/// native module; `Some(Err)` = the file exists but failed to execute.
#[cfg(not(feature = "stdlib-ffi"))]
fn try_import_vendored(name: &str) -> Option<Result<Value, String>> {
    let path = resolve_vendored_path(name)?;
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Some(Err(format!(
                "ImportError: cannot read {}: {e}",
                path.display()
            )))
        }
    };
    Some(run_vendored_module(name, &src, &path))
}

/// Execute a vendored module's source in a fresh namespace (its `__dict__`) at
/// module scope, then package the resulting globals as a native `PyObj::Module`.
/// The caller's globals and frame stack are saved and restored around the run.
///
/// Compiled into both builds: the `pylib/` tree is native-only, but `_thread` is
/// served from `stdlib::pythread`'s source in the ffi build too.
fn run_vendored_module(name: &str, src: &str, path: &std::path::Path) -> Result<Value, String> {
    // Park the importer's frames so the module body runs at a clean module scope
    // (its `def`/`class`/assignments become module globals), and allocate a fresh,
    // PERSISTENT globals slot for it — functions defined here capture this slot's
    // id and keep resolving their globals through it after import completes.
    let parked = with_host(|h| h.enter_module_scope());
    let (mid, saved_mod) = with_host(|h| {
        let name_v = h.new_str(name);
        let file_v = h.new_str(path.to_string_lossy());
        // `__package__` anchors relative imports (`from . import ...`). A package
        // (`__init__.py`) is its own anchor; a plain module `a.b` anchors on its
        // parent `a`.
        let is_package = path.file_name().is_some_and(|f| f == "__init__.py");
        let package = if is_package {
            name.to_string()
        } else {
            match name.rsplit_once('.') {
                Some((parent, _)) => parent.to_string(),
                None => String::new(),
            }
        };
        let package_v = h.new_str(&package);
        // Seed the module dunders CPython sets before executing the body.
        let mut ns: NameMap = NameMap::default();
        ns.insert("__name__".to_string(), name_v);
        ns.insert("__file__".to_string(), file_v);
        ns.insert("__package__".to_string(), package_v);
        ns.insert("__doc__".to_string(), Value::Undef);
        let mid = h.new_module_slot(ns);
        let saved = h.swap_module(mid);
        (mid, saved)
    });

    // Create the module object and cache it NOW — before running the body — so a
    // circular import during the body (os <-> posixpath) resolves to this same,
    // partially-populated object instead of re-running the body forever.
    let module = with_host(|h| {
        let m = h.alloc(PyObj::Module {
            name: name.to_string(),
            slot: mid,
        });
        h.cache_module(name, m.clone());
        m
    });

    let run = (|| -> Result<(), String> {
        // Label the cached entry with the module's own path so `--cacheview`
        // attributes it to the module, not the `<string>`/script that imported it.
        let prog = crate::compile_or_load_labeled(src, &path.to_string_lossy())?;
        let chunk = crate::load_merged(prog);
        run_chunk_on(chunk)?;
        Ok(())
    })();

    with_host(|h| {
        h.swap_module(saved_mod);
        h.restore_scope(parked);
        if run.is_ok() {
            // Nothing to copy: the module object points AT slot `mid`, so every
            // holder — including a mid-import circular reference — already sees
            // the namespace the body built, and keeps seeing later writes to it.
            //
            // A body IS allowed to replace itself, though: `decimal.py` ends with
            // `sys.modules[__name__] = _pydecimal`, handing its own name to the
            // pure-Python implementation. Honour that rebinding, or `import
            // decimal` resolves to the shell whose body just disowned it.
        } else {
            // The body failed: drop the half-built module so a retry re-runs it
            // (and re-raises) instead of resolving to a broken cached shell that
            // silently masks the failure of whatever this module imported.
            h.uncache_module(name);
        }
    });
    run?;
    // A body is allowed to replace itself: `decimal.py` ends with
    // `sys.modules[__name__] = _pydecimal`, handing its own name to the
    // pure-Python implementation. Return whatever `sys.modules` names now, or the
    // caller re-caches the shell whose body just disowned it and `from decimal
    // import Decimal` finds an empty module.
    Ok(with_host(|h| h.sys_module_entry(name)).unwrap_or(module))
}

// ── file / I/O side table (ported from rubylang's `IoCell`) ──────────────────

/// The `OSError` subclass CPython maps an errno to (`_PyExc_CreateExceptionObject`'s
/// table in `Objects/exceptions.c`). Anything unmapped stays a plain `OSError`.
fn errno_exc_class(eno: i32) -> &'static str {
    match eno {
        1 => "PermissionError",   // EPERM
        2 => "FileNotFoundError", // ENOENT
        3 => "ProcessLookupError",
        4 => "InterruptedError",
        11 => "BlockingIOError", // EAGAIN
        13 => "PermissionError",
        17 => "FileExistsError",
        20 => "NotADirectoryError",
        21 => "IsADirectoryError",
        32 => "BrokenPipeError",
        10 => "ChildProcessError",
        _ => "OSError",
    }
}

/// The `strerror` text for an errno, falling back to the Rust error's own
/// `Display` when the platform gives nothing.
fn errno_strerror(eno: i32, e: &std::io::Error) -> String {
    let raw = e.to_string();
    // Rust renders `<strerror> (os error N)`; CPython's `strerror` is the first
    // part alone.
    match raw.split_once(" (os error ") {
        Some((s, _)) => s.to_string(),
        None => {
            let _ = eno;
            raw
        }
    }
}

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
    #[allow(clippy::too_many_arguments)]
    pub fn io_alloc_file(
        &mut self,
        file: std::fs::File,
        path: String,
        mode: String,
        readable: bool,
        writable: bool,
        encoding: TextEncoding,
        newline_translate: bool,
    ) -> Value {
        let id = self.io_handles.len() as u32;
        self.io_handles.push(IoCell::File {
            file: Some(file),
            path,
            mode,
            readable,
            writable,
            encoding,
            newline_translate,
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

    /// The CPython class name of a file handle: a text-mode handle (and every
    /// standard stream) is a `TextIOWrapper`; a binary-mode one is a
    /// `BufferedReader` / `BufferedWriter` / `BufferedRandom` depending on which
    /// directions it was opened for.
    pub fn file_class_name(&self, id: u32) -> &'static str {
        match self.io_handles.get(id as usize) {
            Some(IoCell::File {
                mode,
                readable,
                writable,
                ..
            }) if mode.contains('b') => match (readable, writable) {
                (true, true) => "BufferedRandom",
                (true, false) => "BufferedReader",
                _ => "BufferedWriter",
            },
            _ => "TextIOWrapper",
        }
    }

    /// `f.name` — the path for a real file, the bracketed stream name otherwise.
    pub fn io_name(&self, id: u32) -> String {
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) => "<stdout>".into(),
            Some(IoCell::Stderr) => "<stderr>".into(),
            Some(IoCell::Stdin) => "<stdin>".into(),
            Some(IoCell::File { path, .. }) => path.clone(),
            None => String::new(),
        }
    }

    /// `f.mode` — the mode string exactly as it was passed to `open`.
    pub fn io_mode(&self, id: u32) -> String {
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) | Some(IoCell::Stderr) => "w".into(),
            Some(IoCell::Stdin) => "r".into(),
            Some(IoCell::File { mode, .. }) => mode.clone(),
            None => String::new(),
        }
    }

    /// Whether the handle was opened in BINARY mode (`'b'` anywhere in the mode
    /// string, as CPython's `_io.open` parses it).
    ///
    /// Every read path used to decode UTF-8 unconditionally, so a binary handle
    /// answered a `str` — `type(open(p, 'rb').read())` was `str` — and a file
    /// holding a byte that is not valid UTF-8 failed outright with
    /// `OSError: stream did not contain valid UTF-8` where CPython returns the
    /// bytes.
    pub fn io_is_binary(&self, id: u32) -> bool {
        match self.io_handles.get(id as usize) {
            Some(IoCell::File { mode, .. }) => mode.contains('b'),
            _ => false,
        }
    }

    /// `f.read(n)` on a binary handle — the next `n` raw BYTES, or all of them
    /// for `None`/negative `n`. No decoding, so a byte sequence round-trips.
    pub fn io_read_n_bytes(&mut self, id: u32, n: Option<i64>) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let want = match n {
            None => None,
            Some(k) if k < 0 => None,
            Some(k) => Some(k as usize),
        };
        let mut buf: Vec<u8> = Vec::new();
        let res = match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File {
                file: Some(f),
                readable: true,
                ..
            }) => match want {
                None => f.read_to_end(&mut buf).map(|_| ()),
                Some(k) => {
                    buf.resize(k, 0);
                    read_up_to(f, &mut buf)
                }
            },
            Some(IoCell::File { file: Some(_), .. }) => return Err(unsupported_read()),
            Some(IoCell::File { file: None, .. }) => return Err(closed_err()),
            Some(IoCell::Stdin) => match want {
                None => std::io::stdin().read_to_end(&mut buf).map(|_| ()),
                Some(k) => {
                    buf.resize(k, 0);
                    read_up_to(&mut std::io::stdin(), &mut buf)
                }
            },
            _ => return Err(unsupported_read()),
        };
        res.map_err(io_err)?;
        Ok(buf)
    }

    /// `f.readline()` on a binary handle — bytes up to and including the `\n`.
    pub fn io_readline_bytes(&mut self, id: u32) -> Result<Vec<u8>, String> {
        let mut buf: Vec<u8> = Vec::new();
        while let Some(b) = self.io_read_byte(id)? {
            buf.push(b);
            if b == b'\n' {
                break;
            }
        }
        Ok(buf)
    }

    /// `f.readlines()` / iteration on a binary handle.
    pub fn io_read_lines_bytes(&mut self, id: u32) -> Result<Vec<Vec<u8>>, String> {
        let all = self.io_read_n_bytes(id, None)?;
        Ok(all
            .split_inclusive(|b| *b == b'\n')
            .map(<[u8]>::to_vec)
            .collect())
    }

    /// `f.readable()` / `f.writable()` — the directions the handle was opened
    /// for. A standard stream reports the direction it actually carries.
    pub fn io_dirs(&self, id: u32) -> (bool, bool) {
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) | Some(IoCell::Stderr) => (false, true),
            Some(IoCell::Stdin) => (true, false),
            Some(IoCell::File {
                readable, writable, ..
            }) => (*readable, *writable),
            None => (false, false),
        }
    }

    /// `f.read(n)` — the next `n` *characters* (text mode), decoded from UTF-8.
    /// Reads one whole character at a time (a lead byte plus its continuation
    /// bytes), so a multi-byte character is never split and the stream position
    /// lands exactly after the nth character — matching CPython's text-mode
    /// `read`, whose `tell` after `read(2)` of `"héllo"` is byte 3.
    /// `None`/negative `n` reads to EOF.
    pub fn io_read_n(&mut self, id: u32, n: Option<i64>) -> Result<String, String> {
        let want = match n {
            None => return self.io_read_all(id),
            Some(k) if k < 0 => return self.io_read_all(id),
            Some(k) => k as usize,
        };
        let single_byte = !matches!(
            self.io_handles.get(id as usize),
            Some(IoCell::File {
                encoding: TextEncoding::Utf8,
                ..
            })
        );
        let mut buf: Vec<u8> = Vec::new();
        let mut taken = 0usize;
        while taken < want {
            let Some(lead) = self.io_read_byte(id)? else {
                break;
            };
            taken += 1;
            buf.push(lead);
            if single_byte {
                continue;
            }
            // UTF-8: the lead byte's high bits give the character's total length.
            let extra = match lead {
                0x00..=0x7F => 0,
                0xC0..=0xDF => 1,
                0xE0..=0xEF => 2,
                0xF0..=0xF7 => 3,
                // A stray continuation byte is not a lead byte; take it alone
                // and let the lossy decode below replace it.
                _ => 0,
            };
            for _ in 0..extra {
                match self.io_read_byte(id)? {
                    Some(b) => buf.push(b),
                    None => break,
                }
            }
        }
        let mut text = self.text_decode(id, &buf);
        // A translated `\r\n` cost two reads and yields one character, so the
        // result can be short; take more until it is not, or the file ends.
        while text.chars().count() < want {
            let Some(b) = self.io_read_byte(id)? else {
                break;
            };
            buf.push(b);
            if !single_byte {
                let extra = match b {
                    0x00..=0x7F => 0,
                    0xC0..=0xDF => 1,
                    0xE0..=0xEF => 2,
                    0xF0..=0xF7 => 3,
                    _ => 0,
                };
                for _ in 0..extra {
                    match self.io_read_byte(id)? {
                        Some(x) => buf.push(x),
                        None => break,
                    }
                }
            }
            text = self.text_decode(id, &buf);
        }
        Ok(text)
    }

    /// One byte from a readable handle; `None` at EOF. The shared step behind
    /// `io_readline` and `io_read_n`.
    fn io_read_byte(&mut self, id: u32) -> Result<Option<u8>, String> {
        use std::io::Read;
        let mut one = [0u8; 1];
        let read = match self.io_handles.get_mut(id as usize) {
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
        match read {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(one[0])),
            Err(e) => Err(io_err(e)),
        }
    }

    /// `f.tell()` — the current byte offset (CPython's text-mode cookie is the
    /// byte position for an unbuffered, non-translating stream like ours).
    pub fn io_tell(&mut self, id: u32) -> Result<i64, String> {
        use std::io::Seek;
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File { file: Some(f), .. }) => {
                f.stream_position().map(|p| p as i64).map_err(io_err)
            }
            Some(IoCell::File { file: None, .. }) => Err(closed_err()),
            _ => Err("OSError: [Errno 29] Illegal seek".into()),
        }
    }

    /// `f.seek(offset, whence)` — returns the new absolute position.
    pub fn io_seek(&mut self, id: u32, offset: i64, whence: i64) -> Result<i64, String> {
        use std::io::{Seek, SeekFrom};
        let from = match whence {
            0 => SeekFrom::Start(offset.max(0) as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => {
                return Err(format!(
                    "ValueError: invalid whence ({whence}, should be 0, 1 or 2)"
                ))
            }
        };
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File { file: Some(f), .. }) => {
                f.seek(from).map(|p| p as i64).map_err(io_err)
            }
            Some(IoCell::File { file: None, .. }) => Err(closed_err()),
            _ => Err("OSError: [Errno 29] Illegal seek".into()),
        }
    }

    /// `f.fileno()` — the underlying OS descriptor.
    pub fn io_fileno(&self, id: u32) -> Result<i64, String> {
        #[cfg(unix)]
        use std::os::unix::io::AsRawFd;
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) => Ok(1),
            Some(IoCell::Stderr) => Ok(2),
            Some(IoCell::Stdin) => Ok(0),
            #[cfg(unix)]
            Some(IoCell::File { file: Some(f), .. }) => Ok(f.as_raw_fd() as i64),
            Some(IoCell::File { file: None, .. }) => Err(closed_err()),
            #[allow(unreachable_patterns)]
            _ => Err("io.UnsupportedOperation: fileno".into()),
        }
    }

    /// `f.truncate(size)` — cut the file to `size` (default: current position).
    pub fn io_truncate(&mut self, id: u32, size: Option<i64>) -> Result<i64, String> {
        let at = match size {
            Some(n) => n,
            None => self.io_tell(id)?,
        };
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File {
                file: Some(f),
                writable: true,
                ..
            }) => f.set_len(at.max(0) as u64).map(|_| at).map_err(io_err),
            Some(IoCell::File { file: Some(_), .. }) => Err(unsupported_write()),
            Some(IoCell::File { file: None, .. }) => Err(closed_err()),
            _ => Err("io.UnsupportedOperation: truncate".into()),
        }
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
                file, path, mode, ..
            }) => {
                let closed = if file.is_none() { " (closed)" } else { "" };
                // A binary-mode handle is a `Buffered*` object in CPython: no
                // `mode=`/`encoding=` in its repr, and the class name reflects
                // whether it reads, writes, or both.
                if mode.contains('b') {
                    let cls = self.file_class_name(id);
                    format!("<_io.{cls} name='{path}'{closed}>")
                } else {
                    format!(
                        "<_io.TextIOWrapper name='{path}' mode='{mode}' encoding='UTF-8'{closed}>"
                    )
                }
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
        let bytes = self.text_encode(id, s)?;
        self.io_write_bytes(id, &bytes)?;
        Ok(Value::Int(s.chars().count() as i64))
    }

    // ── output capture ───────────────────────────────────────────────────
    //
    // Every write a *program* makes to a native stream funnels through
    // `write_out`: `print`, `sys.stdout.write`, the interactive displayhook, and
    // the `input()` prompt. Diagnostics the runtime itself emits (the banner, a
    // crash traceback from `main`) deliberately do not — they belong to the
    // process, not to the program.

    /// Start capturing program output in-process. Any text already captured is
    /// discarded, so each run starts clean.
    pub fn begin_capture(&mut self) {
        self.capture = Some(String::new());
    }

    /// Stop capturing and take everything written since [`begin_capture`],
    /// returning the empty string when capture was not on.
    ///
    /// [`begin_capture`]: PyHost::begin_capture
    pub fn end_capture(&mut self) -> String {
        self.capture.take().unwrap_or_default()
    }

    /// Whether output is being captured.
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// Write program output: into the capture buffer when capturing, else to the
    /// native stream `stderr` selects. `s` is written verbatim — `print` has
    /// already applied `sep`/`end`.
    pub fn write_out(&mut self, s: &str, stderr: bool) {
        if let Some(buf) = &mut self.capture {
            buf.push_str(s);
            return;
        }
        use std::io::Write;
        if stderr {
            let mut o = std::io::stderr();
            let _ = o.write_all(s.as_bytes());
            let _ = o.flush();
        } else {
            let mut o = std::io::stdout();
            let _ = o.write_all(s.as_bytes());
            let _ = o.flush();
        }
    }

    /// `f.write(...)` at the byte layer — returns the number of bytes written.
    pub fn io_write_bytes(&mut self, id: u32, bytes: &[u8]) -> Result<Value, String> {
        use std::io::Write;
        // The standard streams route through `write_out` (so an embedder's
        // capture catches them), which needs `&mut self` of its own — hence the
        // early return rather than an arm inside the `io_handles` borrow below.
        match self.io_handles.get(id as usize) {
            Some(IoCell::Stdout) | Some(IoCell::Stderr) => {
                let stderr = matches!(self.io_handles.get(id as usize), Some(IoCell::Stderr));
                self.write_out(&String::from_utf8_lossy(bytes), stderr);
                return Ok(Value::Int(bytes.len() as i64));
            }
            _ => {}
        }
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::Stdout) | Some(IoCell::Stderr) => unreachable!("handled above"),
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

    /// How a text handle turns bytes into `str`: its `encoding=`, then universal
    /// newlines when it was opened with the default `newline=None`.
    fn text_decode(&self, id: u32, bytes: &[u8]) -> String {
        let (encoding, translate) = match self.io_handles.get(id as usize) {
            Some(IoCell::File {
                encoding,
                newline_translate,
                ..
            }) => (*encoding, *newline_translate),
            // A standard stream is UTF-8 and translates, as CPython's are.
            _ => (TextEncoding::Utf8, true),
        };
        let text = encoding.decode(bytes);
        if translate {
            translate_newlines(&text)
        } else {
            text
        }
    }

    /// The reverse: `str` into the bytes this handle's `encoding=` calls for.
    fn text_encode(&self, id: u32, s: &str) -> Result<Vec<u8>, String> {
        match self.io_handles.get(id as usize) {
            Some(IoCell::File { encoding, .. }) => encoding.encode(s),
            _ => Ok(s.as_bytes().to_vec()),
        }
    }

    /// `f.read()` — the remaining contents as a string.
    pub fn io_read_all(&mut self, id: u32) -> Result<String, String> {
        use std::io::Read;
        let mut bytes: Vec<u8> = Vec::new();
        match self.io_handles.get_mut(id as usize) {
            Some(IoCell::File {
                file: Some(f),
                readable: true,
                ..
            }) => {
                f.read_to_end(&mut bytes).map_err(io_err)?;
            }
            Some(IoCell::File { file: Some(_), .. }) => return Err(unsupported_read()),
            Some(IoCell::File { file: None, .. }) => return Err(closed_err()),
            Some(IoCell::Stdin) => {
                std::io::stdin().read_to_end(&mut bytes).map_err(io_err)?;
            }
            _ => return Err(unsupported_read()),
        }
        Ok(self.text_decode(id, &bytes))
    }

    /// `f.readline()` — one line up to and including `\n` (or EOF); "" at EOF.
    pub fn io_readline(&mut self, id: u32) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        while let Some(b) = self.io_read_byte(id)? {
            buf.push(b);
            if b == b'\n' {
                break;
            }
        }
        Ok(self.text_decode(id, &buf))
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
pub fn open_file(
    path: &str,
    mode: &str,
    encoding: Option<&str>,
    newline: Option<&str>,
) -> Result<Value, String> {
    use std::fs::OpenOptions;
    let binary = mode.contains('b');
    // CPython rejects both arguments on a binary handle before it opens anything.
    if binary {
        if encoding.is_some() {
            return Err("ValueError: binary mode doesn't take an encoding argument".to_string());
        }
        if newline.is_some() {
            return Err("ValueError: binary mode doesn't take a newline argument".to_string());
        }
    }
    let encoding = match encoding {
        None => TextEncoding::Utf8,
        Some(name) => TextEncoding::from_name(name)
            .ok_or_else(|| format!("LookupError: unknown encoding: {name}"))?,
    };
    match newline {
        None | Some("") | Some("\n") | Some("\r") | Some("\r\n") => {}
        Some(other) => {
            return Err(format!("ValueError: illegal newline value: {other}"));
        }
    }
    // Universal newlines are the default; any explicit value turns them off.
    let newline_translate = !binary && newline.is_none();
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
        // Every other `open` failure keeps the OS's own errno and strerror, so
        // `.errno` and the `OSError` subclass are right rather than a generic
        // `OSError` carrying Rust's `Display` text. Opening a DIRECTORY is the
        // common one: `open('/etc')` silently SUCCEEDED before this, because
        // `std::fs::File::open` on a directory only fails at read time on some
        // platforms and pythonrs never checked.
        _ => {
            let eno = e.raw_os_error().unwrap_or(0);
            format!(
                "{}: [Errno {eno}] {}: '{path}'",
                errno_exc_class(eno),
                errno_strerror(eno, &e)
            )
        }
    })?;
    // `EISDIR` is not reported by `open(2)` for O_RDONLY on Linux or macOS, so
    // the directory check is explicit — CPython's `_io.FileIO` does the same
    // `fstat` and raises `IsADirectoryError` itself.
    if f.metadata().map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!(
            "IsADirectoryError: [Errno 21] Is a directory: '{path}'"
        ));
    }
    Ok(with_host(|h| {
        h.io_alloc_file(
            f,
            path.to_string(),
            mode.to_string(),
            readable,
            writable,
            encoding,
            newline_translate,
        )
    }))
}

// ── stdout/stderr stream routing ─────────────────────────────────────────────

/// Write `s` to a print target: a native `File` handle uses `io_write` (so
/// `sys.__stdout__` and explicit file handles always reach the real stream); any
/// other object (a CPython `StringIO`, a user object with `write`) has its `write`
/// method called (no host borrow held, so the callee can re-enter).
pub fn write_to_stream(target: &Value, s: &str) -> Result<(), String> {
    if let Some(id) = with_host(|h| h.file_id(target)) {
        with_host(|h| h.io_write(id, s))?;
        return Ok(());
    }
    let sv = with_host(|h| h.new_str(s.to_string()));
    call_method(target, "write", vec![sv], vec![])?;
    Ok(())
}

/// Write `s` to the current `sys.stdout` — its redirect target if reassigned
/// (`sys.stdout = io.StringIO()`, `contextlib.redirect_stdout`), else the native
/// stdout stream.
pub fn write_stdout(s: &str) -> Result<(), String> {
    match with_host(|h| h.stdout_target.clone()) {
        Some(t) => write_to_stream(&t, s),
        None => {
            with_host(|h| h.write_out(s, false));
            Ok(())
        }
    }
}

/// Write `s` to the current `sys.stderr` — its redirect target if reassigned,
/// else the native stderr stream.
pub fn write_stderr(s: &str) -> Result<(), String> {
    match with_host(|h| h.stderr_target.clone()) {
        Some(t) => write_to_stream(&t, s),
        None => {
            with_host(|h| h.write_out(s, true));
            Ok(())
        }
    }
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
    if !is_heap(v) {
        return None;
    }
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

impl PyHost {
    /// Mark `class` as `@functools.total_ordering` (comparison dispatch will
    /// derive its missing rich-comparison ops).
    pub fn mark_total_ordering(&mut self, class: &str) {
        self.total_ordering.insert(class.to_string());
    }

    /// Whether `class` was decorated with `functools.total_ordering`.
    pub fn is_total_ordering(&self, class: &str) -> bool {
        self.total_ordering.contains(class)
    }
}

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

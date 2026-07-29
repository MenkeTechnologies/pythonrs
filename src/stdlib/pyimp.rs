//! `_imp` — the import-machinery primitives `importlib` is built on.
//!
//! `importlib/__init__.py` opens with `import _imp` and hands it to
//! `_bootstrap._setup`, so `importlib.machinery` — and through it `inspect`,
//! `traceback`, `logging`, `unittest` and `hashlib` — cannot load without it.
//!
//! pythonrs resolves imports itself (native arms, then the vendored `pylib/`
//! tree), so most of this module describes machinery this runtime does not use:
//! there are no frozen modules, no `.so` extensions to dlopen, and no `.pyc`
//! files to hash-validate. The honest implementation is to answer accurately
//! about what IS here — `is_builtin` reports the native arms, `is_frozen` is
//! always false, `extension_suffixes` is empty — rather than to pretend. Code
//! that walks `sys.meta_path` then simply finds nothing on those branches and
//! falls through to the path finder, which is the correct outcome.

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// The modules served by a native arm rather than by a `.py` file, and the list
/// `sys.builtin_module_names` reports.
///
/// It has to be ACCURATE, not aspirational. Naming a module here that no arm
/// serves sends `importlib` down `_builtin_from_name`, where `create_builtin`
/// hands back nothing and the import dies inside the machinery instead of
/// falling through to the path finder — which is what `import warnings` did when
/// this list claimed `_warnings`, taking `traceback`, `inspect`, `logging`,
/// `unittest` and `hashlib` with it.
pub const BUILTIN_MODULES: &[&str] = &[
    "_ast",
    "_codecs",
    "_contextvars",
    "_imp",
    "_io",
    "_opcode",
    "_struct",
    "_thread",
    "_tokenize",
    "_typing",
    "atexit",
    "binascii",
    "builtins",
    "errno",
    "itertools",
    "marshal",
    "posix",
    "sys",
    "time",
    // `_bootstrap._setup` loads these three through `_builtin_from_name` before
    // anything else can import, so they must resolve as builtins even though
    // pythonrs serves them from `pylib/`. See `create_builtin`.
    "_warnings",
    "_weakref",
];

/// What `create_builtin` actually imports for a given name. CPython's `_warnings`
/// IS the C version of `_py_warnings` — same API, same semantics — so resolving
/// the name to the vendored module is an alias, not a substitute.
pub fn builtin_source(name: &str) -> &str {
    match name {
        "_warnings" => "_py_warnings",
        other => other,
    }
}

/// `_imp.<name>(...)`.
pub fn call(h: &mut PyHost, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let str_arg = |h: &PyHost, i: usize| args.get(i).and_then(|v| h.as_str(v));
    Some(match name {
        "is_builtin" => {
            // CPython returns 1 for a builtin, -1 for `sys`/`builtins` (which
            // cannot be reinitialized), 0 otherwise. `importlib` only tests
            // truthiness, but the distinction costs nothing to keep.
            let n = str_arg(h, 0).unwrap_or_default();
            Ok(Value::Int(match n.as_str() {
                "sys" | "builtins" => -1,
                _ if BUILTIN_MODULES.contains(&n.as_str()) => 1,
                _ => 0,
            }))
        }
        // Nothing is frozen: pythonrs ships its stdlib as `.py` under `pylib/`,
        // not as marshalled code baked into the binary.
        "is_frozen" | "is_frozen_package" => Ok(Value::Bool(false)),
        "find_frozen" | "get_frozen_object" => Ok(Value::Undef),
        "init_frozen" => Ok(Value::Undef),
        "_frozen_module_names" => Ok(h.new_list(vec![])),
        // No dynamically loaded C extensions, so no suffixes to look for.
        "extension_suffixes" => Ok(h.new_list(vec![])),
        // `create_builtin` is handled by the caller, outside the host borrow:
        // it re-enters the importer, which needs `&mut PyHost` of its own.
        "create_dynamic" => Ok(Value::Undef),
        "exec_builtin" | "exec_dynamic" => Ok(Value::Int(0)),
        // The import lock: pythonrs runs user code on one thread, so the lock is
        // real but never contended.
        "acquire_lock" | "release_lock" => Ok(Value::Undef),
        "lock_held" => Ok(Value::Bool(false)),
        // `_fix_co_filename(code, path)` rewrites a code object's filename after
        // an unmarshal. Nothing is unmarshalled here, so there is nothing to fix.
        "_fix_co_filename" => Ok(Value::Undef),
        "_override_frozen_modules_for_tests" | "_override_multi_interp_extensions_check" => {
            Ok(Value::Int(0))
        }
        // The source hash stamped into a hash-based `.pyc`. pythonrs has its own
        // rkyv bytecode cache keyed by content, and never writes a `.pyc`.
        "source_hash" => Ok(h.alloc(PyObj::Bytes(vec![0; 8]))),
        // `marshal` — CPython's `.pyc` serialization format. pythonrs caches
        // bytecode in its own rkyv store and never reads or writes a `.pyc`, so
        // there is no format here to be compatible with. The module exists
        // because `importlib._bootstrap_external` imports it at module level;
        // reaching an actual call means something tried to load a CPython `.pyc`,
        // which is a real error rather than something to fake a value for.
        "loads" | "load" => Err("ValueError: bad marshal data".to_string()),
        "dumps" | "dump" => Err(host::type_error(
            "unmarshallable object: pythonrs does not emit CPython bytecode",
        )),
        _ => return None,
    })
}

/// The `marshal` namespace. See the `loads`/`dumps` arms above for why it is here
/// and why the functions refuse rather than pretend.
pub fn marshal_entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = ["dump", "dumps", "load", "loads"]
        .iter()
        .map(|f| ((*f), h.alloc(PyObj::Builtin(format!("marshal.{f}")))))
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    // CPython 3.14's marshal format revision.
    out.push(("version".to_string(), Value::Int(4)));
    out
}

/// The `_imp` namespace.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const FNS: &[&str] = &[
        "is_builtin",
        "is_frozen",
        "is_frozen_package",
        "find_frozen",
        "get_frozen_object",
        "init_frozen",
        "_frozen_module_names",
        "extension_suffixes",
        "create_builtin",
        "create_dynamic",
        "exec_builtin",
        "exec_dynamic",
        "acquire_lock",
        "release_lock",
        "lock_held",
        "_fix_co_filename",
        "_override_frozen_modules_for_tests",
        "_override_multi_interp_extensions_check",
        "source_hash",
    ];
    let mut out: Vec<(String, Value)> = FNS
        .iter()
        .map(|f| {
            (
                (*f).to_string(),
                h.alloc(PyObj::Builtin(format!("_imp.{f}"))),
            )
        })
        .collect();
    // `check_hash_based_pycs` is a string setting, not a function. pythonrs never
    // reads a `.pyc`, so the default CPython value is what it reports.
    let default = h.new_str("default".to_string());
    out.push(("check_hash_based_pycs".to_string(), default));
    // An INT, not a function: `_bootstrap_external` does
    // `MAGIC_NUMBER = _imp.pyc_magic_number_token.to_bytes(4, 'little')` at module
    // level. pythonrs never writes a `.pyc`, so the value only has to be stable.
    out.push((
        "pyc_magic_number_token".to_string(),
        Value::Int(168_627_755),
    ));
    let _ = host::type_error;
    out
}

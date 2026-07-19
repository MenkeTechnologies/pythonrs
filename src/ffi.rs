//! CPython stdlib FFI bridge (feature `stdlib-ffi`).
//!
//! pythonrs does not reimplement the standard library. When this feature is on,
//! `import <stdlib>` delegates to an embedded libpython over pyo3, so user code
//! gets the *real* CPython stdlib — pure `.py` modules **and** the C accelerators
//! (`_sre`, `_hashlib`, `_datetime`, `_json`, …). User code still runs on fusevm;
//! only the imported stdlib objects live on the CPython side.
//!
//! A stdlib object that pythonrs can represent by value (int/float/bool/None/str/
//! bytes/list/tuple/dict/set) is marshaled across the boundary. Everything else
//! (compiled regex, `datetime`, sockets, file objects, iterators, …) stays on the
//! CPython side behind a [`PyObj::Foreign`](crate::host::PyObj::Foreign) handle:
//! an index into the side-table below. Attribute access, calls, indexing,
//! iteration, `len`, `str`/`repr`, and membership on a `Foreign` route back
//! through here; pyo3 owns the refcounts and the GIL.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyTuple};
use pyo3::IntoPyObjectExt;

use crate::host::{with_host, PyHost, PyObj};
use fusevm::Value;

/// Side-table of live CPython objects, indexed by the `u32` carried in a
/// `PyObj::Foreign`. Entries are never freed for the process lifetime — stdlib
/// objects (modules, compiled patterns) are effectively permanent, and pyo3's
/// `Py<PyAny>` keeps each alive across GIL drops.
static TABLE: OnceLock<Mutex<Vec<Py<PyAny>>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<Py<PyAny>>> {
    TABLE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Resolve the CPython prefix to hand to `PYTHONHOME`, or `None` to let the
/// linked libpython locate its own stdlib (the system-CPython path).
///
/// Order: `PYTHONRS_STDLIB` env → bundled `<exe_dir>/../lib/python3.14` → system.
fn resolve_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PYTHONRS_STDLIB") {
        return Some(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // The stdlib is `<prefix>/lib/python3.14`; `PYTHONHOME` wants the
            // prefix, so a bundled tree next to the binary makes home `<exe>/..`.
            if dir.join("../lib/python3.14").is_dir() {
                return dir.parent().map(PathBuf::from);
            }
        }
    }
    None
}

/// Initialize the embedded interpreter once, after pinning `PYTHONHOME` so the
/// stdlib resolves to the intended (bundled or system) tree. Idempotent.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(home) = resolve_home() {
            // Set before the interpreter starts — CPython reads it at init only.
            std::env::set_var("PYTHONHOME", home);
        }
        pyo3::prepare_freethreaded_python();
    });
}

/// Store a CPython object in the side-table and hand back its `Foreign` id.
fn store(obj: Py<PyAny>) -> u32 {
    let mut t = table().lock().expect("ffi table poisoned");
    t.push(obj);
    (t.len() - 1) as u32
}

/// A fresh owned handle to the side-table object `id`, bound to `py`.
fn fetch<'py>(py: Python<'py>, id: u32) -> Result<Bound<'py, PyAny>, String> {
    let t = table().lock().expect("ffi table poisoned");
    match t.get(id as usize) {
        Some(obj) => Ok(obj.clone_ref(py).into_bound(py)),
        None => Err(format!("ffi: invalid foreign handle {id}")),
    }
}

/// Import `name` (possibly dotted, e.g. `os.path`) via CPython's own importer and
/// return a `Foreign` handle to the module object.
pub fn import(name: &str) -> Result<u32, String> {
    init();
    Python::with_gil(|py| match py.import(name) {
        Ok(module) => Ok(store(module.into_any().unbind())),
        Err(e) => Err(e.to_string()),
    })
}

// ── marshaling: pythonrs Value ↔ CPython object ──────────────────────────────

/// pythonrs `Value` → CPython object. By value for the representable types;
/// a `Foreign` handle passes the underlying CPython object straight through.
fn value_to_py<'py>(
    host: &PyHost,
    py: Python<'py>,
    v: &Value,
) -> Result<Bound<'py, PyAny>, String> {
    let conv = |b: Result<Bound<'py, PyAny>, PyErr>| b.map_err(|e| e.to_string());
    match v {
        Value::Undef => Ok(py.None().into_bound(py)),
        Value::Bool(b) => conv(b.into_bound_py_any(py)),
        Value::Int(n) => conv(n.into_bound_py_any(py)),
        Value::Float(f) => conv(f.into_bound_py_any(py)),
        Value::Str(s) => conv(s.as_str().into_bound_py_any(py)),
        Value::Obj(_) => match host.get(v) {
            Some(PyObj::Str(s)) => conv(s.as_str().into_bound_py_any(py)),
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(PyBytes::new(py, b).into_any()),
            Some(PyObj::BigInt(b)) => {
                // pyo3 has no num-bigint bridge enabled; round-trip through the
                // decimal string, which CPython's `int` parses into an exact int.
                let int_ctor = py
                    .import("builtins")
                    .and_then(|m| m.getattr("int"))
                    .map_err(|e| e.to_string())?;
                int_ctor.call1((b.to_string(),)).map_err(|e| e.to_string())
            }
            Some(PyObj::List(items)) => {
                let elems = marshal_seq(host, py, items)?;
                Ok(PyList::new(py, elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Tuple(items)) => {
                let elems = marshal_seq(host, py, items)?;
                Ok(PyTuple::new(py, elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Set(s)) => {
                let elems = marshal_seq(host, py, &s.values().cloned().collect::<Vec<_>>())?;
                Ok(PySet::new(py, &elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Dict(d)) => {
                let dict = PyDict::new(py);
                for (k, val) in d.values() {
                    let pk = value_to_py(host, py, k)?;
                    let pv = value_to_py(host, py, val)?;
                    dict.set_item(pk, pv).map_err(|e| e.to_string())?;
                }
                Ok(dict.into_any())
            }
            Some(PyObj::Foreign(id)) => fetch(py, *id),
            // A pythonrs callable (lambda / def / builtin / bound method / partial
            // / lru_cache) passed as a callback (`functools.reduce(f, …)`,
            // `sorted(key=f)`, …) is wrapped so CPython can call back into fusevm.
            Some(
                PyObj::Func(_)
                | PyObj::Builtin(_)
                | PyObj::BoundMethod { .. }
                | PyObj::Partial { .. }
                | PyObj::LruCache { .. }
                | PyObj::StaticMethod(_)
                | PyObj::ClassMethod(_),
            ) => {
                let cb = PyrsCallable { target: v.clone() };
                Py::new(py, cb)
                    .map(|p| p.into_any().into_bound(py))
                    .map_err(|e| e.to_string())
            }
            _ => Err(crate::host::type_error(&format!(
                "cannot pass '{}' to a CPython stdlib call",
                host.type_name(v)
            ))),
        },
        _ => Err(crate::host::type_error(
            "unsupported value for CPython call",
        )),
    }
}

fn marshal_seq<'py>(
    host: &PyHost,
    py: Python<'py>,
    items: &[Value],
) -> Result<Vec<Bound<'py, PyAny>>, String> {
    items.iter().map(|it| value_to_py(host, py, it)).collect()
}

/// CPython object → pythonrs `Value`. Only the *exact* representable types come
/// back by value; a subclass (namedtuple, `OrderedDict`, `Counter`, `IntEnum`, a
/// `str` subclass, …) stays a `Foreign` handle so its CPython repr/behavior is
/// preserved. Anything unrepresentable is likewise kept as `Foreign`.
fn py_to_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Result<Value, String> {
    if obj.is_none() {
        return Ok(Value::Undef);
    }
    if obj.is_exact_instance_of::<PyBool>() {
        return Ok(Value::Bool(
            obj.extract::<bool>().map_err(|e| e.to_string())?,
        ));
    }
    if obj.is_exact_instance_of::<PyInt>() {
        return Ok(match obj.extract::<i64>() {
            Ok(n) => Value::Int(n),
            // Out of i64 range → arbitrary-precision, parsed from the decimal repr.
            Err(_) => {
                let s = obj.str().map_err(|e| e.to_string())?.to_string();
                match s.parse::<num_bigint::BigInt>() {
                    Ok(b) => host.alloc(PyObj::BigInt(b)),
                    Err(_) => return Err(format!("ffi: cannot marshal int '{s}'")),
                }
            }
        });
    }
    if obj.is_exact_instance_of::<PyFloat>() {
        return Ok(Value::Float(
            obj.extract::<f64>().map_err(|e| e.to_string())?,
        ));
    }
    if obj.is_exact_instance_of::<pyo3::types::PyString>() {
        return Ok(host.new_str(obj.extract::<String>().map_err(|e| e.to_string())?));
    }
    if obj.is_exact_instance_of::<PyBytes>() {
        let b = obj.downcast::<PyBytes>().map_err(|e| e.to_string())?;
        return Ok(host.alloc(PyObj::Bytes(b.as_bytes().to_vec())));
    }
    if obj.is_exact_instance_of::<PyList>() {
        let list = obj.downcast::<PyList>().map_err(|e| e.to_string())?;
        let items = unmarshal_seq(host, py, list.iter())?;
        return Ok(host.new_list(items));
    }
    if obj.is_exact_instance_of::<PyTuple>() {
        let tup = obj.downcast::<PyTuple>().map_err(|e| e.to_string())?;
        let items = unmarshal_seq(host, py, tup.iter())?;
        return Ok(host.new_tuple(items));
    }
    if obj.is_exact_instance_of::<PyDict>() {
        let dict = obj.downcast::<PyDict>().map_err(|e| e.to_string())?;
        let mut map = indexmap::IndexMap::new();
        for (k, v) in dict.iter() {
            let kv = py_to_value(host, py, &k)?;
            let vv = py_to_value(host, py, &v)?;
            let key = host.to_key(&kv)?;
            map.insert(key, (kv, vv));
        }
        return Ok(host.new_dict(map));
    }
    if obj.is_exact_instance_of::<PySet>() || obj.is_exact_instance_of::<PyFrozenSet>() {
        let mut map = indexmap::IndexMap::new();
        for it in obj.try_iter().map_err(|e| e.to_string())? {
            let iv = py_to_value(host, py, &it.map_err(|e| e.to_string())?)?;
            let key = host.to_key(&iv)?;
            map.insert(key, iv);
        }
        return Ok(host.new_set(map));
    }
    // Anything else stays on the CPython side behind a Foreign handle.
    Ok(host.alloc(PyObj::Foreign(store(obj.clone().unbind()))))
}

fn unmarshal_seq<'py, I>(host: &mut PyHost, py: Python, items: I) -> Result<Vec<Value>, String>
where
    I: Iterator<Item = Bound<'py, PyAny>>,
{
    items.map(|it| py_to_value(host, py, &it)).collect()
}

// ── operations routed on a Foreign handle ────────────────────────────────────

/// `foreign.name` — attribute access (submodules, functions, constants, …).
pub fn get_attr(host: &mut PyHost, id: u32, name: &str) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let attr = obj.getattr(name).map_err(|e| e.to_string())?;
        py_to_value(host, py, &attr)
    })
}

/// `foreign(*args, **kwargs)` — call the foreign object.
///
/// The host borrow is dropped for the duration of the CPython call so a pythonrs
/// callback (a `PyrsCallable` passed as an argument) can re-enter the host.
pub fn call(id: u32, args: Vec<Value>, kwargs: Vec<(String, Value)>) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        invoke_bound(py, &obj, &args, &kwargs)
    })
}

/// `foreign.name(*args, **kwargs)` — call a method on the foreign object.
pub fn call_method(
    id: u32,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let method = obj.getattr(name).map_err(|e| e.to_string())?;
        invoke_bound(py, &method, &args, &kwargs)
    })
}

/// Marshal args (host borrow held only here, no user code runs), make the CPython
/// call (no host borrow — reverse callbacks are free to run), then marshal the
/// result back (fresh host borrow).
fn invoke_bound(
    py: Python,
    callable: &Bound<PyAny>,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let (arg_tuple, kw) = with_host(|h| build_call_args(h, py, args, kwargs))?;
    let result = callable
        .call(arg_tuple, kw.as_ref())
        .map_err(|e| e.to_string())?;
    with_host(|h| py_to_value(h, py, &result))
}

#[allow(clippy::type_complexity)]
fn build_call_args<'py>(
    host: &PyHost,
    py: Python<'py>,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<(Bound<'py, PyTuple>, Option<Bound<'py, PyDict>>), String> {
    let py_args = marshal_seq(host, py, args)?;
    let arg_tuple = PyTuple::new(py, py_args).map_err(|e| e.to_string())?;
    let kw = if kwargs.is_empty() {
        None
    } else {
        let d = PyDict::new(py);
        for (k, v) in kwargs {
            let pv = value_to_py(host, py, v)?;
            d.set_item(k.as_str(), pv).map_err(|e| e.to_string())?;
        }
        Some(d)
    };
    Ok((arg_tuple, kw))
}

/// A fusevm-side callable (lambda / def / builtin / …) exposed to CPython so it
/// can be used as a stdlib callback. `__call__` marshals the CPython arguments to
/// pythonrs values, runs the callable on fusevm (no host borrow held here), and
/// marshals the result back.
#[pyclass]
struct PyrsCallable {
    target: Value,
}

#[pymethods]
impl PyrsCallable {
    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(
        &self,
        py: Python,
        args: &Bound<PyTuple>,
        kwargs: Option<&Bound<PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let to_pyerr = |e: String| pyo3::exceptions::PyRuntimeError::new_err(e);
        // Marshal CPython args → pythonrs values (host borrow window).
        let rs_args: Vec<Value> = with_host(|h| {
            args.iter()
                .map(|a| py_to_value(h, py, &a))
                .collect::<Result<_, _>>()
        })
        .map_err(to_pyerr)?;
        let rs_kwargs: Vec<(String, Value)> = match kwargs {
            None => Vec::new(),
            Some(d) => with_host(|h| {
                d.iter()
                    .map(|(k, v)| {
                        let key = k.str().map_err(|e| e.to_string())?.to_string();
                        Ok((key, py_to_value(h, py, &v)?))
                    })
                    .collect::<Result<_, String>>()
            })
            .map_err(to_pyerr)?,
        };
        // Run the fusevm callable with NO host borrow held (invoke re-enters it).
        let result = crate::host::invoke(&self.target, rs_args, rs_kwargs).map_err(to_pyerr)?;
        // Marshal the result back to a CPython object (host borrow window).
        with_host(|h| value_to_py(h, py, &result))
            .map(|b| b.unbind())
            .map_err(to_pyerr)
    }
}

/// `foreign[idx]`.
pub fn get_item(host: &mut PyHost, id: u32, idx: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let key = value_to_py(host, py, idx)?;
        let item = obj.get_item(key).map_err(|e| e.to_string())?;
        py_to_value(host, py, &item)
    })
}

/// `iter(foreign)` — returns a `Foreign` iterator handle.
pub fn make_iter(host: &mut PyHost, id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let it = obj.try_iter().map_err(|e| e.to_string())?;
        Ok(host.alloc(PyObj::Foreign(store(it.into_any().unbind()))))
    })
}

/// `next(foreign)` — `None` on `StopIteration`.
pub fn iter_next(host: &mut PyHost, id: u32) -> Result<Option<Value>, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let mut it = obj.try_iter().map_err(|e| e.to_string())?;
        match it.next() {
            None => Ok(None),
            Some(Ok(item)) => Ok(Some(py_to_value(host, py, &item)?)),
            Some(Err(e)) => Err(e.to_string()),
        }
    })
}

/// `item in foreign`.
pub fn contains(host: &mut PyHost, id: u32, item: &Value) -> Result<bool, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let needle = value_to_py(host, py, item)?;
        obj.contains(needle).map_err(|e| e.to_string())
    })
}

/// `len(foreign)`.
pub fn len(id: u32) -> Result<usize, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        obj.len().map_err(|e| e.to_string())
    })
}

/// `str(foreign)`.
pub fn str_of(id: u32) -> String {
    Python::with_gil(
        |py| match fetch(py, id).and_then(|o| o.str().map_err(|e| e.to_string())) {
            Ok(s) => s.to_string(),
            Err(e) => e,
        },
    )
}

/// `repr(foreign)`.
pub fn repr_of(id: u32) -> String {
    Python::with_gil(
        |py| match fetch(py, id).and_then(|o| o.repr().map_err(|e| e.to_string())) {
            Ok(s) => s.to_string(),
            Err(e) => e,
        },
    )
}

/// `bool(foreign)`.
pub fn truthy(id: u32) -> bool {
    Python::with_gil(|py| {
        fetch(py, id)
            .ok()
            .and_then(|o| o.is_truthy().ok())
            .unwrap_or(true)
    })
}

/// The CPython type name of a foreign object (`module`, `re.Pattern`, …).
pub fn type_name(id: u32) -> String {
    Python::with_gil(|py| match fetch(py, id) {
        Ok(obj) => obj
            .get_type()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "object".into()),
        Err(_) => "object".into(),
    })
}

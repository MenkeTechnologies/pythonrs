//! CPython stdlib FFI bridge (feature `stdlib-ffi`).
//!
//! pythonrs does not reimplement the standard library. When this feature is on,
//! `import <stdlib>` delegates to an embedded libpython over pyo3, so user code
//! gets the *real* CPython stdlib — pure `.py` modules **and** the C accelerators
//! (`_sre`, `_hashlib`, `_datetime`, `_json`, …). User code still runs on fusevm;
//! only the imported stdlib objects live on the CPython side.
//!
//! A stdlib object that pythonrs can represent by value (int/float/bool/None/str/
//! bytes/bytearray/list/tuple/dict/set/frozenset/range/complex/`deque`) is
//! marshaled across the boundary in both directions. Everything else (compiled
//! regex, `datetime`, sockets, file objects, iterators, …) stays on the CPython
//! side behind a [`PyObj::Foreign`] handle: an index
//! into the side-table below. Attribute access, calls, indexing, iteration,
//! `len`, `str`/`repr`, and membership on a `Foreign` route back through here;
//! pyo3 owns the refcounts and the GIL.
//!
//! A by-value mutable-container argument (`list`/`bytearray`/`deque`) is copied
//! into a fresh CPython object, so an in-place stdlib mutator (`heapq.heapify`,
//! `random.shuffle`, `struct.pack_into`) would otherwise lose its effect; after
//! the call the bridge re-reads that object and overwrites the pythonrs heap slot
//! in place (see `writeback_mutated_args`) so the mutation — and aliases to the
//! same object — reflect it. Write-back marshals by value only and never
//! allocates a `Foreign`, so it does not grow the side-table.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString,
    PyTuple,
};
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

/// Module name → the side-table id its handle already occupies. `sys.modules`
/// makes every `py.import(name)` after the first hand back the SAME object, so
/// storing it again only grows the table. Without this, `module_ffi_fallback`
/// (which re-imports on every attribute miss — `math.isqrt`, `collections.ChainMap`)
/// allocated a slot per lookup, and per-thread host caches re-imported besides.
static MODULE_HANDLES: OnceLock<Mutex<rustc_hash::FxHashMap<String, u32>>> = OnceLock::new();

fn module_handles() -> &'static Mutex<rustc_hash::FxHashMap<String, u32>> {
    MODULE_HANDLES.get_or_init(|| Mutex::new(rustc_hash::FxHashMap::default()))
}

/// CPython type-object address → the side-table id its handle occupies. See
/// [`type_of`]; without it, `type(x)` on a foreign object in a loop would grow
/// the side-table once per call.
static TYPE_HANDLES: OnceLock<Mutex<rustc_hash::FxHashMap<usize, u32>>> = OnceLock::new();

fn type_handles() -> &'static Mutex<rustc_hash::FxHashMap<usize, u32>> {
    TYPE_HANDLES.get_or_init(|| Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Resolve the CPython prefix to hand to `PYTHONHOME`, or `None` to let the
/// linked libpython locate its own stdlib (the system-CPython path).
///
/// Order: `PYTHONRS_STDLIB` env → bundled `<exe_dir>/../lib/python3.*` → the
/// same probe through the RESOLVED exe path → per-user `~/.pythonrs/lib/python3.*`
/// (the `install.sh` target) → an inherited `PYTHONHOME` → system.
///
/// The resolved-exe probe is not a duplicate of the first one. `current_exe()`
/// on macOS reports the path the process was INVOKED by, symlinks intact, and
/// Homebrew installs pythonrs as `bin/python -> ../libexec/bin/python`. Probing
/// only the invoked path therefore asked whether `/opt/homebrew/lib/python3.*`
/// was a stdlib — see [`has_stdlib`] for why the answer used to be "yes".
fn resolve_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PYTHONRS_STDLIB") {
        return Some(PathBuf::from(p));
    }
    // A bundled tree next to the binary (`<prefix>/bin/python`,
    // `<prefix>/lib/python3.*`); `PYTHONHOME` wants the prefix (`<exe>/..`).
    if let Ok(exe) = std::env::current_exe() {
        for candidate in [Some(exe.clone()), std::fs::canonicalize(&exe).ok()]
            .into_iter()
            .flatten()
        {
            if let Some(prefix) = candidate.parent().and_then(|d| d.parent()) {
                if has_stdlib(prefix) {
                    return Some(prefix.to_path_buf());
                }
            }
        }
    }
    // The `~/.pythonrs` install (co-located with the bytecode cache), so a binary
    // placed anywhere on `PATH` still finds the vendored stdlib.
    if let Some(home) = dirs::home_dir() {
        let prefix = home.join(".pythonrs");
        if has_stdlib(&prefix) {
            return Some(prefix);
        }
    }
    // An inherited `PYTHONHOME` is the caller's explicit instruction, so it is
    // honoured — but only when it really holds a stdlib. Returning it here also
    // means the caller's value is never silently overwritten by one of ours.
    if let Some(p) = std::env::var_os("PYTHONHOME") {
        let prefix = PathBuf::from(p);
        if has_stdlib(&prefix) {
            return Some(prefix);
        }
    }
    None
}

/// Whether `prefix/lib/python3.*` is a REAL stdlib tree.
///
/// The directory existing is not enough, and the difference is the whole bug
/// this function exists to prevent. Homebrew's `/opt/homebrew/lib/python3.14`
/// exists on every Mac that has `python@3.14` installed and contains exactly one
/// entry — `site-packages`. No `encodings`, no `os.py`. Accepting it as a stdlib
/// set `PYTHONHOME=/opt/homebrew`, and CPython then aborted the whole process
/// with `Fatal Python error: Failed to import encodings module` before pythonrs
/// could report anything.
///
/// `encodings/__init__.py` is the file whose absence produces that exact fatal
/// error, and `os.py` is what `Py_Initialize` looks for to confirm a prefix, so
/// the two together are the check CPython itself is about to make.
fn has_stdlib(prefix: &std::path::Path) -> bool {
    stdlib_dir(prefix).is_some()
}

/// The `prefix/lib/python3.*` directory that is a usable stdlib, if any.
fn stdlib_dir(prefix: &std::path::Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(prefix.join("lib")).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("python3."))
            && p.join("encodings/__init__.py").is_file()
            && p.join("os.py").is_file()
    })
}

/// The error every bridged entry point reports when the embedded interpreter
/// cannot be started.
///
/// Deliberately an ordinary `ModuleNotFoundError`, byte-identical to the one a
/// genuinely absent module produces: it is catchable, it leaves the session
/// alive, and `try: import x / except ImportError:` — the shape every optional
/// dependency is written with — takes the branch it is supposed to take.
fn bridge_unavailable(name: &str) -> String {
    format!("ModuleNotFoundError: No module named '{name}'")
}

/// The candidate install prefixes of the libpython this binary is actually
/// linked against, derived from the loaded shared object's own path.
///
/// There is no single layout to assume, so both that exist are returned:
///   * `<prefix>/lib/libpython3.X.{dylib,so}` — the plain Unix install, prefix
///     two levels up.
///   * `<prefix>/Python` — the macOS framework install, where the dylib sits
///     directly in its own prefix (`Python.framework/Versions/3.X/Python`, with
///     the stdlib at `Python.framework/Versions/3.X/lib/python3.X`). This is how
///     Homebrew's `python@3.X` ships, so it is the layout on the common Mac.
///
/// Used only to decide whether starting the interpreter is safe when pythonrs
/// has no stdlib of its own to point at; [`init`] accepts the linked libpython
/// when ANY candidate holds a real stdlib. `dlsym`/`dladdr` are asked rather
/// than taking a symbol's address directly, because the address of an imported
/// function in this binary can be a stub that resolves back to this binary.
#[cfg(unix)]
fn linked_libpython_prefixes() -> Vec<PathBuf> {
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let Ok(sym) = CString::new("Py_IsInitialized") else {
        return Vec::new();
    };
    // SAFETY: `dlsym` on the global handle with a NUL-terminated name, and
    // `dladdr` on the pointer it returned. Both are read-only lookups, and
    // `dli_fname` is owned by the loader and valid for the process lifetime.
    let path = unsafe {
        let addr = libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr());
        if addr.is_null() {
            return Vec::new();
        }
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(addr, &mut info) == 0 || info.dli_fname.is_null() {
            return Vec::new();
        }
        PathBuf::from(OsStr::from_bytes(CStr::from_ptr(info.dli_fname).to_bytes()))
    };
    let Some(libdir) = path.parent() else {
        return Vec::new();
    };
    // Framework layout first, plain Unix layout second.
    let mut out = vec![libdir.to_path_buf()];
    if let Some(prefix) = libdir.parent() {
        out.push(prefix.to_path_buf());
    }
    out
}

/// Non-Unix has no `dladdr`; nothing is reported, and [`init`] proceeds as it
/// always did rather than refusing to start on a platform it cannot inspect.
#[cfg(not(unix))]
fn linked_libpython_prefixes() -> Vec<PathBuf> {
    Vec::new()
}

/// Whether the embedded interpreter can be started at all.
///
/// `false` means every `import` of a bridged module raises a catchable
/// `ModuleNotFoundError` instead of starting CPython. See [`init`].
static BRIDGE_USABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether `Py_Initialize` has actually run. [`flush_open_files`] is called on
/// every exit, including runs that never touched the bridge, and must not start
/// an interpreter just to tear one down.
static INTERPRETER_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Initialize the embedded interpreter once, after pinning `PYTHONHOME` so the
/// stdlib resolves to the intended (bundled or system) tree. Idempotent.
///
/// Returns `false` when the interpreter is not usable, which the callers turn
/// into an ordinary `ImportError`. **Failing to start CPython must never end the
/// process.** `Py_Initialize` reports its own failures through `Py_FatalError`,
/// which prints to stderr and calls `abort()`: there is no error to catch and no
/// stack to unwind, so an interactive session is simply gone — the user is
/// dropped back to their shell mid-line, and the next thing they type is eaten
/// by the shell. That outcome is not acceptable for any cause, so the decision is
/// made HERE, before CPython is started, by checking exactly what CPython is
/// about to check.
///
/// A `None` home is still initialised: that is the system-CPython path, where
/// the linked libpython finds its own compiled-in prefix and pythonrs has
/// nothing better to offer than letting it try.
pub fn init() -> bool {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // An explicit override that is not a stdlib is a hard stop. Honouring it
        // aborts the process, and quietly ignoring it would run against some
        // other tree than the one the caller named — so neither is done.
        if let Some(p) = std::env::var_os("PYTHONRS_STDLIB") {
            if !has_stdlib(std::path::Path::new(&p)) {
                BRIDGE_USABLE.store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        match resolve_home() {
            // Set before the interpreter starts — CPython reads it at init only.
            Some(home) => std::env::set_var("PYTHONHOME", home),
            None => {
                // Nothing we found is usable. An inherited `PYTHONHOME` that did
                // NOT survive `resolve_home` is a broken one, and leaving it set
                // is precisely what aborts, so it is removed rather than trusted.
                if std::env::var_os("PYTHONHOME").is_some() {
                    std::env::remove_var("PYTHONHOME");
                }
                // With no home of our own, CPython falls back to the prefix
                // compiled into the libpython we are linked against. When NONE
                // of that libpython's candidate prefixes holds a stdlib there is
                // nothing left to try, and starting the interpreter would abort
                // the process, so the bridge is declared unusable instead and
                // every import reports it.
                let linked = linked_libpython_prefixes();
                if !linked.is_empty() && !linked.iter().any(|p| has_stdlib(p)) {
                    BRIDGE_USABLE.store(false, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            }
        }
        pyo3::prepare_freethreaded_python();
        INTERPRETER_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
        route_std_streams();
        install_main_module_hook();
    });
    BRIDGE_USABLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// CPython's `__main__` is the embedded interpreter's own, empty module, while
/// the program's names live in pythonrs's `__main__` (module slot 0). A module
/// `__getattr__` (PEP 562) answers a name CPython looks up there from the
/// program's namespace — `pickle` finding `__main__.P` or `__main__.f` by
/// module path, `getattr(sys.modules['__main__'], 'X')` in CPython code. It
/// resolves against the host of the calling thread, the one whose program
/// handed the object over.
fn install_main_module_hook() {
    Python::with_gil(|py| {
        let Ok(main) = py.import("__main__") else {
            return;
        };
        if let Ok(f) = wrap_pyfunction!(main_getattr, py) {
            let _ = main.setattr("__getattr__", f);
        }
    });
}

#[pyfunction]
fn main_getattr(py: Python, name: String) -> PyResult<Py<PyAny>> {
    match crate::host::try_with_host(|h| h.module_global(0, &name)).flatten() {
        Some(v) => with_host(|h| value_to_py(h, py, &v))
            .map(|b| b.unbind())
            .map_err(rs_err),
        None => Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "module '__main__' has no attribute '{name}'"
        ))),
    }
}

/// Point the embedded interpreter's `sys.stdout`/`sys.stderr` at pythonrs's own
/// standard streams.
///
/// One process has one `stdout`, and CPython buffers it in one place. With two
/// buffers — pythonrs's ([`crate::stdio`]) and the embedded interpreter's own
/// `TextIOWrapper` — anything that reaches CPython's real `print`
/// (`functools.partial(print, …)`, `ExitStack.callback(print, …)`,
/// `atexit.register(print, …)`, a bridged module writing `sys.stdout`) would
/// reach the descriptor in whichever order the two happened to flush, or be lost
/// entirely, since the embedded interpreter is never `Py_Finalize`d.
///
/// So each stream is replaced by a `TextIOWrapper` that keeps CPython's text
/// API (`encoding`, `errors`, `newline`, `write` returning a character count)
/// but is write-through over a [`PyrsStdStream`], which hands every write to
/// the same text layer pythonrs's `print` uses. The buffering decisions — line
/// vs block, `-u` — are then made once, by that layer. `sys.__stdout__` and
/// `sys.__stderr__` follow, as they name the same object in CPython.
fn route_std_streams() {
    Python::with_gil(|py| {
        let (Ok(sys), Ok(io)) = (py.import("sys"), py.import("io")) else {
            return;
        };
        for (name, stream) in [
            ("stdout", crate::stdio::Stream::Stdout),
            ("stderr", crate::stdio::Stream::Stderr),
        ] {
            let Ok(old) = sys.getattr(name) else { continue };
            // A stream CPython left as `None` (its descriptor was closed at
            // startup) stays `None`.
            if old.is_none() {
                continue;
            }
            let kw = PyDict::new(py);
            for attr in ["encoding", "errors"] {
                if let Ok(v) = old.getattr(attr) {
                    let _ = kw.set_item(attr, v);
                }
            }
            let _ = kw.set_item("newline", "\n");
            let _ = kw.set_item("write_through", true);
            let Ok(raw) = Py::new(py, PyrsStdStream { stream }) else { continue };
            let Ok(new) = io
                .getattr("TextIOWrapper")
                .and_then(|cls| cls.call((raw,), Some(&kw)))
            else {
                continue;
            };
            let _ = new.setattr("mode", "w");
            let _ = sys.setattr(name, &new);
            let _ = sys.setattr(format!("__{name}__").as_str(), &new);
        }
        // A redirect pythonrs installed before the interpreter existed.
        for stderr in [false, true] {
            if let Some(target) = PENDING_STD_TARGET.with(|p| p.borrow_mut()[stderr as usize].take()) {
                apply_std_target(py, stderr, target);
            }
        }
        watch_sys_streams(py);
    });
}

/// What the embedded interpreter's `sys.stdout`/`sys.stderr` should be — the
/// CPython half of [`crate::host::PyHost::set_std_target`].
pub enum StdTarget {
    /// Not redirected: the routed stream `sys.__stdout__` names.
    Native,
    /// `sys.stdout = None`.
    Null,
    /// A CPython object (an `io.StringIO`), installed as itself so CPython code
    /// sees the very object the program assigned.
    Foreign(u32),
    /// A pythonrs value, with the generation of the host that owns it.
    Pyrs(Value, u64),
}

thread_local! {
    /// A redirect set before the interpreter started, applied by
    /// [`route_std_streams`] once it does. Index 0 is stdout, 1 stderr.
    static PENDING_STD_TARGET: std::cell::RefCell<[Option<StdTarget>; 2]> =
        const { std::cell::RefCell::new([None, None]) };
}

/// Swap the embedded interpreter's `sys.stdout` (or `sys.stderr`) to match a
/// pythonrs redirect. Before the interpreter starts there is nothing to swap;
/// the target is held until [`route_std_streams`] runs.
pub fn set_std_target(stderr: bool, target: StdTarget) {
    if !INTERPRETER_STARTED.load(std::sync::atomic::Ordering::Relaxed) {
        PENDING_STD_TARGET.with(|p| {
            p.borrow_mut()[stderr as usize] = match target {
                StdTarget::Native => None,
                other => Some(other),
            }
        });
        return;
    }
    Python::with_gil(|py| apply_std_target(py, stderr, target));
}

fn apply_std_target(py: Python, stderr: bool, target: StdTarget) {
    let name = if stderr { "stderr" } else { "stdout" };
    let Ok(sys) = py.import("sys") else { return };
    // What CPython already wrote goes out before the stream changes hands.
    if let Ok(current) = sys.getattr(name) {
        if !current.is_none() {
            let _ = current.call_method0("flush");
        }
    }
    let new = match target {
        StdTarget::Native => sys.getattr(format!("__{name}__").as_str()),
        StdTarget::Null => Ok(py.None().into_bound(py)),
        StdTarget::Foreign(id) => fetch(py, id).map_err(pyo3::exceptions::PyRuntimeError::new_err),
        StdTarget::Pyrs(target, generation) => Py::new(
            py,
            PyrsRedirectStream {
                target,
                stderr,
                thread: std::thread::current().id(),
                generation,
            },
        )
        .map(|p| p.into_any().into_bound(py)),
    };
    if let Ok(new) = new {
        APPLYING_STD_TARGET.with(|a| a.set(true));
        let _ = sys.setattr(name, new);
        APPLYING_STD_TARGET.with(|a| a.set(false));
    }
}

// ── CPython-side `sys.stdout` / `sys.stderr` assignment ──────────────────────
//
// pythonrs's `print` writes to the host's `stdout_target`. CPython code can
// reassign the embedded interpreter's `sys.stdout` behind pythonrs's back —
// `unittest`'s `buffer=True` runner, a CPython-side `contextlib.redirect_stdout`
// — and CPython has exactly one `sys.stdout`, so pythonrs's `print` must follow.
//
// The assignment is observed with a dict watcher on `sys.__dict__`
// (`PyDict_AddWatcher`/`PyDict_Watch`, CPython >= 3.12): the callback runs as
// the dict is modified, classifies the new stream, and parks it here. The host
// applies it the next time it reads its target ([`take_cpython_std_assignment`]),
// so `print` pays one relaxed atomic load when nothing changed, and nothing
// changes `type(sys)`. The callback cannot apply it itself: it runs inside a
// CPython call, while the host may be mid-borrow.
//
// The watcher API is outside the limited API this crate builds against
// (`abi3-py39`), so it is resolved with `dlsym` at runtime: on a 3.9–3.11
// interpreter the symbols are absent and CPython-side assignments go unseen,
// as before.

/// The latest CPython-side assignment per stream (index 0 stdout, 1 stderr),
/// not yet applied to the host.
static STD_ASSIGNED: Mutex<[Option<StdTarget>; 2]> = Mutex::new([None, None]);
/// Whether [`STD_ASSIGNED`] holds anything — the fast path of every read.
static STD_ASSIGNED_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// Set while [`apply_std_target`] installs a pythonrs-side redirect, whose
    /// own `sys.stdout = …` the watcher must not report back as CPython's.
    static APPLYING_STD_TARGET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The CPython-side assignment to `sys.stdout` (`stderr` false) or `sys.stderr`
/// made since the last call, if any. Called by the host before it reads the
/// stream it writes to.
pub fn take_cpython_std_assignment(stderr: bool) -> Option<StdTarget> {
    if !STD_ASSIGNED_DIRTY.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let mut pending = STD_ASSIGNED.lock().expect("std assignment lock poisoned");
    let taken = pending[stderr as usize].take();
    if pending.iter().all(Option::is_none) {
        STD_ASSIGNED_DIRTY.store(false, std::sync::atomic::Ordering::Release);
    }
    taken
}

/// `PyDict_WatchEvent` values `ADDED` and `MODIFIED` (`cpython/dictobject.h`).
const PYDICT_EVENT_ADDED: std::ffi::c_int = 0;
const PYDICT_EVENT_MODIFIED: std::ffi::c_int = 1;

type DictWatchCallback = unsafe extern "C" fn(
    std::ffi::c_int,
    *mut pyo3::ffi::PyObject,
    *mut pyo3::ffi::PyObject,
    *mut pyo3::ffi::PyObject,
) -> std::ffi::c_int;

/// Watch `sys.__dict__` for `stdout`/`stderr` assignments. A no-op on an
/// interpreter without dict watchers.
fn watch_sys_streams(py: Python) {
    // SAFETY: `dlsym` with a NUL-terminated name only looks the symbol up. The
    // two signatures are CPython's own (`cpython/dictobject.h`, 3.12+).
    let (add, watch) = unsafe {
        (
            libc::dlsym(libc::RTLD_DEFAULT, c"PyDict_AddWatcher".as_ptr()),
            libc::dlsym(libc::RTLD_DEFAULT, c"PyDict_Watch".as_ptr()),
        )
    };
    if add.is_null() || watch.is_null() {
        return;
    }
    let Ok(sys) = py.import("sys") else { return };
    let Ok(dict) = sys.getattr("__dict__") else { return };
    // SAFETY: the pointers were resolved from the running libpython and carry
    // the declared C signatures; the GIL is held, as both calls require.
    unsafe {
        let add: unsafe extern "C" fn(DictWatchCallback) -> std::ffi::c_int = std::mem::transmute(add);
        let watch: unsafe extern "C" fn(std::ffi::c_int, *mut pyo3::ffi::PyObject) -> std::ffi::c_int =
            std::mem::transmute(watch);
        let id = add(sys_dict_watcher);
        if id < 0 {
            pyo3::ffi::PyErr_Clear();
            return;
        }
        if watch(id, dict.as_ptr()) < 0 {
            pyo3::ffi::PyErr_Clear();
        }
    }
}

/// The `sys.__dict__` watcher: records an assignment to `stdout`/`stderr`.
/// Runs with the GIL held, before the dict changes; it must not raise.
unsafe extern "C" fn sys_dict_watcher(
    event: std::ffi::c_int,
    _dict: *mut pyo3::ffi::PyObject,
    key: *mut pyo3::ffi::PyObject,
    new_value: *mut pyo3::ffi::PyObject,
) -> std::ffi::c_int {
    if !(event == PYDICT_EVENT_ADDED || event == PYDICT_EVENT_MODIFIED)
        || key.is_null()
        || new_value.is_null()
        || APPLYING_STD_TARGET.with(|a| a.get())
    {
        return 0;
    }
    // SAFETY: the callback is invoked with the GIL held and with borrowed
    // references to a live key and value.
    let py = unsafe { Python::assume_gil_acquired() };
    let key = unsafe { Bound::from_borrowed_ptr(py, key) };
    let name = key.downcast::<PyString>().ok().and_then(|s| s.to_cow().ok());
    let stderr = match name.as_deref() {
        Some("stdout") => false,
        Some("stderr") => true,
        _ => return 0,
    };
    let value = unsafe { Bound::from_borrowed_ptr(py, new_value) };
    let target = classify_std_assignment(py, stderr, &value);
    STD_ASSIGNED.lock().expect("std assignment lock poisoned")[stderr as usize] = Some(target);
    STD_ASSIGNED_DIRTY.store(true, std::sync::atomic::Ordering::Release);
    0
}

/// What a CPython-side `sys.stdout = value` means to pythonrs: the routed
/// native stream (`sys.__stdout__`, what a redirect restores), `None`, one of
/// pythonrs's own redirect streams (restoring a pythonrs-side redirect hands
/// back its target), or any other CPython object.
fn classify_std_assignment(py: Python, stderr: bool, value: &Bound<PyAny>) -> StdTarget {
    if value.is_none() {
        return StdTarget::Null;
    }
    let dunder = if stderr { "__stderr__" } else { "__stdout__" };
    let native = py.import("sys").and_then(|sys| sys.getattr(dunder));
    if native.is_ok_and(|n| n.is(value)) {
        return StdTarget::Native;
    }
    if let Ok(redirect) = value.downcast::<PyrsRedirectStream>() {
        let r = redirect.borrow();
        return StdTarget::Pyrs(r.target.clone(), r.generation);
    }
    StdTarget::Foreign(store(value.clone().unbind()))
}

/// Run `python -m <modname> [args…]` on the embedded CPython by calling
/// `runpy._run_module_as_main` — the exact private entry CPython's own `-m` uses
/// (`Modules/main.c` → `pymain_run_module` → `runpy._run_module_as_main`). The
/// module runs on the real interpreter, not on fusevm, so `-m pip`, `-m venv`,
/// `-m http.server`, `-m json.tool`, … behave identically to `python3 -m`.
///
/// `sys.argv` is set to `[modname, *args]`; `_run_module_as_main(alter_argv=True)`
/// then overwrites `argv[0]` with the module's resolved file, matching CPython.
/// Returns the process exit code: a `SystemExit` maps to its `.code` (an int as
/// itself, `None` → 0, a str printed to stderr → 1); any other uncaught exception
/// prints the CPython traceback and returns 1.
pub fn run_module(modname: &str, args: &[String]) -> i32 {
    if !init() {
        eprintln!("python: {}", bridge_unavailable("<module>"));
        return 1;
    }
    Python::with_gil(|py| {
        let sys = match py.import("sys") {
            Ok(m) => m,
            Err(e) => {
                eprintln!("python: {e}");
                return 1;
            }
        };
        let argv = PyList::empty(py);
        let _ = argv.append(modname);
        for a in args {
            let _ = argv.append(a);
        }
        if let Err(e) = sys.setattr("argv", argv) {
            eprintln!("python: {e}");
            return 1;
        }
        let runpy = match py.import("runpy") {
            Ok(m) => m,
            Err(e) => {
                eprintln!("python: {e}");
                return 1;
            }
        };
        // alter_argv=True → runpy replaces argv[0] with the module's origin path,
        // exactly as CPython's `-m` does.
        let code = match runpy.call_method1("_run_module_as_main", (modname, true)) {
            Ok(_) => 0,
            Err(e) => {
                if e.is_instance_of::<pyo3::exceptions::PySystemExit>(py) {
                    system_exit_code(py, &e)
                } else {
                    e.print(py);
                    1
                }
            }
        };
        // The embedded interpreter is never `Py_Finalize`d (the process just
        // exits), so its block-buffered `sys.stdout`/`stderr` would drop pending
        // output on a pipe. Flush both before returning so piped `-m` output
        // (e.g. `python -m pip --version | cat`) is not lost.
        for stream in ["stdout", "stderr"] {
            if let Ok(s) = sys.getattr(stream) {
                let _ = s.call_method0("flush");
            }
        }
        code
    })
}

/// The process exit code carried by a `SystemExit`: an int as itself, `None`
/// (or a missing `.code`) → 0, anything else → print `str(code)` to stderr, 1.
fn system_exit_code(py: Python, e: &PyErr) -> i32 {
    match e.value(py).getattr("code") {
        Ok(code) if code.is_none() => 0,
        Ok(code) => {
            if let Ok(n) = code.extract::<i32>() {
                n
            } else {
                eprintln!("{}", code.str().map(|s| s.to_string()).unwrap_or_default());
                1
            }
        }
        Err(_) => 0,
    }
}

thread_local! {
    /// How many side-table slots THIS thread has allocated. The table itself is
    /// process-global, so its length also counts every other thread's stores —
    /// useless as a leak metric when several interpreters run concurrently (the
    /// test suite). This counter attributes growth to the thread that caused it.
    static STORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Store a CPython object in the side-table and hand back its `Foreign` id.
fn store(obj: Py<PyAny>) -> u32 {
    STORES.with(|n| n.set(n.get() + 1));
    let mut t = table().lock().expect("ffi table poisoned");
    t.push(obj);
    (t.len() - 1) as u32
}

/// Number of live entries in the side-table. Diagnostic only (used by the
/// bridge's own tests to assert bounded growth).
pub fn table_len() -> usize {
    table().lock().map(|t| t.len()).unwrap_or(0)
}

/// Side-table slots allocated by the CURRENT thread. Diagnostic only: the
/// bridge's leak tests take a delta of this rather than of [`table_len`], so a
/// concurrently-running test's stores cannot be mistaken for a leak in the path
/// under measurement.
pub fn table_stores_on_thread() -> usize {
    STORES.with(|n| n.get())
}

/// A fresh owned handle to the side-table object `id`, bound to `py`.
fn fetch<'py>(py: Python<'py>, id: u32) -> Result<Bound<'py, PyAny>, String> {
    let t = table().lock().expect("ffi table poisoned");
    match t.get(id as usize) {
        Some(obj) => Ok(obj.clone_ref(py).into_bound(py)),
        None => Err(format!("ffi: invalid foreign handle {id}")),
    }
}

/// The exception type's `(class name, __mro__ base names)`, so pythonrs's
/// `except` matching can resolve a specific base — `except ValueError` catching a
/// foreign `json.JSONDecodeError`. `None` when the chain can't be read.
fn pyerr_class_bases(py: Python, err: &PyErr) -> Option<(String, Vec<String>)> {
    let ty = err.get_type(py);
    let class = ty.name().ok()?.to_string();
    let mut bases: Vec<String> = Vec::new();
    if let Ok(mro) = ty.getattr("__mro__") {
        if let Ok(seq) = mro.try_iter() {
            for c in seq.flatten() {
                if let Ok(n) = c.getattr("__name__").and_then(|n| n.extract::<String>()) {
                    bases.push(n);
                }
            }
        }
    }
    (!bases.is_empty()).then_some((class, bases))
}

/// Convert a CPython exception to pythonrs's terse `"Class: message"` string,
/// registering its base chain via a fresh host borrow. For the **borrow-free**
/// call path only (`invoke_bound`, which drops the host borrow across the call);
/// a caller that already holds `&mut PyHost` must use [`pyerr_to_error_h`] to
/// avoid a double borrow.
fn pyerr_to_error(py: Python, err: &PyErr) -> String {
    if let Some((class, bases)) = pyerr_class_bases(py, err) {
        with_host(|h| {
            h.foreign_exc_bases.insert(class, bases);
        });
    }
    let line = err.to_string();
    with_host(|h| record_foreign_exc(h, py, err, &line));
    line
}

/// Record what the raised exception carries beyond its rendered line — its real
/// `args` and its instance `__dict__` — so `synth_exc` can rebuild it without
/// re-parsing its own rendering (see [`ForeignExc`]).
fn record_foreign_exc(host: &mut PyHost, py: Python, err: &PyErr, line: &str) {
    host.foreign_exc = None;
    let value = err.value(py);
    let Ok(args) = value.getattr("args").and_then(|a| a.try_iter()) else {
        return;
    };
    let items: Vec<Bound<PyAny>> = args.flatten().collect();
    let Ok(args) = items
        .iter()
        .map(|a| py_to_value(host, py, a))
        .collect::<Result<Vec<Value>, String>>()
    else {
        return;
    };
    // `__dict__` holds only what the exception's own `__init__` set — the
    // `JSONDecodeError.lineno/colno/pos/msg/doc` family. A C-level exception
    // (`OSError`'s `errno`/`strerror`) keeps those in slots and reports an empty
    // dict here, which is why `synth_exc` still handles `OSError` on its own.
    let mut attrs: Vec<(String, Value)> = Vec::new();
    if let Some(d) = value
        .getattr("__dict__")
        .ok()
        .and_then(|d| d.downcast_into::<PyDict>().ok())
    {
        for (k, v) in d.iter() {
            let (Ok(name), Ok(val)) = (k.extract::<String>(), py_to_value(host, py, &v)) else {
                continue;
            };
            attrs.push((name, val));
        }
        attrs.sort_by(|a, b| a.0.cmp(&b.0));
    }
    host.foreign_exc = Some(crate::host::ForeignExc {
        line: line.to_string(),
        args,
        attrs,
        origin: Some(match paired_exception(host, value.as_any()) {
            Some(v) => crate::host::ExcOrigin::Paired(v),
            None => crate::host::ExcOrigin::Raised {
                handle: store(value.clone().into_any().unbind()),
                addr: value.as_ptr() as usize,
            },
        }),
    });
}

/// Like [`pyerr_to_error`] but registers through an already-held `&mut PyHost`
/// (the `get_item`/`set_attr` paths run inside `PyHost::get_item`/`set_attr`,
/// which hold the borrow — calling `with_host` there would double-borrow).
fn pyerr_to_error_h(host: &mut PyHost, py: Python, err: &PyErr) -> String {
    if let Some((class, bases)) = pyerr_class_bases(py, err) {
        host.foreign_exc_bases.insert(class, bases);
    }
    let line = err.to_string();
    record_foreign_exc(host, py, err, &line);
    line
}

/// Entries pythonrs's own `sys.path` holds that the embedded interpreter has not
/// been told about yet. The bridge delegates `import <unknown>` to CPython's
/// importer, which searches the EMBEDDED `sys.path` — a list built by libpython
/// at startup that contains the CPython stdlib and nothing of the running
/// program. The script's own directory (pythonrs `sys.path[0]`) and anything the
/// script inserted therefore never took part in resolution, so `import sibling`
/// beside the script raised `ModuleNotFoundError` on a bridged build.
fn pending_search_paths() -> &'static Mutex<Vec<String>> {
    static P: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

/// Queue pythonrs's `sys.path` for the embedded interpreter. Called from the
/// importer just before it delegates, from OUTSIDE any `with_host` borrow (the
/// bridge cannot re-enter the host). Order is preserved and applied at the front
/// of CPython's `sys.path`, so the script directory wins over the stdlib exactly
/// as it does in CPython.
pub fn queue_search_paths(paths: Vec<String>) {
    if let Ok(mut q) = pending_search_paths().lock() {
        *q = paths;
    }
}

/// Splice any queued paths into the embedded `sys.path`, skipping entries it
/// already has. Idempotent: re-importing does not grow the list.
fn apply_pending_search_paths() {
    let paths = match pending_search_paths().lock() {
        Ok(mut q) if !q.is_empty() => std::mem::take(&mut *q),
        _ => return,
    };
    Python::with_gil(|py| {
        let Ok(sys) = py.import("sys") else { return };
        let Ok(path) = sys.getattr("path") else {
            return;
        };
        let Ok(path) = path.downcast_into::<PyList>() else {
            return;
        };
        let have: Vec<String> = path
            .iter()
            .filter_map(|e| e.extract::<String>().ok())
            .collect();
        // Insert in reverse so the queued order survives repeated front-inserts.
        for p in paths.iter().rev() {
            if !have.iter().any(|h| h == p) {
                let _ = path.insert(0, p.as_str());
            }
        }
    });
}

/// pythonrs's `sys.argv`, waiting to be mirrored into the embedded interpreter.
///
/// The bridge delegates `import argparse` (and `getopt`, `unittest`, `pdb`, …) to
/// CPython, and those modules read the EMBEDDED `sys.argv` — the list libpython
/// builds at startup, which is `['']` because nothing passes the program's
/// arguments to `Py_Initialize`. `parser.parse_args()` therefore parsed an empty
/// argument list and every option came back at its default, silently: no error,
/// just the wrong answer.
fn pending_argv() -> &'static Mutex<Option<Vec<String>>> {
    static A: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(None))
}

/// Queue pythonrs's `sys.argv` for the embedded interpreter. Called from the
/// importer just before it delegates, from OUTSIDE any `with_host` borrow (the
/// bridge cannot re-enter the host), and re-queued on every delegation so a
/// program that rewrote `sys.argv` before importing argparse is honoured.
pub fn queue_argv(argv: Vec<String>) {
    if let Ok(mut q) = pending_argv().lock() {
        *q = Some(argv);
    }
}

/// Copy any queued argv into the embedded `sys.argv`, replacing it wholesale —
/// unlike the search paths, argv is not a set to merge into but the exact list
/// the program was invoked with. A rewrite of `sys.argv` performed AFTER the last
/// bridged import is not mirrored; the queue is refreshed on every import, which
/// is where the modules that read argv come from.
fn apply_pending_argv() {
    let argv = match pending_argv().lock() {
        Ok(mut q) => match q.take() {
            Some(a) => a,
            None => return,
        },
        Err(_) => return,
    };
    Python::with_gil(|py| {
        let Ok(sys) = py.import("sys") else { return };
        let list = PyList::empty(py);
        for a in &argv {
            let _ = list.append(a.as_str());
        }
        let _ = sys.setattr("argv", list);
    });
}

/// Flush every writable CPython file still open, then the standard streams.
///
/// The embedded interpreter is never `Py_Finalize`d — the process just exits — so
/// nothing runs the teardown that flushes CPython's block-buffered files. A
/// handle the program opened through the bridge (`io.open`, `tempfile`,
/// `gzip`, …) and did not close therefore lost every byte written to it, and the
/// file was left on disk at the length `open` truncated it to. Refcounting does
/// not save the dropped-handle case either: the side-table holds a strong
/// reference to each `Foreign` for the process lifetime, so a file object stays
/// alive long after the program's last name for it is gone.
///
/// `gc.get_objects()` is CPython's own enumeration of live containers, which is
/// how `Py_FinalizeEx` finds these too. Anything whose flush raises (a stream
/// closed underneath, a broken pipe) is skipped: this runs at exit, where a
/// teardown error must not replace the program's own outcome.
pub fn flush_open_files() {
    if !INTERPRETER_STARTED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    Python::with_gil(|py| {
        if let (Ok(gc), Ok(io_mod)) = (py.import("gc"), py.import("io")) {
            if let (Ok(objects), Ok(base)) =
                (gc.call_method0("get_objects"), io_mod.getattr("IOBase"))
            {
                if let Ok(objects) = objects.downcast_into::<PyList>() {
                    for obj in objects.iter() {
                        let Ok(true) = obj.is_instance(&base) else {
                            continue;
                        };
                        // `closed` is a property on a real stream; a subclass may
                        // raise from it, in which case the object is left alone.
                        if !matches!(
                            obj.getattr("closed").and_then(|c| c.extract::<bool>()),
                            Ok(false)
                        ) {
                            continue;
                        }
                        if !matches!(
                            obj.call_method0("writable")
                                .and_then(|w| w.extract::<bool>()),
                            Ok(true)
                        ) {
                            continue;
                        }
                        let _ = obj.call_method0("flush");
                    }
                }
            }
        }
        // The standard streams last: a file flush above may have written a
        // diagnostic through them.
        if let Ok(sys) = py.import("sys") {
            for stream in ["stdout", "stderr"] {
                if let Ok(s) = sys.getattr(stream) {
                    let _ = s.call_method0("flush");
                }
            }
        }
    });
}

/// Import `name` (possibly dotted, e.g. `os.path`) via CPython's own importer and
/// return a `Foreign` handle to the module object.
pub fn import(name: &str) -> Result<u32, String> {
    if INTERPRETER_STARTED.load(std::sync::atomic::Ordering::Relaxed) {
        apply_pending_argv();
    }
    if let Some(id) = module_handles()
        .lock()
        .ok()
        .and_then(|m| m.get(name).copied())
    {
        return Ok(id);
    }
    if !init() {
        return Err(bridge_unavailable(name));
    }
    apply_pending_search_paths();
    apply_pending_argv();
    Python::with_gil(|py| match py.import(name) {
        Ok(module) => {
            let id = store(module.into_any().unbind());
            if let Ok(mut m) = module_handles().lock() {
                m.insert(name.to_string(), id);
            }
            Ok(id)
        }
        Err(e) => Err(e.to_string()),
    })
}

/// Whether two `Foreign` handles point at the *same* CPython object (`is`
/// identity). Enum members and other CPython singletons compare equal under `is`
/// even when fetched into distinct handles.
pub fn same_object(a: u32, b: u32) -> bool {
    Python::with_gil(|py| match (fetch(py, a), fetch(py, b)) {
        (Ok(x), Ok(y)) => x.is(&y),
        _ => false,
    })
}

/// `PyObject_RichCompareBool(a, b, Py_EQ)` on two `Foreign` handles — CPython's
/// own identity-then-`__eq__`, the exact primitive `in` / `.index` / `.count` /
/// list-`==` use. Borrow-free: it reads only the FFI side-table (never the host),
/// so it is safe to call from `PyHost::equal` while the host is already borrowed
/// (unlike [`binary_op_cb`], which re-borrows the host to marshal a native
/// operand). Enum members — and any two equal CPython objects — compare True.
pub fn foreign_eq(a: u32, b: u32) -> bool {
    Python::with_gil(|py| match (fetch(py, a), fetch(py, b)) {
        (Ok(x), Ok(y)) => x.eq(&y).unwrap_or(false),
        _ => false,
    })
}

/// A native scalar to compare against a `Foreign` object without a host borrow —
/// the operand of `enum_member in [ints]`, `Decimal in [floats]`, etc.
pub enum Prim<'a> {
    Int(i64),
    Float(f64),
    Str(&'a str),
}

/// `a == b` where `a` is a `Foreign` handle and `b` is a native scalar — CPython's
/// `__eq__` (so `IntEnum.HIGH == 3`, `Decimal('1.5') == 1.5`). Borrow-free: the
/// scalar is built directly, no host marshaling, so it is safe to call from
/// `PyHost::equal` (which holds the host borrow) for `in`/`.index`/`.count`.
pub fn foreign_eq_prim(fid: u32, prim: Prim) -> bool {
    Python::with_gil(|py| {
        let Ok(x) = fetch(py, fid) else { return false };
        let other: Bound<PyAny> = match prim {
            Prim::Int(n) => match n.into_pyobject(py) {
                Ok(o) => o.into_any(),
                Err(_) => return false,
            },
            Prim::Float(f) => match crate::host::export_float(f).into_pyobject(py) {
                Ok(o) => o.into_any(),
                Err(_) => return false,
            },
            Prim::Str(s) => match s.into_pyobject(py) {
                Ok(o) => o.into_any(),
                Err(_) => return false,
            },
        };
        x.eq(&other).unwrap_or(false)
    })
}

/// The native number a `Foreign` object is EXACTLY equal to, if there is one.
///
/// CPython's numeric tower guarantees that numerically equal values hash
/// equally and therefore share one dict slot: `{1, Decimal(1)}` is a
/// one-element set. pythonrs keys a dict by a structural `PKey`, so a
/// `PKey::Foreign` and a `PKey::Int` can never be the same slot no matter how
/// their hashes compare — the equal pair stayed split. Reporting the native
/// equivalent lets the caller key such an object as that native number, which
/// puts both in one slot the way CPython's hash table does.
///
/// The equality is tested, not assumed, and that is the whole subtlety:
/// `int(Decimal('1.5'))` is `1` but `Decimal('1.5') != 1`, and
/// `float(Decimal('0.1'))` is `0.1` but `Decimal('0.1') != 0.1` (the decimal is
/// exactly one tenth, the float is not) — CPython keeps those as two distinct
/// keys, and so must this. `int` is tried before `float` so a large exact
/// integer is not forced through a lossy float.
///
/// Returns `None` for anything with no exact numeric equivalent — a `datetime`,
/// a plain `Enum` member (which compares equal to nothing), `Fraction(1, 3)`.
#[cfg(feature = "stdlib-ffi")]
pub fn foreign_numeric_key(fid: u32) -> Option<Value> {
    Python::with_gil(|py| {
        let obj = fetch(py, fid).ok()?;
        let builtins = py.import("builtins").ok()?;
        // `bool`/`int`/`Decimal(1)`/`IntEnum.RED` → the equal integer.
        if let Ok(as_int) = builtins.getattr("int").ok()?.call1((&obj,)) {
            if obj.eq(&as_int).unwrap_or(false) {
                return with_host(|h| py_to_value(h, py, &as_int)).ok();
            }
        }
        // `Decimal('0.5')`/`Fraction(1, 2)`/`Decimal('Infinity')` → the equal float.
        if let Ok(as_float) = builtins.getattr("float").ok()?.call1((&obj,)) {
            if obj.eq(&as_float).unwrap_or(false) {
                return as_float.extract::<f64>().ok().map(Value::Float);
            }
        }
        None
    })
}

/// Rich-compare two `Foreign` handles for ordering (`<`), so foreign elements
/// order correctly inside a pythonrs list/tuple sort or comparison
/// (`sorted([(IntEnum, …)])`, `[date] < [date]`). Borrow-free. An error (two
/// unorderable foreign types) surfaces CPython's `TypeError`.
pub fn foreign_cmp(a: u32, b: u32) -> Result<std::cmp::Ordering, String> {
    Python::with_gil(|py| match (fetch(py, a), fetch(py, b)) {
        (Ok(x), Ok(y)) => x.compare(&y).map_err(|e| e.to_string()),
        _ => Err("ffi: invalid foreign handle".into()),
    })
}

/// `hash(obj)` for a `Foreign` handle — CPython's own `__hash__`, so equal
/// objects hash equal (`hash(Decimal('1.5')) == hash(Decimal('1.50'))`) and enum
/// members / dates / fractions can key a pythonrs set or dict. Borrow-free (reads
/// only the FFI table). An unhashable CPython object (a marshaled `list`/`dict`
/// never reaches here) surfaces its `TypeError`.
pub fn foreign_hash(id: u32) -> Result<i64, String> {
    Python::with_gil(|py| match fetch(py, id) {
        Ok(x) => x.hash().map(|h| h as i64).map_err(|e| e.to_string()),
        Err(e) => Err(e),
    })
}

/// Create a class with foreign (CPython) bases via CPython's own class machinery
/// (`class C(enum.Enum): A = 1` → `EnumType`). `types.new_class` computes the
/// metaclass, fires `__prepare__`, and the body populates the prepared namespace
/// one key at a time — so a metaclass namespace like Enum's `_EnumDict` records
/// each member through `__setitem__`. Returns a `Foreign` handle to the class.
pub fn build_foreign_class(
    name: &str,
    bases: &[Value],
    members: &[(String, Value)],
) -> Result<Value, String> {
    if !init() {
        return Err(bridge_unavailable(name));
    }
    Python::with_gil(|py| {
        // Marshal bases + members under a short host borrow; the metaclass call
        // runs with none held (a method body may re-enter fusevm).
        let (bases_tuple, members_dict): (Bound<PyAny>, Bound<PyAny>) =
            with_host(|h| -> Result<_, String> {
                let base_objs: Vec<Bound<PyAny>> = bases
                    .iter()
                    .map(|b| value_to_py(h, py, b))
                    .collect::<Result<_, _>>()?;
                let bases_tuple = PyTuple::new(py, &base_objs).map_err(|e| e.to_string())?;
                let members_dict = PyDict::new(py);
                for (k, v) in members {
                    let pv = value_to_py(h, py, v)?;
                    members_dict
                        .set_item(k.as_str(), pv)
                        .map_err(|e| e.to_string())?;
                }
                Ok((bases_tuple.into_any(), members_dict.into_any()))
            })?;
        let helper = make_class_helper(py)?;
        let cls = helper
            .call1((name, bases_tuple, members_dict))
            .map_err(|e| e.to_string())?;
        let id = store(cls.unbind());
        Ok(with_host(|h| h.alloc(PyObj::Foreign(id))))
    })
}

/// The cached `_make(name, bases, members)` helper (built via `types.new_class`),
/// which populates the metaclass-prepared namespace one key at a time so
/// `_EnumDict.__setitem__` — and any other metaclass namespace — sees each key.
fn make_class_helper(py: Python) -> Result<Bound<PyAny>, String> {
    static MAKE_CLASS: OnceLock<Py<PyAny>> = OnceLock::new();
    if let Some(f) = MAKE_CLASS.get() {
        return Ok(f.bind(py).clone());
    }
    let code = cr#"
import types
def _make(name, bases, members):
    def body(ns):
        for k in members:
            ns[k] = members[k]
    return types.new_class(name, tuple(bases), {}, body)
"#;
    let module = PyModule::from_code(py, code, c"_pyrs_class.py", c"_pyrs_class")
        .map_err(|e| e.to_string())?;
    let f = module.getattr("_make").map_err(|e| e.to_string())?;
    let _ = MAKE_CLASS.set(f.clone().unbind());
    Ok(f)
}

// ── exceptions across the bridge ─────────────────────────────────────────────

/// [`crate::host::ExcBridge::pair`] for CPython object `obj`.
fn pair_exception(host: &PyHost, v: &Value, obj: &Bound<PyAny>, handle: u32, raised_by_cpython: bool) {
    host.exc_bridge
        .borrow_mut()
        .pair(v, handle, obj.as_ptr() as usize, raised_by_cpython);
}

/// The pythonrs exception CPython object `obj` stands for, if it has crossed
/// before (in either direction).
fn paired_exception(host: &PyHost, obj: &Bound<PyAny>) -> Option<Value> {
    host.exc_bridge.borrow().from_py.get(&(obj.as_ptr() as usize)).cloned()
}

/// The CPython object for pythonrs exception `v`, or `None` when `v` is not an
/// exception. An exception that has crossed before is the object it crossed
/// as. Otherwise one is built and paired with it: a builtin class constructs
/// its CPython builtin from `args` (a parser `SyntaxError` also carrying its
/// `_metadata`), and any other class — a user exception, a native module's
/// (`struct.error`) — becomes an instance of its mirror class (see
/// [`exception_mirror`]).
fn exc_to_py<'py>(host: &PyHost, py: Python<'py>, v: &Value) -> Result<Option<Bound<'py, PyAny>>, String> {
    let Value::Obj(id) = v else {
        return Ok(None);
    };
    if let Some(&handle) = host.exc_bridge.borrow().to_py.get(id) {
        return fetch(py, handle).map(Some);
    }
    let (class, args, builtin) = match host.get(v) {
        Some(PyObj::Exception { class, args }) => (class.clone(), args.clone(), true),
        Some(PyObj::Instance(i)) if host.class_is_exception(&i.class) => {
            let args = match host.inst_attr(&i.dict, "args").map(|t| host.get(&t).cloned()) {
                Some(Some(PyObj::Tuple(items))) => items,
                _ => Vec::new(),
            };
            (i.class.clone(), args, false)
        }
        _ => return Ok(None),
    };
    let pargs = marshal_seq(host, py, &args)?;
    let tup = PyTuple::new(py, pargs).map_err(|e| e.to_string())?;
    let builtin_type = builtin
        .then(|| py.import("builtins").and_then(|m| m.getattr(class.as_str())).ok())
        .flatten()
        .filter(|t| is_exception_type(py, t));
    let exc = match builtin_type {
        Some(ty) => ty.call1(tup).map_err(|e| e.to_string())?,
        None => {
            let mirror = exception_mirror(host, py, &class)?;
            exception_helpers(py)?
                .getattr("new")
                .and_then(|new| new.call1((mirror, tup)))
                .map_err(|e| e.to_string())?
        }
    };
    // A parser-raised `SyntaxError`'s `_metadata` is set beside its `args`,
    // not from them; `traceback`'s keyword-typo hint reads it.
    let meta = host
        .func_attrs
        .get(id)
        .filter(|_| crate::host::is_syntax_error_class(&class))
        .and_then(|attrs| attrs.get("_metadata"))
        .filter(|m| !matches!(m, Value::Undef));
    if let Some(meta) = meta {
        let pmeta = value_to_py(host, py, meta)?;
        exc.setattr("_metadata", pmeta).map_err(|e| e.to_string())?;
    }
    let handle = store(exc.clone().unbind());
    pair_exception(host, v, &exc, handle, false);
    Ok(Some(exc))
}

fn is_exception_type(py: Python, t: &Bound<PyAny>) -> bool {
    t.downcast::<pyo3::types::PyType>()
        .ok()
        .and_then(|t| t.is_subclass(&py.get_type::<pyo3::exceptions::PyBaseException>()).ok())
        .unwrap_or(false)
}

/// The CPython class standing for pythonrs exception class `class`, built once:
/// a subclass of its nearest builtin exception ancestor under the class's own
/// name, qualified name and module, whose `str`, `repr` and missing attributes
/// are answered by the pythonrs exception it mirrors — so a user exception
/// (`class MyErr(ValueError)`) is caught by a CPython `except ValueError`, and
/// prints and reads (`e.code`) as itself.
fn exception_mirror<'py>(host: &PyHost, py: Python<'py>, class: &str) -> Result<Bound<'py, PyAny>, String> {
    if let Some(&handle) = host.exc_bridge.borrow().mirrors.get(class) {
        return fetch(py, handle);
    }
    let builtins = py.import("builtins").map_err(|e| e.to_string())?;
    let (name, qualname, module, ancestors) = match host.classes.get(class) {
        Some(c) => (c.name.clone(), c.qualname.clone(), c.module.clone(), host.mro_of(class)),
        None => {
            let (module, name) = class.rsplit_once('.').unwrap_or(("builtins", class));
            (name.to_string(), name.to_string(), module.to_string(), crate::builtins::builtin_mro(class))
        }
    };
    let base = ancestors
        .iter()
        .filter(|a| !host.classes.contains_key(*a))
        .filter_map(|a| builtins.getattr(a.as_str()).ok())
        .find(|t| is_exception_type(py, t))
        .map_or_else(|| builtins.getattr("Exception"), Ok)
        .map_err(|e| e.to_string())?;
    let module = if module.is_empty() { "__main__".to_string() } else { module };
    let delegate = wrap_pyfunction!(exception_mirror_delegate, py).map_err(|e| e.to_string())?;
    let mirror = exception_helpers(py)?
        .getattr("mirror")
        .and_then(|f| f.call1((name, qualname, module, base, delegate)))
        .map_err(|e| e.to_string())?;
    let handle = store(mirror.clone().unbind());
    host.exc_bridge.borrow_mut().mirrors.insert(class.to_string(), handle);
    Ok(mirror)
}

/// What a mirror class asks of the pythonrs exception behind `exc`: its
/// `str()` (`what == "__str__"`), its `repr()`, or attribute `what`.
#[pyfunction]
fn exception_mirror_delegate(py: Python, exc: Bound<PyAny>, what: String) -> PyResult<Py<PyAny>> {
    let missing = || pyo3::exceptions::PyAttributeError::new_err(what.clone());
    let v = with_host(|h| paired_exception(h, &exc)).ok_or_else(missing)?;
    match what.as_str() {
        "__str__" => {
            let s = crate::builtins::py_str(&v).map_err(rs_err)?;
            Ok(s.into_pyobject(py)?.into_any().unbind())
        }
        "__repr__" => {
            let s = crate::builtins::py_repr(&v).map_err(rs_err)?;
            Ok(s.into_pyobject(py)?.into_any().unbind())
        }
        _ => {
            let attr = with_host(|h| h.get_attr(&v, &what)).map_err(|_| missing())?;
            with_host(|h| value_to_py(h, py, &attr))
                .map(|b| b.unbind())
                .map_err(rs_err)
        }
    }
}

/// The two Python-level helpers the mirrors need: `mirror(...)` builds a mirror
/// class, `new(cls, args)` an instance of one without running an `__init__`
/// (the pythonrs exception already ran its own).
fn exception_helpers(py: Python) -> Result<Bound<PyModule>, String> {
    static HELPERS: OnceLock<Py<PyModule>> = OnceLock::new();
    if let Some(m) = HELPERS.get() {
        return Ok(m.bind(py).clone());
    }
    let code = cr#"
def mirror(name, qualname, module, base, delegate):
    def __str__(self):
        return delegate(self, '__str__')
    def __repr__(self):
        return delegate(self, '__repr__')
    def __getattr__(self, attr):
        return delegate(self, attr)
    namespace = {
        '__qualname__': qualname,
        '__module__': module,
        '__str__': __str__,
        '__repr__': __repr__,
        '__getattr__': __getattr__,
    }
    return type(base)(name, (base,), namespace)

def new(cls, args):
    return cls.__new__(cls, *args)
"#;
    let module = PyModule::from_code(py, code, c"_pyrs_exceptions.py", c"_pyrs_exceptions")
        .map_err(|e| e.to_string())?;
    let _ = HELPERS.set(module.clone().unbind());
    Ok(module)
}

// ── native classes and functions as CPython sees them ────────────────────────

/// The CPython objects this thread's native classes and callables cross as.
///
/// A CPython class or function is ONE object: `pickle` stores a class or a
/// function by module path and refuses one whose path leads to a different
/// object (`found is not obj`), and `is` on anything the stdlib hands back
/// relies on the same. So a native class crosses as one cached mirror and a
/// native callable as one cached proxy, per host generation (heap ids are
/// never reused within one).
#[derive(Default)]
struct BridgeIdentity {
    /// (generation, class key) → the class's mirror.
    mirrors: std::collections::HashMap<(u64, String), Py<PyAny>>,
    /// Mirror address → (generation, class key) while the mirror stands for
    /// the native class alone. CPython code that CHANGES a mirror —
    /// `@dataclass` setting `__init__` — makes it a CPython class in its own
    /// right (the decorated class the program continues with), which then
    /// crosses back as a `Foreign` handle rather than as the native class.
    pristine: std::collections::HashMap<usize, (u64, String)>,
    /// (generation, heap id) → the callable's proxy.
    callables: std::collections::HashMap<(u64, u32), Py<PyAny>>,
    /// (generation, heap id) → the instance's proxy.
    instances: std::collections::HashMap<(u64, u32), Py<PyAny>>,
    /// CPython instance of a mirror (by address) → (generation, the instance,
    /// the native instance it became); see [`instance_from_mirror`].
    converted: std::collections::HashMap<usize, (u64, Py<PyAny>, Value)>,
}

thread_local! {
    static IDENTITY: std::cell::RefCell<BridgeIdentity> =
        std::cell::RefCell::new(BridgeIdentity::default());
}

/// The cached `_pyrs_mirror` helper module: the metaclass a mirror is built
/// with, and `make_mirror`. Calling a pristine mirror constructs the NATIVE
/// class (`pickle` rebuilding `P(*args)` from a `__reduce__`), and any change
/// to a mirror detaches it from the native class.
fn mirror_helper(py: Python) -> Result<Bound<PyAny>, String> {
    static HELPER: OnceLock<Py<PyAny>> = OnceLock::new();
    if let Some(m) = HELPER.get() {
        return Ok(m.bind(py).clone());
    }
    let code = cr#"
import types

class PyrsMirror(type):
    def __call__(cls, /, *args, **kwargs):
        native = _native_class(cls)
        if native is None:
            return super().__call__(*args, **kwargs)
        return native(*args, **kwargs)

    def __setattr__(cls, name, value):
        _detach(cls)
        super().__setattr__(name, value)

    def __delattr__(cls, name):
        _detach(cls)
        super().__delattr__(name)

def make_mirror(name, members):
    def body(ns):
        for k in members:
            ns[k] = members[k]
    return types.new_class(name, (object,), {'metaclass': PyrsMirror}, body)
"#;
    let module = PyModule::from_code(py, code, c"_pyrs_mirror.py", c"_pyrs_mirror")
        .map_err(|e| e.to_string())?;
    let install = |f: PyResult<Bound<pyo3::types::PyCFunction>>| -> Result<(), String> {
        module.add_function(f.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    };
    install(wrap_pyfunction!(_native_class, &module))?;
    install(wrap_pyfunction!(_detach, &module))?;
    let _ = HELPER.set(module.clone().into_any().unbind());
    Ok(module.into_any())
}

/// The native class a pristine mirror stands for, as a callable that
/// constructs it, or `None` once the mirror has been changed (or belongs to
/// another thread's or an earlier host's heap).
#[pyfunction]
fn _native_class(py: Python, cls: &Bound<PyAny>) -> PyResult<Option<Py<PyAny>>> {
    let Some(generation) = crate::host::try_with_host(|h| h.generation) else {
        return Ok(None);
    };
    let Some(cname) = pristine_class(cls, generation) else {
        return Ok(None);
    };
    let class = with_host(|h| h.alloc(PyObj::Class(cname)));
    let proxy = PyrsCallable {
        target: class,
        doc: None,
        module: None,
    };
    Ok(Some(Py::new(py, proxy)?.into_any()))
}

/// A mirror was changed by CPython code: it no longer stands for the native class.
#[pyfunction]
fn _detach(cls: &Bound<PyAny>) {
    let key = cls.as_ptr() as usize;
    IDENTITY.with(|m| m.borrow_mut().pristine.remove(&key));
}

/// The class key of the native class `obj` mirrors, if `obj` is a mirror that
/// still stands for it in the host of `generation` (this thread's live one).
fn pristine_class(obj: &Bound<PyAny>, generation: u64) -> Option<String> {
    IDENTITY.with(|m| {
        m.borrow()
            .pristine
            .get(&(obj.as_ptr() as usize))
            .filter(|(g, _)| *g == generation)
            .map(|(_, cname)| cname.clone())
    })
}

/// A native class as CPython sees it: a class over `object` carrying the
/// native namespace — methods as `PyrsCallable` descriptors (they bind `self`),
/// `__annotations__` and class variables by value — so a decorator
/// (`@dataclass`) can read the fields and add methods, and introspection
/// (`dataclasses.fields(Cls)`) sees what CPython would. Built once per class
/// and refreshed from the namespace on every later crossing while it is
/// pristine, so a class changed after it first crossed is seen as it is now.
fn class_mirror<'py>(
    host: &PyHost,
    py: Python<'py>,
    cname: &str,
) -> Result<Bound<'py, PyAny>, String> {
    let class_def = host.classes.get(cname);
    let members: Vec<(String, Value)> = class_def
        .map(|c| c.ns.iter().map(|(k, val)| (k.clone(), val.clone())).collect())
        .unwrap_or_default();
    let ns_dict = PyDict::new(py);
    for (k, val) in &members {
        let pv = value_to_py(host, py, val)?;
        ns_dict.set_item(k.as_str(), pv).map_err(|e| e.to_string())?;
    }
    let name = class_def.map_or(cname, |c| c.name.as_str());
    let qualname = class_def
        .map(|c| c.qualname.as_str())
        .filter(|q| !q.is_empty())
        .unwrap_or(name);
    if !ns_dict.contains("__module__").unwrap_or(false) {
        let module = class_def.map_or("__main__", |c| c.module.as_str());
        let _ = ns_dict.set_item("__module__", module);
    }
    let _ = ns_dict.set_item("__qualname__", qualname);
    let key = (host.generation, cname.to_string());
    let cached = IDENTITY.with(|m| m.borrow().mirrors.get(&key).map(|o| o.clone_ref(py)));
    if let Some(mirror) = cached {
        let mirror = mirror.into_bound(py);
        let still_pristine = IDENTITY.with(|m| {
            m.borrow().pristine.contains_key(&(mirror.as_ptr() as usize))
        });
        if still_pristine {
            // `type.__setattr__` itself: the metaclass's override would detach.
            let set = py
                .get_type::<pyo3::types::PyType>()
                .getattr("__setattr__")
                .map_err(|e| e.to_string())?;
            for (k, v) in ns_dict.iter() {
                set.call1((&mirror, k, v)).map_err(|e| e.to_string())?;
            }
        }
        return Ok(mirror);
    }
    let mirror = mirror_helper(py)?
        .getattr("make_mirror")
        .and_then(|f| f.call1((name, ns_dict)))
        .map_err(|e| e.to_string())?;
    IDENTITY.with(|m| {
        let mut m = m.borrow_mut();
        m.pristine.insert(mirror.as_ptr() as usize, key.clone());
        m.mirrors.insert(key, mirror.clone().unbind());
    });
    Ok(mirror)
}

/// A pythonrs callable (lambda / def / builtin / bound method / lru_cache)
/// passed as a callback (`functools.reduce(f, …)`, `sorted(key=f)`, …), wrapped
/// so CPython can call back into fusevm — the same wrapper every time.
fn callable_proxy<'py>(host: &PyHost, py: Python<'py>, v: &Value) -> Result<Bound<'py, PyAny>, String> {
    let Value::Obj(id) = v else {
        return Err(crate::host::type_error("unsupported value for CPython call"));
    };
    let key = (host.generation, *id);
    if let Some(p) = IDENTITY.with(|m| m.borrow().callables.get(&key).map(|o| o.clone_ref(py))) {
        return Ok(p.into_bound(py));
    }
    let proxy = PyrsCallable {
        target: v.clone(),
        doc: None,
        module: None,
    };
    let proxy = Py::new(py, proxy).map_err(|e| e.to_string())?.into_any();
    IDENTITY.with(|m| m.borrow_mut().callables.insert(key, proxy.clone_ref(py)));
    Ok(proxy.into_bound(py))
}

/// A CPython instance of a pristine mirror — made by `object.__new__(mirror)`,
/// which is how `pickle` (`copyreg.__newobj__`, `copyreg._reconstructor`)
/// rebuilds an object before restoring its state — crosses back as an instance
/// of the native class carrying the same attributes.
///
/// It is ONE native object however often it crosses: a class's own
/// `__setstate__` runs on it mid-load, and the finished object crosses again at
/// the end, so the pair is recorded for the life of the process (like the
/// side-table, never freed — the CPython object is kept alive with it, so its
/// address is not reused). It registers itself before its attributes are
/// converted, so an attribute that refers back to the object is the native one.
fn instance_from_mirror(
    host: &mut PyHost,
    py: Python,
    obj: &Bound<PyAny>,
    cname: String,
) -> Result<Value, String> {
    let key = obj.as_ptr() as usize;
    let known = IDENTITY.with(|m| {
        m.borrow()
            .converted
            .get(&key)
            .filter(|(generation, _, _)| *generation == host.generation)
            .map(|(_, _, v)| v.clone())
    });
    if let Some(inst) = known {
        return Ok(inst);
    }
    let inst = host.new_instance(cname.clone(), crate::host::NameMap::default());
    IDENTITY.with(|m| {
        m.borrow_mut()
            .converted
            .insert(key, (host.generation, obj.clone().unbind(), inst.clone()))
    });
    remember_from_py(obj, &inst);
    // `BUILD` puts the state into `__dict__` and the slot values (named as
    // `copyreg._slotnames` names them, mangled) through `setattr`. The mirror
    // of a subclass does not inherit its base's slots, so a value is routed by
    // the NATIVE class's slots, wherever the mirror instance kept it.
    let mut dict_attrs: Vec<(Bound<PyAny>, Bound<PyAny>)> = Vec::new();
    if let Ok(dict) = obj.getattr("__dict__") {
        if let Ok(dict) = dict.downcast::<PyDict>() {
            dict_attrs.extend(dict.iter());
        }
    }
    // Slot values are read by the native class's slot names: asking CPython's
    // `copyreg._slotnames` would cache `__slotnames__` on the mirror, a change
    // that detaches it.
    let native_slots = host.slot_names(&cname);
    let mut slot_attrs: Vec<(Bound<PyAny>, Bound<PyAny>)> = Vec::new();
    for name in &native_slots {
        if let Ok(value) = obj.getattr(name.as_str()) {
            slot_attrs.push((PyString::new(py, name).into_any(), value));
        }
    }
    for (k, v) in dict_attrs.into_iter().chain(slot_attrs) {
        let name: String = k.extract().map_err(|e| e.to_string())?;
        let value = py_to_value(host, py, &v)?;
        if native_slots.contains(&name) {
            host.set_attr(&inst, &name, value)?;
        } else {
            host.instance_dict_set(&inst, &name, value);
        }
    }
    Ok(inst)
}

// ── marshaling: pythonrs Value ↔ CPython object ──────────────────────────────

/// The containers one marshal has already converted, so the object GRAPH
/// crosses rather than a tree: a container reached twice (`[x, x]`, or the same
/// list passed as two arguments) becomes ONE object on the far side, and a
/// container that reaches itself (`l.append(l)`) terminates instead of
/// recursing until the native stack is gone — which aborted the process for
/// `json.dumps(l)` and `pickle.dumps(l)` where CPython raises or round-trips.
/// A mutable container registers itself BEFORE its elements are converted, so
/// a cycle back to it finds the object under construction. The memo lives for
/// one outermost marshal ([`MemoScope`]), never across a call.
struct Memo<K, V> {
    depth: usize,
    seen: std::collections::HashMap<K, V>,
}

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Memo {
            depth: 0,
            seen: std::collections::HashMap::new(),
        }
    }
}

type MemoKey<K, V> = std::thread::LocalKey<std::cell::RefCell<Memo<K, V>>>;

thread_local! {
    /// pythonrs heap id → the CPython object it crossed as.
    static TO_PY_MEMO: std::cell::RefCell<Memo<u32, Py<PyAny>>> =
        std::cell::RefCell::new(Memo::default());
    /// CPython object address → the pythonrs value it crossed as.
    static FROM_PY_MEMO: std::cell::RefCell<Memo<usize, Value>> =
        std::cell::RefCell::new(Memo::default());
}

/// One (possibly nested) marshal: the memo is cleared when the outermost scope
/// ends, so it never holds an object past the conversion that made it.
struct MemoScope<K: 'static, V: 'static>(&'static MemoKey<K, V>);

impl<K: 'static, V: 'static> MemoScope<K, V> {
    fn enter(key: &'static MemoKey<K, V>) -> Self {
        key.with(|m| m.borrow_mut().depth += 1);
        MemoScope(key)
    }
}

impl<K: 'static, V: 'static> Drop for MemoScope<K, V> {
    fn drop(&mut self) {
        // Taken out of the cell before it is dropped: releasing a `Py` can run
        // CPython code, which must not find the memo borrowed.
        let finished = self.0.with(|m| {
            let mut m = m.borrow_mut();
            m.depth -= 1;
            (m.depth == 0).then(|| std::mem::take(&mut m.seen))
        });
        drop(finished);
    }
}

/// Record that pythonrs container `v` crossed as `obj`.
fn remember_to_py(v: &Value, obj: &Bound<PyAny>) {
    if let Value::Obj(id) = v {
        TO_PY_MEMO.with(|m| m.borrow_mut().seen.insert(*id, obj.clone().unbind()));
    }
}

/// Record that CPython container `obj` crossed as `v`.
fn remember_from_py(obj: &Bound<PyAny>, v: &Value) {
    FROM_PY_MEMO.with(|m| m.borrow_mut().seen.insert(obj.as_ptr() as usize, v.clone()));
}

/// pythonrs `Value` → CPython object. By value for the representable types;
/// a `Foreign` handle passes the underlying CPython object straight through.
/// A container already converted in this marshal is the same object again.
fn value_to_py<'py>(
    host: &PyHost,
    py: Python<'py>,
    v: &Value,
) -> Result<Bound<'py, PyAny>, String> {
    let _scope = MemoScope::enter(&TO_PY_MEMO);
    let Value::Obj(id) = v else {
        return value_to_py_node(host, py, v);
    };
    if let Some(obj) = TO_PY_MEMO.with(|m| m.borrow().seen.get(id).map(|o| o.clone_ref(py))) {
        return Ok(obj.into_bound(py));
    }
    let obj = value_to_py_node(host, py, v)?;
    if matches!(
        host.get(v),
        Some(
            PyObj::List(_)
                | PyObj::Tuple(_)
                | PyObj::Dict(_)
                | PyObj::Set(_)
                | PyObj::Frozenset(_)
                | PyObj::Bytearray(_)
                | PyObj::Deque { .. }
                | PyObj::Instance(_)
        )
    ) {
        remember_to_py(v, &obj);
    }
    Ok(obj)
}

/// NaN identity across the bridge.
///
/// A float crosses by value, but a NaN's identity is observable (`[n] == [n]`,
/// `n in d`), and pythonrs carries it in the NaN's bits (`host::fresh_nan`).
/// This table pairs each CPython NaN object that crossed with the minted NaN
/// that stands for it, in both directions: a CPython NaN read twice
/// (`json.loads('[NaN, NaN]')`, whose two elements are its one `NaN` constant)
/// is the same pythonrs NaN, and a pythonrs NaN handed over twice (`[n, n]`) is
/// the same CPython object, and comes back as itself.
///
/// Only a NaN with CPython's own `Py_NAN` payload is paired. A NaN carrying a
/// payload of its own (read with `struct.unpack`) crosses with its bits
/// untouched, so packing it again writes the same bytes.
///
/// Each entry holds its CPython object alive, so its address cannot be reused
/// while the entry exists. An entry whose object nothing but this table
/// references can never be read back from CPython again, so it is dropped when
/// the table has doubled since the last sweep; the pythonrs copies keep their
/// minted bits, which no later NaN reuses.
#[derive(Default)]
struct NanBridge {
    /// CPython object address → (the object, the minted NaN standing for it).
    by_addr: rustc_hash::FxHashMap<usize, (Py<PyAny>, f64)>,
    /// Minted NaN bits → the CPython object's address.
    by_bits: rustc_hash::FxHashMap<u64, usize>,
    /// Entry count at which the next sweep runs.
    sweep_at: usize,
}

static NAN_BRIDGE: OnceLock<Mutex<NanBridge>> = OnceLock::new();

impl NanBridge {
    fn with<R>(f: impl FnOnce(&mut NanBridge) -> R) -> R {
        let m = NAN_BRIDGE.get_or_init(|| Mutex::new(NanBridge::default()));
        f(&mut m.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn insert(&mut self, py: Python<'_>, obj: Py<PyAny>, nan: f64) {
        if self.by_addr.len() >= self.sweep_at {
            self.by_addr.retain(|_, (o, _)| o.get_refcnt(py) > 1);
            let live: rustc_hash::FxHashSet<usize> = self.by_addr.keys().copied().collect();
            self.by_bits.retain(|_, addr| live.contains(addr));
            self.sweep_at = (self.by_addr.len() * 2).max(64);
        }
        let addr = obj.as_ptr() as usize;
        self.by_bits.insert(nan.to_bits(), addr);
        self.by_addr.insert(addr, (obj, nan));
    }
}

/// A CPython `float` as a pythonrs float, keeping a NaN's identity (see
/// [`NanBridge`]).
fn float_from_py(obj: &Bound<'_, PyAny>) -> PyResult<f64> {
    let f = obj.extract::<f64>()?;
    if f.to_bits() & !(1 << 63) != crate::host::PY_NAN_BITS {
        return Ok(f);
    }
    let addr = obj.as_ptr() as usize;
    Ok(NanBridge::with(|t| {
        if let Some((_, nan)) = t.by_addr.get(&addr) {
            return *nan;
        }
        let nan = crate::host::fresh_nan(f.is_sign_negative());
        t.insert(obj.py(), obj.clone().unbind(), nan);
        nan
    }))
}

/// A pythonrs float as a CPython `float`: a minted NaN is the CPython object
/// already paired with it, or a new one paired from now on (see [`NanBridge`]).
fn float_to_py<'py>(py: Python<'py>, f: f64) -> PyResult<Bound<'py, PyAny>> {
    if !crate::host::is_minted_nan(f) {
        return crate::host::export_float(f).into_bound_py_any(py);
    }
    NanBridge::with(|t| {
        if let Some(addr) = t.by_bits.get(&f.to_bits()) {
            if let Some((obj, _)) = t.by_addr.get(addr) {
                return Ok(obj.bind(py).clone());
            }
        }
        let obj = crate::host::export_float(f).into_bound_py_any(py)?;
        t.insert(py, obj.clone().unbind(), f);
        Ok(obj)
    })
}

/// One node of [`value_to_py`]: converts `v`, going back through
/// [`value_to_py`] for its elements.
fn value_to_py_node<'py>(
    host: &PyHost,
    py: Python<'py>,
    v: &Value,
) -> Result<Bound<'py, PyAny>, String> {
    let conv = |b: Result<Bound<'py, PyAny>, PyErr>| b.map_err(|e| e.to_string());
    match v {
        Value::Undef => Ok(py.None().into_bound(py)),
        Value::Bool(b) => conv(b.into_bound_py_any(py)),
        Value::Int(n) => conv(n.into_bound_py_any(py)),
        Value::Float(f) => conv(float_to_py(py, *f)),
        Value::Str(s) => conv(s.as_str().into_bound_py_any(py)),
        Value::Obj(_) => match host.get(v) {
            Some(PyObj::Str(s)) => conv(s.as_str().into_bound_py_any(py)),
            Some(PyObj::Bytes(b)) => Ok(PyBytes::new(py, b).into_any()),
            // A `bytearray` is mutable, so it crosses as a CPython `bytearray`
            // (not immutable `bytes`) — an in-place stdlib mutator such as
            // `struct.pack_into` writes into it, and the write-back after the call
            // reflects that back into the pythonrs object.
            Some(PyObj::Bytearray(b)) => Ok(PyByteArray::new(py, b).into_any()),
            Some(PyObj::BigInt(b)) => {
                // pyo3 has no num-bigint bridge enabled; round-trip through a
                // HEX string. Decimal would be bounded by the embedded
                // interpreter's `sys.get_int_max_str_digits()`, so any int past
                // 4300 digits failed to cross; a power-of-two base is exempt.
                let int_ctor = py
                    .import("builtins")
                    .and_then(|m| m.getattr("int"))
                    .map_err(|e| e.to_string())?;
                int_ctor
                    .call1((b.to_str_radix(16), 16))
                    .map_err(|e| e.to_string())
            }
            Some(PyObj::List(items)) => {
                let list = PyList::empty(py);
                remember_to_py(v, list.as_any());
                for item in items {
                    list.append(value_to_py(host, py, item)?)
                        .map_err(|e| e.to_string())?;
                }
                Ok(list.into_any())
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
            Some(PyObj::Frozenset(s)) => {
                let elems = marshal_seq(host, py, &s.values().cloned().collect::<Vec<_>>())?;
                Ok(PyFrozenSet::new(py, &elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Range { start, stop, step }) => {
                let range = py
                    .import("builtins")
                    .and_then(|m| m.getattr("range"))
                    .map_err(|e| e.to_string())?;
                range
                    .call1((*start, *stop, *step))
                    .map_err(|e| e.to_string())
            }
            Some(PyObj::Complex(re, im)) => {
                let cplx = py
                    .import("builtins")
                    .and_then(|m| m.getattr("complex"))
                    .map_err(|e| e.to_string())?;
                let (re, im) = (crate::host::export_float(*re), crate::host::export_float(*im));
                cplx.call1((re, im)).map_err(|e| e.to_string())
            }
            Some(PyObj::Deque { items, maxlen }) => {
                let elems = marshal_seq(host, py, &items.iter().cloned().collect::<Vec<_>>())?;
                let pylist = PyList::new(py, elems).map_err(|e| e.to_string())?;
                let deque = py
                    .import("collections")
                    .and_then(|m| m.getattr("deque"))
                    .map_err(|e| e.to_string())?;
                match maxlen {
                    Some(n) => deque.call1((pylist, *n)),
                    None => deque.call1((pylist,)),
                }
                .map_err(|e| e.to_string())
            }
            Some(PyObj::Dict(d)) => {
                let dict = PyDict::new(py);
                remember_to_py(v, dict.as_any());
                for (k, val) in d.values() {
                    let pk = value_to_py(host, py, k)?;
                    let pv = value_to_py(host, py, val)?;
                    dict.set_item(pk, pv).map_err(|e| e.to_string())?;
                }
                Ok(dict.into_any())
            }
            Some(PyObj::Ellipsis) => Ok(py.Ellipsis().into_bound(py)),
            Some(PyObj::Foreign(id)) => fetch(py, *id),
            // A pythonrs lazy iterator (generator / zip / map / filter /
            // enumerate / composite iterator) passed into a CPython call
            // (`itertools.takewhile(pred, gen())`, `"".join(gen())`, …) is wrapped
            // as a CPython iterator whose `__next__` drives fusevm one step at a
            // time — so an infinite generator is never materialized.
            Some(
                PyObj::Generator { .. }
                | PyObj::Iter(_)
                | PyObj::Zip { .. }
                | PyObj::MapObj { .. }
                | PyObj::FilterObj { .. }
                | PyObj::EnumerateObj { .. }
                | PyObj::CallIter { .. },
            ) => {
                let it = PyrsIterator { target: v.clone() };
                Py::new(py, it)
                    .map(|p| p.into_any().into_bound(py))
                    .map_err(|e| e.to_string())
            }
            // A bare builtin type/function (`int`, `str`, `len`, `sorted`, …)
            // crosses as the REAL CPython object when one exists: so `Optional
            // [int]` holds CPython's `int` (its repr needs no callback into a
            // borrowed host), and `reduce(min, …)` calls the real function. Only a
            // pythonrs-only or method-qualified builtin (`dict.fromkeys`) falls
            // through to the callback proxy below.
            Some(PyObj::Builtin(name))
                if !name.contains('.')
                    && py
                        .import("builtins")
                        .and_then(|m| m.getattr(name.as_str()))
                        .is_ok() =>
            {
                py.import("builtins")
                    .and_then(|m| m.getattr(name.as_str()))
                    .map_err(|e| e.to_string())
            }
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
            ) => callable_proxy(host, py, v),
            // A native pythonrs class passed into a CPython call (`@dataclass`,
            // `dataclasses.fields(Cls)`): build a CPython mirror over `object`
            // with the class namespace — methods cross as `PyrsCallable`
            // descriptors (they bind `self`), `__annotations__`/class-vars by
            // value — so the decorator can read the fields and add methods.
            Some(PyObj::Class(cname)) => class_mirror(host, py, cname),
            // An exception passed into a CPython call — the value handed to a
            // foreign context manager's `__exit__`, a `gen.throw` argument, an
            // error leaving a callback: the CPython object it is paired with.
            Some(PyObj::Exception { .. }) => exc_to_py(host, py, v)?
                .ok_or_else(|| "ffi: exception did not cross".to_string()),
            // A pythonrs `open()` handle passed into a CPython call
            // (`json.dump(cfg, f)`, `csv.writer(f)`, `csv.DictReader(f)`) is
            // wrapped as a file-like object whose read/write/iteration route
            // back to the native handle — so the stdlib writes the real file.
            Some(PyObj::File { .. }) => {
                let proxy = PyrsFile { target: v.clone() };
                Py::new(py, proxy)
                    .map(|p| p.into_any().into_bound(py))
                    .map_err(|e| e.to_string())
            }
            // A pythonrs instance passed into a CPython call (`operator.attrgetter
            // ("x")(pt)`, `sorted(objs, key=itemgetter(0))`, `json.dumps(obj,
            // default=...)`) is wrapped so CPython's attribute/item access,
            // comparison, hashing, and repr route back to the fusevm object.
            //
            // A class that defines `__index__` crosses as the subclass that fills
            // CPython's `nb_index` slot, so `operator.index(obj)`, `range(obj)`
            // and `lst[obj]` on the CPython side accept it. Only such a class:
            // CPython probes that slot (`PyIndex_Check`) to CHOOSE a path —
            // `bytes(x)` takes a length from an index-able `x` — so every other
            // instance must keep answering "no slot".
            //
            // The proxy is the same object every time the instance crosses,
            // as the instance is one object: `pickle` reaches an object again
            // through its own state (`a.me = a`) and recognises it by identity.
            Some(PyObj::Instance(i)) => {
                if let Some(exc) = exc_to_py(host, py, v)? {
                    return Ok(exc);
                }
                let Value::Obj(id) = v else {
                    return Err(crate::host::type_error("unsupported value for CPython call"));
                };
                let key = (host.generation, *id);
                if let Some(p) = IDENTITY.with(|m| m.borrow().instances.get(&key).map(|o| o.clone_ref(py))) {
                    return Ok(p.into_bound(py));
                }
                let proxy = PyrsInstance { target: v.clone() };
                let proxy = if crate::builtins::instance_has(host, i, "__index__") {
                    Py::new(py, (PyrsIndexInstance, proxy)).map(|p| p.into_any())
                } else {
                    Py::new(py, proxy).map(|p| p.into_any())
                }
                .map_err(|e| e.to_string())?;
                IDENTITY.with(|m| m.borrow_mut().instances.insert(key, proxy.clone_ref(py)));
                Ok(proxy.into_bound(py))
            }
            // `foreign[1:]` — a mutable container held behind a handle (see
            // `get_attr`) is sliced through CPython's own `__getitem__`, so the
            // slice object has to cross. Each bound is `None` or an int.
            Some(PyObj::Slice { lo, hi, step }) => {
                // Built through the `slice` builtin rather than `PySlice::new`,
                // because an omitted bound must cross as `None`: `xs[1:]` is
                // `slice(1, None)`, and a sentinel integer would be wrong the
                // moment the step is negative.
                let (lo, hi, step) = (lo.clone(), hi.clone(), step.clone());
                let bound = |b: &Value| match b {
                    Value::Undef => Ok(py.None().into_bound(py)),
                    other => value_to_py(host, py, other),
                };
                let a = bound(&lo)?;
                let b = bound(&hi)?;
                let c = bound(&step)?;
                py.import("builtins")
                    .and_then(|m| m.getattr("slice"))
                    .and_then(|f| f.call1((a, b, c)))
                    .map_err(|e| e.to_string())
            }
            // A native `list[int]` / `tuple[int, bool]` handed to the CPython
            // `typing` module (`Optional[tuple[int, int]]` in an annotation)
            // crosses as a real `types.GenericAlias` over the converted origin
            // and args, so `typing`'s `_type_check` accepts it.
            Some(PyObj::GenericAlias { origin, args }) => {
                let (origin, args) = (origin.clone(), args.clone());
                let porigin = value_to_py(host, py, &origin)?;
                let pargs = marshal_seq(host, py, &args)?;
                let tup = PyTuple::new(py, pargs).map_err(|e| e.to_string())?;
                py.import("types")
                    .and_then(|m| m.getattr("GenericAlias"))
                    .and_then(|ga| ga.call1((porigin, tup)))
                    .map_err(|e| e.to_string())
            }
            // A native PEP 604 union (`int | str`) crosses as `typing.Union[...]`
            // over the converted members — CPython's own union of the same types.
            Some(PyObj::Union { args }) => {
                let args = args.clone();
                let pargs = marshal_seq(host, py, &args)?;
                let tup = PyTuple::new(py, pargs).map_err(|e| e.to_string())?;
                py.import("typing")
                    .and_then(|m| m.getattr("Union"))
                    .and_then(|u| u.get_item(tup))
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

/// A CPython `int` outside `i64` as a `BigInt`, read through `format(n, "x")`.
/// Its decimal `str()` is refused past the embedded interpreter's
/// `sys.get_int_max_str_digits()`, which made any int over 4300 digits returned
/// by a bridged call fail to come back at all; hex is never limited.
fn big_from_py(obj: &Bound<PyAny>) -> Result<num_bigint::BigInt, String> {
    let hex = obj
        .call_method1("__format__", ("x",))
        .and_then(|s| s.extract::<String>())
        .map_err(|e| e.to_string())?;
    let (neg, digits) = match hex.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, hex.as_str()),
    };
    let b = num_bigint::BigInt::parse_bytes(digits.as_bytes(), 16)
        .ok_or_else(|| format!("ffi: cannot marshal int 0x{hex}"))?;
    Ok(if neg { -b } else { b })
}

/// CPython object → pythonrs `Value`. Only the *exact* representable types come
/// back by value; a subclass (namedtuple, `OrderedDict`, `Counter`, `IntEnum`, a
/// `str` subclass, …) stays a `Foreign` handle so its CPython repr/behavior is
/// preserved. Anything unrepresentable is likewise kept as `Foreign`.
fn py_to_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Result<Value, String> {
    let _scope = MemoScope::enter(&FROM_PY_MEMO);
    let key = obj.as_ptr() as usize;
    if let Some(v) = FROM_PY_MEMO.with(|m| m.borrow().seen.get(&key).cloned()) {
        return Ok(v);
    }
    let v = py_to_value_node(host, py, obj)?;
    if obj.is_exact_instance_of::<PyTuple>()
        || obj.is_exact_instance_of::<PySet>()
        || obj.is_exact_instance_of::<PyFrozenSet>()
    {
        remember_from_py(obj, &v);
    }
    Ok(v)
}

/// One node of [`py_to_value`]: converts `obj`, going back through
/// [`py_to_value`] for its elements. A list or dict registers itself (see
/// [`Memo`]) before its elements are converted.
fn py_to_value_node(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Result<Value, String> {
    if obj.is_none() {
        return Ok(Value::Undef);
    }
    // CPython `Ellipsis` (`...`) crosses back as the native singleton (distinct
    // from `None`) so identity and repr match.
    if obj.is(&py.Ellipsis()) {
        return Ok(host.alloc(PyObj::Ellipsis));
    }
    if obj.is_exact_instance_of::<PyBool>() {
        return Ok(Value::Bool(
            obj.extract::<bool>().map_err(|e| e.to_string())?,
        ));
    }
    if obj.is_exact_instance_of::<PyInt>() {
        return Ok(match obj.extract::<i64>() {
            Ok(n) => Value::Int(n),
            // Out of i64 range → arbitrary-precision, read back in hex (see
            // `big_from_py`).
            Err(_) => host.alloc(PyObj::BigInt(big_from_py(obj)?)),
        });
    }
    if obj.is_exact_instance_of::<PyFloat>() {
        return Ok(Value::Float(float_from_py(obj).map_err(|e| e.to_string())?));
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
        let out = host.new_list(Vec::new());
        remember_from_py(obj, &out);
        let items = unmarshal_seq(host, py, list.iter())?;
        if let Some(PyObj::List(slot)) = host.get_mut(&out) {
            *slot = items;
        }
        return Ok(out);
    }
    if obj.is_exact_instance_of::<PyTuple>() {
        let tup = obj.downcast::<PyTuple>().map_err(|e| e.to_string())?;
        let items = unmarshal_seq(host, py, tup.iter())?;
        return Ok(host.new_tuple(items));
    }
    if obj.is_exact_instance_of::<PyDict>() {
        let dict = obj.downcast::<PyDict>().map_err(|e| e.to_string())?;
        let out = host.new_dict(indexmap::IndexMap::new());
        remember_from_py(obj, &out);
        let mut map = indexmap::IndexMap::new();
        for (k, v) in dict.iter() {
            let kv = py_to_value(host, py, &k)?;
            let vv = py_to_value(host, py, &v)?;
            let key = host.to_key(&kv)?;
            map.insert(key, (kv, vv));
        }
        if let Some(PyObj::Dict(slot)) = host.get_mut(&out) {
            *slot = map;
        }
        return Ok(out);
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
    // A pythonrs value that crossed OUT through one of the proxy pyclasses is
    // coming back: hand back the ORIGINAL value, not a Foreign handle wrapping
    // the proxy. Without this the round trip mints a new object every time, so
    // `is` fails on anything the stdlib merely stores and returns —
    // `functools.wraps(f)` left `wrapper.__wrapped__ is f` False, and every
    // stdlib API that hands a callback back (a re-raised exception, a registered
    // hook, a memoized function) had the same silent identity break.
    if let Some(v) = unwrap_proxy(obj) {
        return Ok(v);
    }
    // An exception that crossed before is the object it crossed as.
    if let Some(v) = paired_exception(host, obj) {
        return Ok(v);
    }
    // A native class's mirror is the native class; an instance CPython made of
    // one (`pickle` rebuilding it) is an instance of the native class.
    if let Some(cname) = pristine_class(obj, host.generation) {
        return Ok(host.alloc(PyObj::Class(cname)));
    }
    if let Some(cname) = pristine_class(obj.get_type().as_any(), host.generation) {
        return instance_from_mirror(host, py, obj, cname);
    }
    // A CPython builtin type pythonrs implements natively (`int`, `list`, …)
    // comes back as the native type object — the one a bare `int` names — so
    // `dataclasses.fields(dc)[0].type is int` holds and the type constructs
    // native values.
    if let Some(name) = native_builtin_type(py, obj) {
        return Ok(host.builtin_object(name));
    }
    // Anything else stays on the CPython side behind a Foreign handle.
    Ok(host.alloc(PyObj::Foreign(store(obj.clone().unbind()))))
}

/// The native name of `obj` when it IS one of CPython's builtin type objects
/// that pythonrs also implements ([`crate::builtins::BUILTIN_TYPES`]).
fn native_builtin_type(py: Python, obj: &Bound<PyAny>) -> Option<&'static str> {
    if !obj.is_instance_of::<pyo3::types::PyType>() {
        return None;
    }
    let name: String = obj.getattr("__name__").ok()?.extract().ok()?;
    let native = crate::builtins::BUILTIN_TYPES
        .iter()
        .copied()
        .find(|n| *n == name)?;
    let builtin = py.import("builtins").ok()?.getattr(native).ok()?;
    builtin.is(obj).then_some(native)
}

/// The pythonrs value behind a proxy pyclass, if `obj` is one. Every proxy this
/// module hands to CPython holds the `Value` it stands for; this is the single
/// place that reads it back, so a proxy added later only has to be listed here.
fn unwrap_proxy(obj: &Bound<PyAny>) -> Option<Value> {
    if let Ok(c) = obj.downcast::<PyrsCallable>() {
        return Some(c.borrow().target.clone());
    }
    if let Ok(c) = obj.downcast::<PyrsIterator>() {
        return Some(c.borrow().target.clone());
    }
    if let Ok(c) = obj.downcast::<PyrsInstance>() {
        return Some(c.borrow().target.clone());
    }
    if let Ok(c) = obj.downcast::<PyrsFile>() {
        return Some(c.borrow().target.clone());
    }
    if let Ok(c) = obj.downcast::<PyrsRedirectStream>() {
        return Some(c.borrow().target.clone());
    }
    None
}

fn unmarshal_seq<'py, I>(host: &mut PyHost, py: Python, items: I) -> Result<Vec<Value>, String>
where
    I: Iterator<Item = Bound<'py, PyAny>>,
{
    items.map(|it| py_to_value(host, py, &it)).collect()
}

// ── operations routed on a Foreign handle ────────────────────────────────────

/// A value reached THROUGH a bridged object — its attribute or its item.
///
/// A mutable container has to keep its identity here, where
/// [`py_to_value`] would copy it. That copy is right for a call RESULT (a fresh
/// object the caller owns, and arguments are written back by
/// [`writeback_mutated_args`]) and wrong for a reference into a live object:
/// `p.tags is p.tags` was `False` and `p.tags.append(3)` mutated a temporary
/// that was then discarded. Keeping the handle preserves identity and makes the
/// mutation land; the mutators, `len`, iteration, `in`, `repr`, and
/// subscripting all route back through this module.
///
/// Applies at every depth: `d.m['k'].append(2)` reaches the inner list through
/// [`get_item`] on an already-`Foreign` dict, so the item path needs the rule
/// as much as the attribute path.
///
/// Immutable containers (`tuple`, `frozenset`, `bytes`, `str`, scalars) still
/// cross by value — nothing can observe the difference, and operations on them
/// stay native.
fn reference_to_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Result<Value, String> {
    if obj.is_exact_instance_of::<PyList>()
        || obj.is_exact_instance_of::<PyDict>()
        || obj.is_exact_instance_of::<PySet>()
    {
        return Ok(host.alloc(PyObj::Foreign(store(obj.clone().unbind()))));
    }
    py_to_value(host, py, obj)
}

/// `foreign.name` — attribute access (submodules, functions, constants, …).
pub fn get_attr(host: &mut PyHost, id: u32, name: &str) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let attr = obj
            .getattr(name)
            .map_err(|e| pyerr_to_error_h(host, py, &e))?;
        reference_to_value(host, py, &attr)
    })
}

/// `dir(foreign)` — CPython's own `dir()` for a bridged object, so a module's
/// or an instance's real attribute list is what a caller sees. Returns an empty
/// list rather than an error if CPython declines: `dir()` never raises.
pub fn dir_names(id: u32) -> Vec<String> {
    Python::with_gil(|py| {
        let Ok(obj) = fetch(py, id) else {
            return Vec::new();
        };
        obj.dir()
            .ok()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.extract::<String>().ok())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// `foreign.name = value` — set an attribute on a foreign (CPython) object, e.g.
/// `decimal.getcontext().prec = 6`.
pub fn set_attr(host: &mut PyHost, id: u32, name: &str, value: &Value) -> Result<(), String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let v = value_to_py(host, py, value)?;
        obj.setattr(name, v)
            .map_err(|e| pyerr_to_error_h(host, py, &e))
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
        let method = obj.getattr(name).map_err(|e| pyerr_to_error(py, &e))?;
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
        .call(&arg_tuple, kw.as_ref())
        .map_err(|e| pyerr_to_error(py, &e))?;
    with_host(|h| {
        // Reflect any in-place mutation the stdlib call made to a by-value
        // mutable-container argument (`heapq.heapify(lst)`, `random.shuffle(lst)`,
        // `struct.pack_into(fmt, buf, …)`) back into the pythonrs object.
        writeback_mutated_args(h, py, args, &arg_tuple);
        call_result_to_value(h, py, &result, args, &arg_tuple, kw.as_ref())
    })
}

/// Marshal a call's result, keeping identity where the result is not fresh.
///
/// A call that hands back one of its own (by-value) arguments returns the
/// pythonrs original, which [`writeback_mutated_args`] has already updated.
/// Otherwise a mutable container is copied only when the call created it: one
/// that something else still holds (refcount above the result's own
/// reference) is a live object — `catch_warnings(record=True).__enter__()`
/// returns the list `warnings.warn` goes on appending to — and stays behind a
/// handle, like an attribute read ([`reference_to_value`]). The arguments'
/// own marshalled copies are excluded from that test: the argument tuple is
/// what holds them.
fn call_result_to_value(
    host: &mut PyHost,
    py: Python,
    result: &Bound<PyAny>,
    args: &[Value],
    arg_tuple: &Bound<PyTuple>,
    kwargs: Option<&Bound<PyDict>>,
) -> Result<Value, String> {
    if let Some(i) = arg_tuple.iter().position(|a| a.is(result)) {
        if matches!(
            host.get(&args[i]),
            Some(PyObj::List(_) | PyObj::Bytearray(_) | PyObj::Deque { .. })
        ) {
            return Ok(args[i].clone());
        }
        return py_to_value(host, py, result);
    }
    let is_kwarg = kwargs.is_some_and(|d| d.values().iter().any(|v| v.is(result)));
    if !is_kwarg && result.get_refcnt() > 1 {
        return reference_to_value(host, py, result);
    }
    py_to_value(host, py, result)
}

/// The pythonrs mutable-container kinds whose in-place mutation by a CPython
/// stdlib call must be copied back after the call. Immutable arguments
/// (`str`/`tuple`/`frozenset`/`bytes`/scalars), `Foreign` handles (which are the
/// *same* CPython object — mutations are already visible), and callables never
/// need write-back.
#[derive(Clone, Copy)]
enum MutKind {
    List,
    Bytearray,
    Deque(Option<usize>),
}

/// For each positional argument that was marshaled by value as a mutable
/// container, re-read the (possibly mutated) CPython object and overwrite the
/// existing pythonrs heap slot *in place*, so aliases to the same object observe
/// the mutation too. Best-effort: a container whose contents don't round-trip to
/// representable values (a `Foreign` element) is left untouched rather than
/// re-wrapped — that would allocate a fresh handle and is never what an in-place
/// mutator produces in practice.
fn writeback_mutated_args(
    host: &mut PyHost,
    py: Python,
    args: &[Value],
    arg_tuple: &Bound<PyTuple>,
) {
    // Each argument's copy maps back to the argument itself, so a container
    // that holds one of the arguments (itself included) is rebuilt around the
    // original rather than recursing through the cycle.
    let _scope = MemoScope::enter(&FROM_PY_MEMO);
    let mut mutable = Vec::new();
    for (i, orig) in args.iter().enumerate() {
        let kind = match host.get(orig) {
            Some(PyObj::List(_)) => MutKind::List,
            Some(PyObj::Bytearray(_)) => MutKind::Bytearray,
            Some(PyObj::Deque { maxlen, .. }) => MutKind::Deque(*maxlen),
            _ => continue,
        };
        let Ok(cpy) = arg_tuple.get_item(i) else {
            continue;
        };
        remember_from_py(&cpy, orig);
        mutable.push((orig, cpy, kind));
    }
    for (orig, cpy, kind) in mutable {
        if let Some(obj) = rebuild_mutable(host, py, &cpy, kind) {
            if let Some(slot) = host.get_mut(orig) {
                *slot = obj;
            }
        }
    }
}

/// Rebuild the pythonrs `PyObj` for a mutable container from its CPython object
/// after an in-place mutation. Returns `None` (skip write-back) if any element is
/// not representable by value.
fn rebuild_mutable(
    host: &mut PyHost,
    py: Python,
    cpy: &Bound<PyAny>,
    kind: MutKind,
) -> Option<PyObj> {
    match kind {
        MutKind::Bytearray => {
            let ba = cpy.downcast::<PyByteArray>().ok()?;
            Some(PyObj::Bytearray(ba.to_vec()))
        }
        MutKind::List => {
            let items = pure_seq(host, py, cpy)?;
            Some(PyObj::List(items))
        }
        MutKind::Deque(maxlen) => {
            let items = pure_seq(host, py, cpy)?;
            Some(PyObj::Deque {
                items: items.into_iter().collect(),
                maxlen,
            })
        }
    }
}

/// Iterate a CPython container and marshal every element by value, yielding
/// `None` if any element is not representable (so the caller skips write-back).
fn pure_seq(host: &mut PyHost, py: Python, cpy: &Bound<PyAny>) -> Option<Vec<Value>> {
    let it = cpy.try_iter().ok()?;
    let mut out = Vec::new();
    for item in it {
        out.push(pure_value(host, py, &item.ok()?)?);
    }
    Some(out)
}

/// A CPython object → pythonrs `Value` *without* the `Foreign` fallback: returns
/// `None` for anything not representable by value. Used only by write-back, whose
/// contract is "reflect an in-place mutation losslessly, or leave the object
/// alone" — never allocate a new `Foreign` handle (that would leak on every call
/// and change identity). `py_to_value` is the authoritative marshaler and keeps
/// unrepresentable results as `Foreign`; the two contracts differ, so they stay
/// separate functions.
fn pure_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Option<Value> {
    // A container this write-back already reached (an argument itself, when a
    // list contains itself) is that same pythonrs object.
    let key = obj.as_ptr() as usize;
    if let Some(v) = FROM_PY_MEMO.with(|m| m.borrow().seen.get(&key).cloned()) {
        return Some(v);
    }
    if obj.is_none() {
        return Some(Value::Undef);
    }
    if obj.is_exact_instance_of::<PyBool>() {
        return obj.extract::<bool>().ok().map(Value::Bool);
    }
    if obj.is_exact_instance_of::<PyInt>() {
        return match obj.extract::<i64>() {
            Ok(n) => Some(Value::Int(n)),
            Err(_) => big_from_py(obj).ok().map(|b| host.alloc(PyObj::BigInt(b))),
        };
    }
    if obj.is_exact_instance_of::<PyFloat>() {
        return float_from_py(obj).ok().map(Value::Float);
    }
    if obj.is_exact_instance_of::<PyString>() {
        return obj.extract::<String>().ok().map(|s| host.new_str(s));
    }
    if obj.is_exact_instance_of::<PyBytes>() {
        let b = obj.downcast::<PyBytes>().ok()?;
        return Some(host.alloc(PyObj::Bytes(b.as_bytes().to_vec())));
    }
    if obj.is_exact_instance_of::<PyByteArray>() {
        let b = obj.downcast::<PyByteArray>().ok()?;
        return Some(host.alloc(PyObj::Bytearray(b.to_vec())));
    }
    if obj.is_exact_instance_of::<PyList>() {
        let out = host.new_list(Vec::new());
        remember_from_py(obj, &out);
        let items = pure_seq(host, py, obj)?;
        if let Some(PyObj::List(slot)) = host.get_mut(&out) {
            *slot = items;
        }
        return Some(out);
    }
    if obj.is_exact_instance_of::<PyTuple>() {
        let items = pure_seq(host, py, obj)?;
        return Some(host.new_tuple(items));
    }
    None
}

#[allow(clippy::type_complexity)]
fn build_call_args<'py>(
    host: &PyHost,
    py: Python<'py>,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<(Bound<'py, PyTuple>, Option<Bound<'py, PyDict>>), String> {
    // One marshal for the whole call, so an object passed twice is one object.
    let _scope = MemoScope::enter(&TO_PY_MEMO);
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

// A fusevm-side callable (lambda / def / builtin / …) exposed to CPython so it
// can be used as a stdlib callback. `__call__` marshals the CPython arguments to
// pythonrs values, runs the callable on fusevm (no host borrow held here), and
// marshals the result back. (Plain `//`, not `///`: a doc comment would become
// the pyclass `__doc__` and leak as every wrapped callable's `__doc__`.)
// `dict` gives each proxy a `__dict__`, so CPython code can set attributes on it
// (`functools.update_wrapper` does `setattr(wrapper, '__module__', …)` and
// `wrapper.__dict__.update(...)`). Attributes it doesn't set fall through to
// `__getattr__`, which delegates the wrapped callable's dunders.
#[pyclass(dict)]
struct PyrsCallable {
    target: Value,
    // `__doc__` and `__module__` need their own slots. Every pyclass ALREADY
    // answers both from its type (`None` and `"builtins"`), so normal lookup
    // succeeds and `__getattr__` never fires for those two names —
    // `functools.wraps(f)` therefore copied `None` over the wrapped function's
    // docstring and `"builtins"` over its module. A getset pair intercepts both
    // directions: the getter delegates to the target until something assigns.
    // Every other function dunder (`__name__`, `__qualname__`, …) is absent
    // from the pyclass type and reaches `__getattr__` normally.
    doc: Option<Py<PyAny>>,
    module: Option<Py<PyAny>>,
}

#[pymethods]
impl PyrsCallable {
    /// The wrapped callable's `__doc__`, or whatever was last assigned here.
    #[getter(__doc__)]
    fn get_doc(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.shadowed_dunder(py, self.doc.as_ref(), "__doc__")
    }

    /// `functools.update_wrapper` assigns `__doc__` directly; without a setter
    /// the getset descriptor would make that an `AttributeError`.
    #[setter(__doc__)]
    fn set_doc(&mut self, value: Py<PyAny>) {
        self.doc = Some(value);
    }

    /// The wrapped callable's `__module__` (`'__main__'` for a script-level
    /// `def`), not the pyclass's own `'builtins'`.
    #[getter(__module__)]
    fn get_module(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.shadowed_dunder(py, self.module.as_ref(), "__module__")
    }

    #[setter(__module__)]
    fn set_module(&mut self, value: Py<PyAny>) {
        self.module = Some(value);
    }
    /// Delegate a missing attribute (function dunder like `__name__` /
    /// `__qualname__` / `__module__`) to the wrapped fusevm callable, so
    /// `functools.update_wrapper` can copy them off it. `__getattr__` runs only
    /// after normal lookup (including the instance `__dict__`) misses, so a
    /// wraps-assigned attribute wins over the delegate. A dunder the target
    /// lacks becomes `AttributeError` (which `update_wrapper` silently skips).
    fn __getattr__(&self, py: Python, name: String) -> PyResult<Py<PyAny>> {
        // Through the descriptor-aware read, outside the host borrow: a lazy
        // `__annotations__` runs user code, and its `NameError` must reach
        // CPython as itself rather than as a missing attribute.
        let outer = with_host(|h| h.exc.clone());
        match crate::builtins::raw_getattr(&self.target, &name) {
            Ok(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| b.unbind())
                .map_err(pyo3::exceptions::PyRuntimeError::new_err),
            Err(e) if !e.starts_with("AttributeError") => Err(call_err(e, outer)),
            Err(e) => Err(pyo3::exceptions::PyAttributeError::new_err(e)),
        }
    }

    /// Write an assigned attribute THROUGH to the wrapped pythonrs callable
    /// instead of into the proxy's own `__dict__`.
    ///
    /// `functools.wraps` is the case that forces this: it does
    /// `setattr(wrapper, '__name__' / '__qualname__' / '__doc__' / '__wrapped__',
    /// …)` and then RETURNS the wrapper. Stored on the proxy, every one of those
    /// assignments died with the proxy the moment the value crossed back — the
    /// decorated function kept its own `__name__` and had no `__wrapped__` at
    /// all. Writing through makes the mutation land on the object the caller
    /// actually keeps, which is what CPython's in-place semantics mean.
    ///
    /// A target that cannot hold attributes (a builtin) reports the host's
    /// AttributeError, exactly as assigning to `len.x` does in CPython.
    fn __setattr__(&self, py: Python, name: String, value: Bound<PyAny>) -> PyResult<()> {
        let v = with_host(|h| py_to_value(h, py, &value))
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
        with_host(|h| h.set_attr(&self.target, &name, v))
            .map_err(pyo3::exceptions::PyAttributeError::new_err)
    }

    /// Descriptor protocol: a pythonrs function stored in a CPython-built class
    /// (an `enum`/`dataclass`/`NamedTuple` method) binds `self` on instance
    /// access, and — because it now has `__get__` — CPython recognizes it as a
    /// method rather than a plain attribute (Enum's `_EnumDict` would otherwise
    /// make it a member). Class access (`obj is None`) yields the unbound proxy.
    fn __get__<'py>(
        slf: Bound<'py, Self>,
        py: Python<'py>,
        obj: Option<Bound<'py, PyAny>>,
        _owner: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // `classmethod.__get__` binds the owner (or `type(obj)`), and
        // `staticmethod.__get__` binds nothing, on instance and class access alike.
        let kind = with_host(|h| match h.get(&slf.borrow().target) {
            Some(PyObj::ClassMethod(_)) => Some(true),
            Some(PyObj::StaticMethod(_)) => Some(false),
            _ => None,
        });
        match kind {
            Some(true) => {
                let cls = match (_owner, obj) {
                    (Some(owner), _) if !owner.is_none() => owner,
                    (_, Some(instance)) if !instance.is_none() => instance.get_type().into_any(),
                    _ => return Ok(slf.into_any()),
                };
                return py.import("types")?.getattr("MethodType")?.call1((slf, cls));
            }
            Some(false) => return Ok(slf.into_any()),
            None => {}
        }
        match obj {
            Some(instance) if !instance.is_none() => py
                .import("types")?
                .getattr("MethodType")?
                .call1((slf, instance)),
            _ => Ok(slf.into_any()),
        }
    }

    /// How CPython pickles a callable: a function or builtin BY NAME — its
    /// qualified name, looked up again in its module on load (the proxy is the
    /// one object that lookup finds) — and a bound method as `getattr(obj,
    /// name)`, as `method.__reduce__` does.
    fn __reduce__(&self, py: Python) -> PyResult<Py<PyAny>> {
        let bound = with_host(|h| match h.get(&self.target) {
            Some(PyObj::BoundMethod { recv, func }) => Some((recv.clone(), func.clone())),
            _ => None,
        });
        if let Some((recv, func)) = bound {
            let name = run_for_cpython(|| crate::builtins::raw_getattr(&func, "__name__"))?;
            let getattr = py.import("builtins")?.getattr("getattr")?;
            let args = with_host(|h| -> Result<_, String> {
                Ok((value_to_py(h, py, &recv)?, value_to_py(h, py, &name)?))
            })
            .map_err(rs_err)?;
            return Ok((getattr, args).into_pyobject(py)?.into_any().unbind());
        }
        let qualname = run_for_cpython(|| crate::builtins::raw_getattr(&self.target, "__qualname__"))?;
        with_host(|h| value_to_py(h, py, &qualname))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }

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
        let result = run_for_cpython(|| crate::host::invoke(&self.target, rs_args, rs_kwargs))?;
        // Marshal the result back to a CPython object (host borrow window).
        with_host(|h| value_to_py(h, py, &result))
            .map(|b| b.unbind())
            .map_err(to_pyerr)
    }
}

impl PyrsCallable {
    /// Read a dunder the pyclass type already shadows: the assigned override if
    /// there is one, else the wrapped callable's own value, else `None` (an
    /// `AttributeError` here would be wrong — CPython's every function has both
    /// `__doc__` and `__module__`).
    fn shadowed_dunder(
        &self,
        py: Python,
        override_value: Option<&Py<PyAny>>,
        name: &str,
    ) -> PyResult<Py<PyAny>> {
        if let Some(v) = override_value {
            return Ok(v.clone_ref(py));
        }
        match with_host(|h| h.get_attr(&self.target, name)) {
            Ok(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| b.unbind())
                .map_err(pyo3::exceptions::PyRuntimeError::new_err),
            Err(_) => Ok(py.None()),
        }
    }
}

// A CPython iterator backed by a pythonrs lazy iterator (generator / zip / map /
// filter / enumerate / composite). `__next__` advances fusevm one step with NO
// host borrow held (`iter_step` manages its own borrows and may re-enter
// pythonrs), so CPython can consume `itertools.takewhile(pred, gen())` and the
// like without materializing an (possibly infinite) source. Plain `//` (not
// `///`) so the doc text doesn't become a leaking `__doc__`.
#[pyclass]
struct PyrsIterator {
    target: Value,
}

#[pymethods]
impl PyrsIterator {
    fn __iter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    fn __next__(&self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        match crate::host::iter_step(&self.target).map_err(rs_err_typed)? {
            // `None` from `__next__` raises `StopIteration` in pyo3.
            None => Ok(None),
            Some(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| Some(b.unbind()))
                .map_err(rs_err),
        }
    }

    // `gen.send(value)` — resume the wrapped pythonrs generator with `value`.
    // The stdlib drives a generator this way whenever it owns the pull side:
    // `contextlib.contextmanager`'s `__enter__` is `next(self.gen)`, and any
    // coroutine-style helper sends into it.
    #[pyo3(signature = (value = None))]
    fn send(&self, py: Python, value: Option<Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        self.require_generator("send")?;
        let sent = match value {
            None => Value::Undef,
            Some(v) => with_host(|h| py_to_value(h, py, &v)).map_err(rs_err)?,
        };
        if !crate::host::gen_started(&self.target) && !matches!(sent, Value::Undef) {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "can't send non-None value to a just-started generator",
            ));
        }
        let outer = with_host(|h| h.exc.clone());
        self.finish_step(py, crate::host::gen_resume(&self.target, sent), outer)
    }

    // `gen.throw(exc)` / the legacy `gen.throw(type, value, tb)` — raise `exc` at
    // the generator's current `yield`. This is the whole mechanism behind
    // `@contextlib.contextmanager`: `_GeneratorContextManager.__exit__` throws
    // the body's exception into the generator so a `try/except` around the
    // `yield` can handle it, and reads `StopIteration` back as "suppressed".
    // Without it a `with cm():` block that raised died with `AttributeError:
    // 'builtins.PyrsIterator' object has no attribute 'throw'`.
    #[pyo3(signature = (*args))]
    fn throw(&self, py: Python, args: &Bound<'_, PyTuple>) -> PyResult<Py<PyAny>> {
        self.require_generator("throw")?;
        let exc = normalize_throw_args(py, args)?;
        let exc_v = pyexc_to_value(py, &exc)?;
        let outer = with_host(|h| h.exc.clone());
        self.finish_step(py, crate::host::gen_throw(&self.target, exc_v), outer)
    }

    // `gen.close()` — throw `GeneratorExit` in and require the body to finish.
    // `contextlib.closing(gen)` and `ExitStack` both call it, as does CPython's
    // own generator finalization.
    fn close(&self) -> PyResult<()> {
        self.require_generator("close")?;
        if with_host(|h| h.close_unstarted_gen(&self.target)) {
            return Ok(());
        }
        let outer = with_host(|h| h.exc.clone());
        let ge = with_host(|h| {
            h.alloc(PyObj::Exception {
                class: "GeneratorExit".into(),
                args: vec![],
            })
        });
        match crate::host::gen_throw(&self.target, ge) {
            Ok(Some(_)) => {
                hand_off_error(outer);
                Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "generator ignored GeneratorExit",
                ))
            }
            Ok(None) => Ok(()),
            // `GeneratorExit` (or a clean `StopIteration`) reaching the top is the
            // normal outcome of `close()`; CPython swallows both.
            Err(e) if e.contains("GeneratorExit") || e.contains("StopIteration") => {
                hand_off_error(outer);
                Ok(())
            }
            Err(e) => Err(self.body_err(e, outer)),
        }
    }
}

impl PyrsIterator {
    /// The CPython exception for an error raised by the wrapped generator's body,
    /// taken from the parked exception OBJECT when there is one (so `args` — and
    /// therefore `str(exc)` — survive the crossing) and from the error string
    /// otherwise. Clears the pythonrs-side error last: ownership moves to CPython.
    fn body_err(&self, e: String, outer: Option<Value>) -> pyo3::PyErr {
        let err = crate::host::gen_pending_exc(&self.target)
            .as_ref()
            .and_then(exc_value_to_pyerr)
            .unwrap_or_else(|| rs_err_typed(e));
        hand_off_error(outer);
        err
    }

    /// `send`/`throw`/`close` exist on CPython generators only. A pythonrs `zip`/
    /// `map`/`filter`/`enumerate` object reaches CPython through the same wrapper,
    /// and asking one for `.throw` must raise `AttributeError` exactly as it does
    /// on the real `zip` object.
    fn require_generator(&self, name: &str) -> PyResult<()> {
        if with_host(|h| matches!(h.get(&self.target), Some(PyObj::Generator { .. }))) {
            return Ok(());
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'{}' object has no attribute '{name}'",
            with_host(|h| h.type_name(&self.target))
        )))
    }

    /// Shared tail of `send`/`throw`: a yielded value crosses back, an exhausted
    /// generator becomes `StopIteration(return_value)`, and a body exception
    /// becomes the CPython exception of the same class.
    fn finish_step(
        &self,
        py: Python,
        step: Result<Option<Value>, String>,
        outer: Option<Value>,
    ) -> PyResult<Py<PyAny>> {
        match step {
            Ok(Some(v)) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| b.unbind())
                .map_err(rs_err),
            Ok(None) => {
                let ret = crate::host::coro_return_value(&self.target);
                let arg = match ret {
                    Value::Undef => None,
                    v => Some(with_host(|h| value_to_py(h, py, &v)).map_err(rs_err)?),
                };
                Err(match arg {
                    Some(a) => pyo3::exceptions::PyStopIteration::new_err((a.unbind(),)),
                    None => pyo3::exceptions::PyStopIteration::new_err(()),
                })
            }
            Err(e) => Err(self.body_err(e, outer)),
        }
    }
}

/// Normalize `throw`'s argument forms to a single CPython exception INSTANCE:
/// `throw(instance)` (the only form CPython's own stdlib uses since 3.13) and the
/// legacy `throw(type[, value[, tb]])`.
fn normalize_throw_args<'py>(
    py: Python<'py>,
    args: &Bound<'py, PyTuple>,
) -> PyResult<Bound<'py, PyAny>> {
    let first = args.get_item(0).map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err("throw() takes at least 1 argument (0 given)")
    })?;
    let base_exc = py.get_type::<pyo3::exceptions::PyBaseException>();
    // `throw(SomeError, ...)`: instantiate unless arg 1 already is an instance.
    let is_exc_class = first
        .downcast::<pyo3::types::PyType>()
        .ok()
        .map(|t| t.is_subclass(&base_exc).unwrap_or(false))
        .unwrap_or(false);
    if is_exc_class {
        let second = args.get_item(1).ok().filter(|v| !v.is_none());
        return match second {
            Some(v) if v.is_instance(&first)? => Ok(v),
            Some(v) if v.is_instance_of::<PyTuple>() => {
                first.call1(v.downcast::<PyTuple>()?.clone())
            }
            Some(v) => first.call1((v,)),
            None => first.call0(),
        };
    }
    if !first.is_instance(&base_exc)? {
        return Err(pyo3::exceptions::PyTypeError::new_err(
            "exceptions must derive from BaseException",
        ));
    }
    Ok(first)
}

/// `raise obj` for a CPython object held at `id`: an exception instance is
/// raised as the pythonrs exception it is paired with (see [`exc_to_py`]), an
/// exception class is instantiated first (`raise struct.error`), and anything
/// else is CPython's `TypeError`.
pub fn raised_foreign(id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let exc = if is_exception_type(py, &obj) {
            obj.call0().map_err(|e| pyerr_to_error(py, &e))?
        } else {
            obj
        };
        let base_exc = py.get_type::<pyo3::exceptions::PyBaseException>();
        if !exc.is_instance(&base_exc).unwrap_or(false) {
            return Err(crate::host::type_error("exceptions must derive from BaseException"));
        }
        pyexc_to_value(py, &exc).map_err(|e| e.to_string())
    })
}

/// CPython's own rendering of the frames the exception at ffi handle `handle`
/// passed through on the CPython side — `traceback.format_tb` over its
/// `__traceback__` — or `None` when it has none.
pub fn traceback_text(handle: u32) -> Option<String> {
    Python::with_gil(|py| {
        let exc = fetch(py, handle).ok()?;
        let tb = exc.getattr("__traceback__").ok().filter(|tb| !tb.is_none())?;
        let lines = py
            .import("traceback")
            .and_then(|m| m.getattr("format_tb"))
            .and_then(|f| f.call1((tb,)))
            .ok()?;
        let lines: Vec<String> = lines.extract().ok()?;
        Some(lines.concat())
    })
}

/// The name a traceback's last line gives the type of the CPython exception at
/// ffi handle `handle`: `module.qualname`, without a `builtins` or `__main__`
/// module (`traceback.TracebackException.format_exception_only`).
pub fn exception_type_name(handle: u32) -> Option<String> {
    Python::with_gil(|py| {
        let ty = fetch(py, handle).ok()?.get_type();
        let qualname: String = ty.getattr("__qualname__").ok()?.extract().ok()?;
        let module: Option<String> = ty.getattr("__module__").ok().and_then(|m| m.extract().ok());
        Some(match module {
            Some(m) if m != "builtins" && m != "__main__" => format!("{m}.{qualname}"),
            _ => qualname,
        })
    })
}

/// A CPython exception instance as the pythonrs exception value a fusevm
/// `except` clause can match. The type's `__mro__` is registered first so a
/// pythonrs `except ValueError` catches a thrown `json.JSONDecodeError`.
fn pyexc_to_value(py: Python, exc: &Bound<PyAny>) -> PyResult<Value> {
    if let Some(v) = with_host(|h| paired_exception(h, exc)) {
        return Ok(v);
    }
    let ty = exc.get_type();
    let class = ty
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "Exception".into());
    let mut bases: Vec<String> = Vec::new();
    if let Ok(mro) = ty.getattr("__mro__") {
        if let Ok(seq) = mro.try_iter() {
            for c in seq.flatten() {
                if let Ok(n) = c.getattr("__name__").and_then(|n| n.extract::<String>()) {
                    bases.push(n);
                }
            }
        }
    }
    let args: Vec<Bound<PyAny>> = match exc.getattr("args").and_then(|a| a.try_iter()) {
        Ok(it) => it.flatten().collect(),
        Err(_) => Vec::new(),
    };
    with_host(|h| {
        if !bases.is_empty() {
            h.foreign_exc_bases.insert(class.clone(), bases);
        }
        let args = args
            .iter()
            .map(|a| py_to_value(h, py, a))
            .collect::<Result<Vec<_>, _>>()?;
        let v = h.alloc(PyObj::Exception { class, args });
        pair_exception(h, &v, exc, store(exc.clone().unbind()), true);
        Ok::<Value, String>(v)
    })
    .map_err(rs_err)
}

/// A pythonrs exception OBJECT as the CPython exception it crosses as (see
/// [`exc_to_py`]). `None` when the value is not an exception or cannot cross —
/// the caller then falls back to parsing the terse error string.
fn exc_value_to_pyerr(v: &Value) -> Option<pyo3::PyErr> {
    Python::with_gil(|py| {
        let exc = with_host(|h| exc_to_py(h, py, v)).ok()??;
        Some(pyo3::PyErr::from_value(exc))
    })
}

/// Map a pythonrs error raised by USER CODE running under a CPython caller to the
/// CPython exception of the same class.
///
/// Every wrapper used to answer `PyRuntimeError(e)`, so a `raise KeyError(k)` or a
/// failed `assertEqual` arrived on the CPython side as
/// `RuntimeError: KeyError: 'k'`. Anything that branches on the class was then
/// wrong: `unittest` reported a failed assertion as an ERROR rather than a FAIL,
/// and a caller's `except KeyError` did not fire at all.
///
/// Same two steps as [`PyrsCallable::body_err`], which already did this for
/// generator bodies: rebuild from the live exception object when it is the one
/// that just propagated (its class must head the error string, so a stale `h.exc`
/// from an earlier caught exception cannot claim this failure), else parse the
/// class out of pythonrs's terse `"Class: message"` rendering.
fn call_err(e: String, outer: Option<Value>) -> pyo3::PyErr {
    let pending = with_host(|h| h.exc.clone());
    let class = pending.as_ref().and_then(|v| {
        with_host(|h| match h.get(v) {
            Some(PyObj::Exception { class, .. }) => Some(class.clone()),
            Some(PyObj::Instance(i)) if h.class_is_exception(&i.class) => Some(h.type_name(v)),
            _ => None,
        })
    });
    let from_value = match class {
        Some(c) if e == c || e.starts_with(&format!("{c}: ")) => {
            pending.as_ref().and_then(exc_value_to_pyerr)
        }
        _ => None,
    };
    let err = from_value.unwrap_or_else(|| rs_err_typed(e));
    hand_off_error(outer);
    err
}

/// Run pythonrs code on behalf of a CPython caller: an error it raises leaves
/// as the CPython exception of its class ([`call_err`]).
fn run_for_cpython<T>(f: impl FnOnce() -> Result<T, String>) -> PyResult<T> {
    let outer = with_host(|h| h.exc.clone());
    f().map_err(|e| call_err(e, outer))
}

/// Drop the pythonrs-side error state once its exception has been handed to
/// CPython as a `PyErr`. Ownership moves with it: leaving `h.error` set would
/// make the next fusevm step abort on an exception CPython is already carrying.
/// The exception the pythonrs side was HANDLING when CPython called in
/// (`outer`) is handled again: the error belonged to the callee, and a bare
/// `raise` in the handler that called out — the one a `with` statement
/// re-raises through after `__exit__` declines — must still find its own.
fn hand_off_error(outer: Option<Value>) {
    with_host(|h| {
        h.error = None;
        h.exc = outer;
    });
}

// A CPython view of a pythonrs instance: attribute/item access, comparison,
// hashing, and repr/str route back to the fusevm object, so an instance can be
// passed into a CPython call (`operator.attrgetter("x")(obj)`, `sorted(objs,
// key=itemgetter(0))`, a custom `json.dumps` default). Each host call runs with
// no host borrow held (CPython invokes these outside the marshalling window).
#[pyclass(subclass)]
struct PyrsInstance {
    target: Value,
}

#[pymethods]
impl PyrsInstance {
    /// The object's class as CPython sees it — the class's mirror — so
    /// `pickle`'s `obj.__class__ is cls` check, `isinstance` against the
    /// mirror and `type(obj).__name__`-style reporting through `__class__`
    /// all name the program's class rather than the proxy type.
    #[getter(__class__)]
    fn get_class(&self, py: Python) -> PyResult<Py<PyAny>> {
        let class = with_host(|h| h.get_attr(&self.target, "__class__")).map_err(rs_err)?;
        with_host(|h| value_to_py(h, py, &class))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }

    /// `obj.__reduce_ex__(protocol)` as the native object answers it — the
    /// class's own `__reduce_ex__`/`__reduce__`/`__getstate__` or `object`'s —
    /// so `pickle` and `copyreg` serialize the program's object, with its
    /// class crossing as the mirror `__class__` names.
    fn __reduce_ex__(&self, py: Python, protocol: Bound<PyAny>) -> PyResult<Py<PyAny>> {
        let protocol = with_host(|h| py_to_value(h, py, &protocol)).map_err(rs_err)?;
        let r = run_for_cpython(|| {
            crate::host::call_method(&self.target, "__reduce_ex__", vec![protocol], vec![])
        })?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }

    fn __reduce__(&self, py: Python) -> PyResult<Py<PyAny>> {
        let r = run_for_cpython(|| crate::host::call_method(&self.target, "__reduce__", vec![], vec![]))?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }

    /// An attribute CPython code assigns lands on the native object.
    fn __setattr__(&self, py: Python, name: String, value: Bound<PyAny>) -> PyResult<()> {
        let v = with_host(|h| py_to_value(h, py, &value)).map_err(rs_err)?;
        let name_v = with_host(|h| h.new_str(name));
        run_for_cpython(|| {
            crate::builtins::call_builtin_function("setattr", vec![self.target.clone(), name_v, v], vec![])
        })
        .map(|_| ())
    }

    fn __delattr__(&self, name: String) -> PyResult<()> {
        let name_v = with_host(|h| h.new_str(name));
        run_for_cpython(|| {
            crate::builtins::call_builtin_function("delattr", vec![self.target.clone(), name_v], vec![])
        })
        .map(|_| ())
    }

    fn __getattr__(&self, py: Python, name: String) -> PyResult<Py<PyAny>> {
        match with_host(|h| h.get_attr(&self.target, &name)) {
            Ok(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| b.unbind())
                .map_err(pyo3::exceptions::PyRuntimeError::new_err),
            Err(e) => Err(pyo3::exceptions::PyAttributeError::new_err(e)),
        }
    }

    fn __getitem__(&self, py: Python, key: Bound<PyAny>) -> PyResult<Py<PyAny>> {
        let to_pyerr = |e: String| pyo3::exceptions::PyRuntimeError::new_err(e);
        let key_v = with_host(|h| py_to_value(h, py, &key)).map_err(to_pyerr)?;
        let r = run_for_cpython(|| {
            crate::host::call_method(&self.target, "__getitem__", vec![key_v], vec![])
        })?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(to_pyerr)
    }

    fn __richcmp__(
        &self,
        py: Python,
        other: Bound<PyAny>,
        op: pyo3::pyclass::CompareOp,
    ) -> PyResult<Py<PyAny>> {
        use fusevm::NumOp;
        let to_pyerr = |e: String| pyo3::exceptions::PyRuntimeError::new_err(e);
        let other_v = with_host(|h| py_to_value(h, py, &other)).map_err(to_pyerr)?;
        let numop = match op {
            pyo3::pyclass::CompareOp::Lt => NumOp::Lt,
            pyo3::pyclass::CompareOp::Le => NumOp::Le,
            pyo3::pyclass::CompareOp::Eq => NumOp::Eq,
            pyo3::pyclass::CompareOp::Ne => NumOp::Ne,
            pyo3::pyclass::CompareOp::Gt => NumOp::Gt,
            pyo3::pyclass::CompareOp::Ge => NumOp::Ge,
        };
        let r = crate::builtins::numeric_hook(numop, &self.target, &other_v).map_err(to_pyerr)?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(to_pyerr)
    }

    fn __hash__(&self) -> PyResult<isize> {
        with_host(|h| h.to_key(&self.target))
            .map(|k| crate::builtins::hash_key(&k) as isize)
            .map_err(pyo3::exceptions::PyTypeError::new_err)
    }

    fn __repr__(&self) -> PyResult<String> {
        crate::builtins::py_repr(&self.target).map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }

    fn __str__(&self) -> PyResult<String> {
        crate::builtins::py_str(&self.target).map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }
}

// A [`PyrsInstance`] whose class defines `__index__`: the one slot CPython
// reads to take an object as an integer. The method runs on the fusevm side
// and its result is checked as CPython's `PyNumber_Index` checks it.
#[pyclass(extends = PyrsInstance)]
struct PyrsIndexInstance;

#[pymethods]
impl PyrsIndexInstance {
    fn __index__(slf: PyRef<Self>, py: Python) -> PyResult<Py<PyAny>> {
        let target = slf.as_super().target.clone();
        let r = run_for_cpython(|| crate::builtins::index_dunder(&target))?
            // The class lost `__index__` after the object crossed.
            .ok_or_else(|| {
                let tn = with_host(|h| h.type_name(&target));
                pyo3::exceptions::PyTypeError::new_err(format!(
                    "'{tn}' object cannot be interpreted as an integer"
                ))
            })?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }
}

// A CPython file-like view of a pythonrs `open()` handle, so a native file can
// be handed to a stdlib call that reads or writes it (`json.dump(cfg, f)`,
// `csv.writer(f)`, `csv.DictReader(f)`, `shutil.copyfileobj`). Every method
// routes back to the same `file_method` the interpreter uses, with no host
// borrow held (CPython calls these outside the marshalling window). Plain `//`
// so the doc text doesn't become a leaking `__doc__`.
// The binary layer under the embedded interpreter's `sys.stdout`/`sys.stderr`
// (see `route_std_streams`): every byte its write-through `TextIOWrapper` hands
// down goes into pythonrs's own stream, so both interpreters share one buffer.
// It answers the probes `TextIOWrapper` and stream users make of a buffer
// (`readable`/`writable`/`seekable`/`closed`/`fileno`/`isatty`/`name`).
#[pyclass]
struct PyrsStdStream {
    stream: crate::stdio::Stream,
}

impl PyrsStdStream {
    fn fd(&self) -> i32 {
        match self.stream {
            crate::stdio::Stream::Stdout => 1,
            crate::stdio::Stream::Stderr => 2,
        }
    }
}

#[pymethods]
impl PyrsStdStream {
    fn write(&self, data: &[u8]) -> usize {
        crate::stdio::write_bytes(self.stream, data);
        data.len()
    }

    fn flush(&self) {
        crate::stdio::flush(self.stream);
    }

    fn close(&self) {
        self.flush();
    }

    fn fileno(&self) -> i32 {
        self.fd()
    }

    fn isatty(&self) -> bool {
        // SAFETY: `isatty` only queries the descriptor.
        unsafe { libc::isatty(self.fd()) == 1 }
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        true
    }

    fn seekable(&self) -> bool {
        false
    }

    #[getter]
    fn closed(&self) -> bool {
        false
    }

    #[getter]
    fn name(&self) -> &'static str {
        match self.stream {
            crate::stdio::Stream::Stdout => "<stdout>",
            crate::stdio::Stream::Stderr => "<stderr>",
        }
    }
}

// The embedded interpreter's `sys.stdout`/`sys.stderr` while pythonrs has it
// redirected to one of its own values (`redirect_stdout(writer)`,
// `sys.stdout = writer`): a text stream whose writes go to that value. It is
// bound to the host that owns the value — a write from another thread, after
// that host was reset, or while it is mid-borrow (CPython printing during a
// marshalling window) goes to the native stream instead of resolving the
// value against the wrong heap.
#[pyclass]
struct PyrsRedirectStream {
    target: Value,
    stderr: bool,
    thread: std::thread::ThreadId,
    generation: u64,
}

impl PyrsRedirectStream {
    fn usable(&self) -> bool {
        std::thread::current().id() == self.thread
            && crate::host::try_with_host(|h| h.generation == self.generation).unwrap_or(false)
    }

    fn native(&self) -> crate::stdio::Stream {
        if self.stderr {
            crate::stdio::Stream::Stderr
        } else {
            crate::stdio::Stream::Stdout
        }
    }
}

#[pymethods]
impl PyrsRedirectStream {
    fn write(&self, text: &str) -> PyResult<usize> {
        if self.usable() {
            run_for_cpython(|| crate::host::write_to_stream(&self.target, text))?;
        } else {
            crate::stdio::write_bytes(self.native(), text.as_bytes());
        }
        Ok(text.chars().count())
    }

    // A target with only `write` is a valid stream and has nothing to flush.
    fn flush(&self) -> PyResult<()> {
        if !self.usable() {
            crate::stdio::flush(self.native());
            return Ok(());
        }
        if with_host(|h| h.get_attr(&self.target, "flush")).is_ok() {
            run_for_cpython(|| crate::host::call_method(&self.target, "flush", vec![], vec![]))?;
        }
        Ok(())
    }

    // Everything else a stream user reads (`encoding`, `getvalue`, `isatty`)
    // is the target's own attribute.
    fn __getattr__(&self, py: Python, name: String) -> PyResult<Py<PyAny>> {
        if !self.usable() {
            return Err(pyo3::exceptions::PyAttributeError::new_err(name));
        }
        let v = with_host(|h| h.get_attr(&self.target, &name)).map_err(rs_err_typed)?;
        with_host(|h| value_to_py(h, py, &v))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }
}

#[pyclass]
struct PyrsFile {
    target: Value,
}

impl PyrsFile {
    /// Call one file method on the wrapped handle and marshal the result back.
    fn call(&self, py: Python, name: &str, args: Vec<Value>) -> PyResult<Py<PyAny>> {
        let r = run_for_cpython(|| crate::host::call_method(&self.target, name, args, vec![]))?;
        with_host(|h| value_to_py(h, py, &r))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }
}

#[pymethods]
impl PyrsFile {
    #[pyo3(signature = (size=None))]
    fn read(&self, py: Python, size: Option<i64>) -> PyResult<Py<PyAny>> {
        self.call(py, "read", vec![size.map_or(Value::Undef, Value::Int)])
    }

    fn readline(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "readline", vec![])
    }

    fn readlines(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "readlines", vec![])
    }

    fn write(&self, py: Python, data: Bound<PyAny>) -> PyResult<Py<PyAny>> {
        let v = with_host(|h| py_to_value(h, py, &data)).map_err(rs_err)?;
        self.call(py, "write", vec![v])
    }

    fn writelines(&self, py: Python, lines: Bound<PyAny>) -> PyResult<Py<PyAny>> {
        let v = with_host(|h| py_to_value(h, py, &lines)).map_err(rs_err)?;
        self.call(py, "writelines", vec![v])
    }

    fn flush(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "flush", vec![])
    }

    fn close(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "close", vec![])
    }

    fn tell(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "tell", vec![])
    }

    #[pyo3(signature = (offset, whence=0))]
    fn seek(&self, py: Python, offset: i64, whence: i64) -> PyResult<Py<PyAny>> {
        self.call(py, "seek", vec![Value::Int(offset), Value::Int(whence)])
    }

    fn fileno(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "fileno", vec![])
    }

    fn readable(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "readable", vec![])
    }

    fn writable(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "writable", vec![])
    }

    fn seekable(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "seekable", vec![])
    }

    fn isatty(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.call(py, "isatty", vec![])
    }

    #[getter]
    fn name(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.attr(py, "name")
    }

    #[getter]
    fn mode(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.attr(py, "mode")
    }

    #[getter]
    fn closed(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.attr(py, "closed")
    }

    #[getter]
    fn encoding(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.attr(py, "encoding")
    }

    #[getter]
    fn newlines(&self, py: Python) -> PyResult<Py<PyAny>> {
        self.attr(py, "newlines")
    }

    fn __iter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    // Line iteration for `csv.reader(f)` / `csv.DictReader(f)`: an empty line
    // from `readline` is EOF, which pyo3 turns into `StopIteration`.
    fn __next__(&self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        let line =
            crate::host::call_method(&self.target, "readline", vec![], vec![]).map_err(rs_err)?;
        let text = with_host(|h| h.str_of(&line));
        if text.is_empty() {
            return Ok(None);
        }
        with_host(|h| value_to_py(h, py, &line))
            .map(|b| Some(b.unbind()))
            .map_err(rs_err)
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python, _args: &Bound<PyTuple>) -> PyResult<bool> {
        self.call(py, "close", vec![])?;
        Ok(false)
    }

    fn __repr__(&self) -> PyResult<String> {
        crate::builtins::py_repr(&self.target).map_err(rs_err)
    }
}

impl PyrsFile {
    /// Read one data attribute (`name`/`mode`/`closed`/…) off the handle.
    fn attr(&self, py: Python, name: &str) -> PyResult<Py<PyAny>> {
        let v = with_host(|h| h.get_attr(&self.target, name)).map_err(rs_err)?;
        with_host(|h| value_to_py(h, py, &v))
            .map(|b| b.unbind())
            .map_err(rs_err)
    }
}

/// A pythonrs-side error string as the CPython exception a stdlib caller sees.
fn rs_err(e: String) -> pyo3::PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(e)
}

/// Like [`rs_err`], but preserves the exception CLASS carried in pythonrs's terse
/// `"Class: message"` error string by reconstructing the real builtin exception.
///
/// The class is what the stdlib branches on: `contextlib`'s `__exit__` reads a
/// `StopIteration` out of `gen.throw` as "the body's exception was suppressed"
/// and anything else as "re-raise", so collapsing every pythonrs error to
/// `RuntimeError` (what [`rs_err`] does) turns a handled `with` block into an
/// unhandled `RuntimeError`. Falls back to `RuntimeError` for a class CPython
/// has no builtin for — a user-defined exception has no CPython counterpart to
/// rebuild, and inventing one would let `except SomeUserError` match on the
/// CPython side by name alone.
fn rs_err_typed(e: String) -> pyo3::PyErr {
    let (class, msg) = match e.split_once(": ") {
        Some((c, m)) => (c, m),
        None => (e.as_str(), ""),
    };
    // A class name is a bare identifier; anything else is a plain message.
    if class.is_empty() || !class.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return rs_err(e);
    }
    Python::with_gil(|py| {
        let Ok(ty) = py.import("builtins").and_then(|m| m.getattr(class)) else {
            return rs_err(e.clone());
        };
        let is_exc = ty
            .downcast::<pyo3::types::PyType>()
            .ok()
            .map(|t| {
                t.is_subclass(&py.get_type::<pyo3::exceptions::PyBaseException>())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !is_exc {
            return rs_err(e.clone());
        }
        let built = if msg.is_empty() {
            ty.call0()
        } else {
            ty.call1((msg,))
        };
        match built {
            Ok(obj) => pyo3::PyErr::from_value(obj),
            Err(_) => rs_err(e.clone()),
        }
    })
}

/// `foreign[idx]`.
pub fn get_item(host: &mut PyHost, id: u32, idx: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let key = value_to_py(host, py, idx)?;
        let item = obj
            .get_item(key)
            .map_err(|e| pyerr_to_error_h(host, py, &e))?;
        reference_to_value(host, py, &item)
    })
}

/// [`get_item`] for the borrow-free path: the caller must NOT hold the host
/// borrow, so a `Foreign` object whose `__getitem__` is a pythonrs method can
/// re-enter. Key and result marshal under fresh short borrows.
pub fn get_item_cb(id: u32, idx: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let key = with_host(|h| value_to_py(h, py, idx))?;
        let obj = fetch(py, id)?;
        let item = obj.get_item(key).map_err(|e| pyerr_to_error(py, &e))?;
        with_host(|h| reference_to_value(h, py, &item))
    })
}

/// `foreign[idx] = value` — routed to CPython's own `__setitem__`, so a
/// mutable container held behind a handle (see [`get_attr`]) is assignable
/// through it and the mutation lands on the real object.
pub fn set_item(host: &mut PyHost, id: u32, idx: &Value, val: &Value) -> Result<(), String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let key = value_to_py(host, py, idx)?;
        let v = value_to_py(host, py, val)?;
        obj.set_item(key, v)
            .map_err(|e| pyerr_to_error_h(host, py, &e))
    })
}

/// `del foreign[idx]` — CPython's own `__delitem__`, the other half of
/// [`set_item`].
pub fn del_item(host: &mut PyHost, id: u32, idx: &Value) -> Result<(), String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let key = value_to_py(host, py, idx)?;
        obj.del_item(key)
            .map_err(|e| pyerr_to_error_h(host, py, &e))
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

/// [`make_iter`] for the borrow-free path: the caller must NOT hold the host
/// borrow, so a `Foreign` object whose `__iter__` is a pythonrs method can
/// re-enter. `try_iter` (which runs `__iter__`) is called with no borrow held;
/// only the resulting handle is allocated under a fresh short borrow.
pub fn make_iter_cb(id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let it = obj.try_iter().map_err(|e| e.to_string())?;
        let handle = it.into_any().unbind();
        Ok(with_host(|h| h.alloc(PyObj::Foreign(store(handle)))))
    })
}

/// True if the foreign object is an iterator (CPython's `PyIter_Check`, i.e.
/// `type(obj).__next__` exists) — not merely iterable.
pub fn is_iterator(id: u32) -> bool {
    Python::with_gil(|py| fetch(py, id).is_ok_and(|obj| obj.hasattr("__next__").unwrap_or(false)))
}

/// `next(foreign)` — `None` on `StopIteration`. Caller holds the host borrow, so
/// only safe for iterators that never re-enter pythonrs during `next()` (a plain
/// CPython container). Callback-driving iterators must use [`iter_next_cb`].
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

/// `next(foreign)` for the borrow-free iteration path (`host::iter_step` /
/// `host::iter_vec`). The caller must NOT hold the host borrow: advancing a lazy
/// CPython iterator (`itertools.starmap`/`dropwhile`/`takewhile`/`filterfalse`
/// over a pythonrs callable) runs that callable, which re-enters the host. The
/// advance therefore happens with no borrow held; the result is marshaled under
/// a fresh short borrow, exactly like `invoke_bound`.
pub fn iter_next_cb(id: u32) -> Result<Option<Value>, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let mut it = obj.try_iter().map_err(|e| e.to_string())?;
        match it.next() {
            None => Ok(None),
            Some(Ok(item)) => Ok(Some(with_host(|h| py_to_value(h, py, &item))?)),
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

/// [`contains`] for the borrow-free path: the caller must NOT hold the host
/// borrow, so a `Foreign` object whose `__contains__` is a pythonrs method can
/// re-enter. The needle marshals under a fresh short borrow.
pub fn contains_cb(id: u32, item: &Value) -> Result<bool, String> {
    Python::with_gil(|py| {
        let needle = with_host(|h| value_to_py(h, py, item))?;
        let obj = fetch(py, id)?;
        obj.contains(needle).map_err(|e| e.to_string())
    })
}

/// A binary/comparison operator (`+ - * / // % ** @ & | ^ << >>`,
/// `== != < <= > >=`) where at least one operand is a `Foreign` CPython object.
///
/// `func` is the corresponding `operator`-module attribute (`add`, `truediv`,
/// `mod`, `and_`, `lshift`, `lt`, `eq`, …). Both operands are marshaled to CPython
/// (a native operand crosses by value via the in-marshaler; a `Foreign` passes its
/// underlying object straight through), the real CPython operation runs, and the
/// result marshals back — by value when representable, else a fresh `Foreign`
/// (so `date + timedelta` → a CPython `date`, `Decimal + Decimal` → an exact
/// `Decimal`, `datetime < datetime` → a `bool`). A `TypeError`/`NotImplemented`
/// from CPython surfaces as a pythonrs error string, never a bridge panic.
pub fn binary_op(host: &mut PyHost, func: &str, a: &Value, b: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let pa = value_to_py(host, py, a)?;
        let pb = value_to_py(host, py, b)?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pa, pb)).map_err(|e| e.to_string())?;
        py_to_value(host, py, &res)
    })
}

/// `float(foreign)` — run CPython's own `float()` on the object so `__float__`
/// (`Fraction`, `Decimal`, `numpy` scalars, …) and `__index__` are honored. A
/// `TypeError` (no conversion) surfaces as a pythonrs error string.
pub fn to_float(id: u32) -> Result<f64, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let f = py
            .import("builtins")
            .and_then(|b| b.getattr("float"))
            .and_then(|f| f.call1((obj,)))
            .map_err(|e| e.to_string())?;
        f.extract::<f64>().map_err(|e| e.to_string())
    })
}

/// `isinstance(v, foreign_cls)` — the class is a CPython class/ABC
/// (`collections.abc.Sequence`, a `typing`/`enum` type). `v` is marshaled to its
/// CPython form (a native list crosses as a `list`, etc.) and CPython's
/// `isinstance` decides, so an ABC's structural `__instancecheck__` runs.
pub fn isinstance_foreign(host: &mut PyHost, v: &Value, cls_id: u32) -> Result<bool, String> {
    Python::with_gil(|py| {
        let obj = value_to_py(host, py, v)?;
        let cls = fetch(py, cls_id)?;
        obj.is_instance(&cls).map_err(|e| e.to_string())
    })
}

/// `issubclass(sub, cls)` where either side is a CPython object behind a handle
/// (a `collections.namedtuple` class, a `typing`/`abc` type): both cross the
/// bridge and CPython's own `issubclass` decides, its `TypeError`s included.
pub fn issubclass_values(host: &mut PyHost, sub: &Value, cls: &Value) -> Result<bool, String> {
    Python::with_gil(|py| {
        let a = value_to_py(host, py, sub)?;
        let b = value_to_py(host, py, cls)?;
        py.import("builtins")
            .and_then(|m| m.getattr("issubclass"))
            .and_then(|f| f.call1((a, b)))
            .and_then(|r| r.is_truthy())
            .map_err(|e| e.to_string())
    })
}

/// A CPython class's name as pythonrs's `type_name` spells a native type: the
/// bare `__name__` for a class in `builtins` (`types.FunctionType` is
/// `builtins.function`, `types.GeneratorType` is `builtins.generator`), else
/// `module.qualname` (`typing.TypeAliasType`, `re.Pattern`). Lets
/// `isinstance(native_fn, types.FunctionType)` compare against pythonrs's own
/// type of the same name: the native value crosses the bridge as a proxy,
/// which CPython's check would never accept.
pub fn foreign_builtin_type_name(cls_id: u32) -> Option<String> {
    Python::with_gil(|py| {
        let cls = fetch(py, cls_id).ok()?;
        let module: String = cls.getattr("__module__").ok()?.extract().ok()?;
        if module == "builtins" {
            return cls.getattr("__name__").ok()?.extract().ok();
        }
        let qualname: String = cls.getattr("__qualname__").ok()?.extract().ok()?;
        Some(format!("{module}.{qualname}"))
    })
}

/// `isinstance(foreign_v, <builtin type>)` — the mirror case: the VALUE is a
/// CPython object behind a handle and the class is one of pythonrs's own builtin
/// type objects, named by `type_name` (`tuple`, `dict`, `int`, …). The handle's
/// type name is the CPython class (`collections.namedtuple('P', …)` instances
/// report `P`), so the native structural check cannot see the base chain —
/// resolve the same name out of CPython's `builtins` and let CPython answer.
/// `false` when the name is not a builtin (a dotted native dispatch name, a user
/// class), which leaves the native check to decide.
pub fn foreign_isinstance_of_builtin(fid: u32, type_name: &str) -> bool {
    Python::with_gil(|py| {
        let (Ok(obj), Ok(builtins)) = (fetch(py, fid), py.import("builtins")) else {
            return false;
        };
        match builtins.getattr(type_name) {
            Ok(ty) => obj.is_instance(&ty).unwrap_or(false),
            Err(_) => false,
        }
    })
}

/// `int(foreign)` — run CPython's own `int()` on the object so `__int__` /
/// `__index__` and an `IntEnum` member (an `int` subclass) convert. The result
/// crosses back by value (bignum-safe via `py_to_value`).
pub fn to_int(host: &mut PyHost, id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let i = py
            .import("builtins")
            .and_then(|b| b.getattr("int"))
            .and_then(|f| f.call1((obj,)))
            .map_err(|e| e.to_string())?;
        py_to_value(host, py, &i)
    })
}

/// [`to_int`] for the borrow-free path: the caller must NOT hold the host borrow,
/// so a `Foreign` object whose `__int__`/`__index__` is a pythonrs method can
/// re-enter. Only the result marshals back, under a fresh short borrow.
pub fn to_int_cb(id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let i = py
            .import("builtins")
            .and_then(|b| b.getattr("int"))
            .and_then(|f| f.call1((obj,)))
            .map_err(|e| e.to_string())?;
        with_host(|h| py_to_value(h, py, &i))
    })
}

/// [`binary_op`] for the borrow-free path (`numeric_hook`): the caller must NOT
/// hold the host borrow. The operator runs in CPython with no borrow held, so an
/// operand whose comparison/arithmetic calls back into pythonrs (a
/// `functools.cmp_to_key` wrapper's `__lt__` invoking the user cmp function) can
/// re-enter the host. Args and result are marshaled under fresh short borrows.
pub fn binary_op_cb(func: &str, a: &Value, b: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let (pa, pb) = with_host(|h| -> Result<_, String> {
            Ok((value_to_py(h, py, a)?, value_to_py(h, py, b)?))
        })?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pa, pb)).map_err(|e| e.to_string())?;
        with_host(|h| py_to_value(h, py, &res))
    })
}

/// A unary operator on a `Foreign` CPython object: negation (`-x` → `neg`), unary
/// plus (`+x` → `pos`), bitwise invert (`~x` → `invert`), or `abs(x)` (`abs`).
/// `func` is the `operator`-module attribute; the CPython result marshals back the
/// same way as [`binary_op`].
pub fn unary_op(host: &mut PyHost, func: &str, v: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let pv = value_to_py(host, py, v)?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pv,)).map_err(|e| e.to_string())?;
        py_to_value(host, py, &res)
    })
}

/// [`unary_op`] for the borrow-free path: the caller must NOT hold the host
/// borrow. The CPython operator runs with no borrow held, so an operand whose
/// `__neg__`/`__abs__`/… is a pythonrs method (a `@dataclass` with user dunders)
/// can re-enter the host. Arg and result marshal under fresh short borrows.
pub fn unary_op_cb(func: &str, v: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let pv = with_host(|h| value_to_py(h, py, v))?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pv,)).map_err(|e| e.to_string())?;
        with_host(|h| py_to_value(h, py, &res))
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

/// `repr(module)` for a module whose `__spec__` is the CPython `ModuleSpec`
/// `spec`: `importlib._bootstrap._module_repr_from_spec`, which is what
/// `_module_repr` returns for any module carrying a spec
/// (`<module 'builtins' (built-in)>`). Reads only the bridge, never the host.
pub fn module_repr_from_spec(spec: u32) -> Option<String> {
    Python::with_gil(|py| {
        let spec = fetch(py, spec).ok()?;
        py.import("_frozen_importlib")
            .and_then(|m| m.getattr("_module_repr_from_spec"))
            .and_then(|f| f.call1((spec,)))
            .and_then(|r| r.extract::<String>())
            .ok()
    })
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
/// The `__name__` of a foreign *class* object (`except json.JSONDecodeError` →
/// `"JSONDecodeError"`). `None` if the handle isn't a class / has no `__name__`.
pub fn class_name(id: u32) -> Option<String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id).ok()?;
        obj.getattr("__name__")
            .ok()
            .and_then(|n| n.extract::<String>().ok())
    })
}

/// A handle to the CPython *type object* of foreign object `id` — what `type(x)`
/// must return. [`type_name`] only yields the unqualified name (`date`), which
/// cannot be rebuilt into the class: pythonrs has no `datetime.date`, so
/// `type()` used to answer with a `Builtin("date")` that printed as
/// `<built-in function date>`. Handing back CPython's own type object makes
/// `repr`, `dir`, attribute access, and `==` against the class all correct.
pub fn type_of(id: u32) -> Option<u32> {
    Python::with_gil(|py| {
        let ty = fetch(py, id).ok()?.get_type();
        // Memoize by the type object's address so `type(x)` in a loop does not
        // allocate a side-table slot per call. Reuse of a freed address cannot
        // alias a stale entry: the table holds a strong reference forever, so a
        // cached type object is never deallocated.
        let key = ty.as_ptr() as usize;
        let mut cache = type_handles().lock().expect("ffi type cache poisoned");
        if let Some(existing) = cache.get(&key) {
            return Some(*existing);
        }
        let fid = store(ty.into_any().unbind());
        cache.insert(key, fid);
        Some(fid)
    })
}

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

/// Whether `type(foreign)` defines `name` somewhere on its MRO — CPython's
/// `_PyType_Lookup`, the lookup `_PyObject_LookupSpecial` does for a special
/// method. The instance dict and the metatype are deliberately not consulted:
/// `with` checks the TYPE, so an `__exit__` stored on the instance does not
/// make it a context manager.
pub fn type_defines(id: u32, name: &str) -> bool {
    Python::with_gil(|py| {
        let Ok(obj) = fetch(py, id) else {
            return false;
        };
        let Ok(mro) = obj.get_type().getattr("__mro__") else {
            return false;
        };
        let Ok(classes) = mro.try_iter() else {
            return false;
        };
        classes.flatten().any(|cls| {
            cls.getattr("__dict__")
                .and_then(|d| d.contains(name))
                .unwrap_or(false)
        })
    })
}

/// `type(foreign)`'s fully qualified name — CPython's `%T` format,
/// `_PyType_GetFullyQualifiedName`: `module.qualname`, with the module left
/// off for `builtins` and `__main__`.
pub fn type_qualified_name(id: u32) -> String {
    Python::with_gil(|py| {
        let Ok(obj) = fetch(py, id) else {
            return "object".into();
        };
        let ty = obj.get_type();
        let qualname = ty
            .getattr("__qualname__")
            .and_then(|q| q.extract::<String>())
            .unwrap_or_else(|_| "object".into());
        match ty
            .getattr("__module__")
            .and_then(|m| m.extract::<String>())
        {
            Ok(m) if m != "builtins" && m != "__main__" => format!("{m}.{qualname}"),
            _ => qualname,
        }
    })
}

//! Python standard-library modules kept native for pythonrs.
//!
//! Each submodule exposes `entries(&mut PyHost) -> Vec<(String, Value)>` (the
//! `import <mod>` attribute namespace, functions as `PyObj::Builtin("<mod>.<fn>")`)
//! and a `call(...)` that `builtins::call_builtin_function` routes to when one of
//! those builtins is invoked. Both take `&mut PyHost` directly (no re-entrancy).
//!
//! What lives here is the C-accelerator floor and nothing else. Every hand-rolled
//! PURE-Python shadow has been deleted as the vendored `pylib/` original became
//! runnable — `json`/`os`/`random`/`string`/`itertools`/`functools` first, then
//! `textwrap` and `statistics` once `re` grew look-around and `decimal`/`fractions`
//! imported. A shadow is a second implementation to keep in parity forever; the
//! vendored file is CPython's own source and cannot drift from it.
pub mod binascii;
pub mod codecs;
pub mod pyast;
pub mod pycsv;
pub mod pyhash;
pub mod pyimp;
pub mod pyio;
pub mod pyopcode;
pub mod pysignal;
pub mod pystruct;
pub mod pythread;
pub mod pytokenize;

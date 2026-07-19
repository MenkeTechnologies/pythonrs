//! Python standard-library modules kept native for pythonrs.
//!
//! Each submodule exposes `entries(&mut PyHost) -> Vec<(String, Value)>` (the
//! `import <mod>` attribute namespace, functions as `PyObj::Builtin("<mod>.<fn>")`)
//! and a `call(...)` that `builtins::call_builtin_function` routes to when one of
//! those builtins is invoked. Both take `&mut PyHost` directly (no re-entrancy).
//!
//! The former `json`/`os`/`random`/`string`/`itertools`/`functools` shadows were
//! removed once the `stdlib-ffi` bridge (`crate::ffi`) began importing the real
//! CPython stdlib for those modules. Only modules with no C-accelerator parity
//! concern remain hand-rolled here.
pub mod statistics;
pub mod textwrap;

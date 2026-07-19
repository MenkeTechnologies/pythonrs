//! Python standard-library modules implemented natively for pythonrs.
//!
//! Each submodule exposes `entries(&mut PyHost) -> Vec<(String, Value)>` (the
//! `import <mod>` attribute namespace, functions as `PyObj::Builtin("<mod>.<fn>")`)
//! and a `call(...)` that `builtins::call_builtin_function` routes to when one of
//! those builtins is invoked. `itertools`/`functools` expose `call` as free
//! functions (they re-enter `with_host` for generator-safe iteration and callback
//! dispatch, so they must not hold a `&mut PyHost`); `json`/`os`/`random`/`string`
//! take `&mut PyHost` directly (no re-entrancy).
pub mod bisect;
pub mod datetime;
pub mod functools;
pub mod heapq;
pub mod itertools;
pub mod json;
pub mod os;
pub mod random;
pub mod re;
pub mod statistics;
pub mod string;
pub mod textwrap;

//! `_signal` — the POSIX signal surface `signal.py` wraps.
//!
//! `signal.py` opens with `from _signal import *` and re-exports the numbers as
//! `enum.IntEnum` members, so the module cannot load without this one —
//! `unittest` imports `signal` to install its Ctrl-C handler.
//!
//! The signal NUMBERS are real: they come from libc, so `signal.SIGTERM` is the
//! value this platform actually uses and `os.kill(pid, signal.SIGTERM)` sends the
//! right one. What is not real is delivery INTO the interpreter: pythonrs runs
//! user code on one thread with no async-signal check between bytecodes, so a
//! handler registered here is remembered and returned by `getsignal`, but never
//! invoked. `signal.signal(...)` therefore succeeds and composes — which is what
//! `unittest`'s installer needs — without claiming an interruption that will not
//! arrive.

use crate::host::{PyHost, PyObj};
use fusevm::Value;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Handlers registered through `signal.signal`, by signal number. Remembered
    /// so `getsignal` reports what was installed; never dispatched.
    static HANDLERS: RefCell<HashMap<i64, Value>> = RefCell::new(HashMap::new());
}

/// `SIG_DFL` / `SIG_IGN`, the two integer sentinels a handler slot can hold.
const SIG_DFL: i64 = 0;
const SIG_IGN: i64 = 1;

/// `_signal.<fn>(...)`.
pub fn call(h: &mut PyHost, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    Some(match name {
        // `signal(signalnum, handler)` — install and return the previous handler.
        "signal" => {
            let num = args.first().and_then(|v| h.as_int(v)).unwrap_or(0);
            let new = args.get(1).cloned().unwrap_or(Value::Int(SIG_DFL));
            let old = HANDLERS.with(|m| m.borrow_mut().insert(num, new));
            Ok(old.unwrap_or(Value::Int(SIG_DFL)))
        }
        "getsignal" => {
            let num = args.first().and_then(|v| h.as_int(v)).unwrap_or(0);
            if let Some(v) = HANDLERS.with(|m| m.borrow().get(&num).cloned()) {
                return Some(Ok(v));
            }
            // SIGINT starts out bound to `default_int_handler`, as CPython
            // installs at startup; everything else defaults to `SIG_DFL`.
            Ok(if num == libc::SIGINT as i64 {
                h.alloc(PyObj::Builtin("_signal.default_int_handler".into()))
            } else {
                Value::Int(SIG_DFL)
            })
        }
        // `default_int_handler(signum, frame)` raises KeyboardInterrupt, which is
        // exactly what the default SIGINT handler does.
        "default_int_handler" => Err("KeyboardInterrupt".to_string()),
        // Alarms and timers need delivery, which there is none of.
        "alarm" | "setitimer" | "getitimer" => Ok(Value::Int(0)),
        "pause" => Ok(Value::Undef),
        "raise_signal" => Ok(Value::Undef),
        "strsignal" => {
            let num = args.first().and_then(|v| h.as_int(v)).unwrap_or(0);
            Ok(h.new_str(format!("Signal {num}")))
        }
        "valid_signals" => {
            let mut items: indexmap::IndexMap<crate::host::PKey, Value> = indexmap::IndexMap::new();
            for (_, n) in SIGNALS {
                items.insert(crate::host::PKey::Int(*n as i64), Value::Int(*n as i64));
            }
            Ok(h.new_set(items))
        }
        // Nothing is masked, and there is no wakeup fd to write to.
        "pthread_sigmask" => Ok(h.new_set(indexmap::IndexMap::new())),
        "set_wakeup_fd" => Ok(Value::Int(-1)),
        "siginterrupt" => Ok(Value::Undef),
        _ => return None,
    })
}

/// The signal numbers this platform uses, straight from libc.
const SIGNALS: &[(&str, i32)] = &[
    ("SIGHUP", libc::SIGHUP),
    ("SIGINT", libc::SIGINT),
    ("SIGQUIT", libc::SIGQUIT),
    ("SIGILL", libc::SIGILL),
    ("SIGTRAP", libc::SIGTRAP),
    ("SIGABRT", libc::SIGABRT),
    ("SIGFPE", libc::SIGFPE),
    ("SIGKILL", libc::SIGKILL),
    ("SIGBUS", libc::SIGBUS),
    ("SIGSEGV", libc::SIGSEGV),
    ("SIGSYS", libc::SIGSYS),
    ("SIGPIPE", libc::SIGPIPE),
    ("SIGALRM", libc::SIGALRM),
    ("SIGTERM", libc::SIGTERM),
    ("SIGURG", libc::SIGURG),
    ("SIGSTOP", libc::SIGSTOP),
    ("SIGTSTP", libc::SIGTSTP),
    ("SIGCONT", libc::SIGCONT),
    ("SIGCHLD", libc::SIGCHLD),
    ("SIGTTIN", libc::SIGTTIN),
    ("SIGTTOU", libc::SIGTTOU),
    ("SIGIO", libc::SIGIO),
    ("SIGXCPU", libc::SIGXCPU),
    ("SIGXFSZ", libc::SIGXFSZ),
    ("SIGVTALRM", libc::SIGVTALRM),
    ("SIGPROF", libc::SIGPROF),
    ("SIGWINCH", libc::SIGWINCH),
    ("SIGUSR1", libc::SIGUSR1),
    ("SIGUSR2", libc::SIGUSR2),
];

/// The `_signal` namespace.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const FNS: &[&str] = &[
        "signal",
        "getsignal",
        "default_int_handler",
        "alarm",
        "pause",
        "raise_signal",
        "strsignal",
        "valid_signals",
        "setitimer",
        "getitimer",
        "pthread_sigmask",
        "set_wakeup_fd",
        "siginterrupt",
    ];
    let mut out: Vec<(String, Value)> = FNS
        .iter()
        .map(|f| {
            (
                (*f).to_string(),
                h.alloc(PyObj::Builtin(format!("_signal.{f}"))),
            )
        })
        .collect();
    for (name, num) in SIGNALS {
        out.push(((*name).to_string(), Value::Int(*num as i64)));
    }
    for (name, v) in [
        ("SIG_DFL", SIG_DFL),
        ("SIG_IGN", SIG_IGN),
        ("SIG_BLOCK", libc::SIG_BLOCK as i64),
        ("SIG_UNBLOCK", libc::SIG_UNBLOCK as i64),
        ("SIG_SETMASK", libc::SIG_SETMASK as i64),
        ("ITIMER_REAL", 0),
        ("ITIMER_VIRTUAL", 1),
        ("ITIMER_PROF", 2),
        ("NSIG", 32),
    ] {
        out.push((name.to_string(), Value::Int(v)));
    }
    let err = h.alloc(PyObj::Builtin("OSError".into()));
    out.push(("ItimerError".to_string(), err));
    out
}

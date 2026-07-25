//! `_thread` — the threading primitives `threading.py` is built on.
//!
//! **pythonrs runs user code on a single thread.** That is a property of the
//! runtime, not an omission here: the object heap is a `thread_local`, and
//! generators suspend by switching stacks on the same OS thread. So a thread
//! started through this module runs its target IMMEDIATELY, to completion, and
//! is finished by the time `start()` returns.
//!
//! That is a real semantic difference from CPython and is not papered over.
//! `Thread.start()` / `join()` / `is_alive()` all behave consistently under it —
//! a joined thread has run, an un-joined one has too — but two threads never
//! overlap, so code that depends on interleaving (a producer/consumer handoff
//! through an unbounded queue, a spin on a flag another thread sets) will not
//! make progress. Code that uses threads to ORGANIZE work rather than to overlap
//! it — which is what `logging` and `hashlib` do, guarding shared state with
//! locks they then never contend — works exactly as written.
//!
//! The primitives live in the native `_thread_core` arm; the Python-shaped parts
//! (the handle object, the shutdown bookkeeping) are written in Python below,
//! because that is what they are.

/// The Python source of the `_thread` module. `_thread_core` supplies the pieces
/// that need the host (locks, the current thread id); everything else is plain
/// Python over those.
pub fn module_source() -> &'static str {
    r#""""Low-level threading primitives.

pythonrs executes user code on ONE thread: a thread started here runs its target
immediately and has finished by the time `start_joinable_thread` returns.
"""

from _thread_core import (
    allocate_lock,
    error,
    get_ident as _host_ident,
    get_native_id as _host_native_id,
    RLock,
    start_new_thread,
    TIMEOUT_MAX,
)

# Threads run one at a time, but each still needs its OWN identity while it runs:
# `threading` keys its registry of live threads by ident, so reusing one id would
# make a finished thread's `_delete()` evict the main thread's entry and leave
# `current_thread()` answering with a dummy.
_MAIN_IDENT = _host_ident()
_next_ident = _MAIN_IDENT
_running_ident = _MAIN_IDENT


def get_ident():
    return _running_ident


def get_native_id():
    return _running_ident

# `_thread.LockType` is the lock TYPE and is directly instantiable — `threading`
# binds `Lock = _LockType` and calls it. `allocate_lock` is the same constructor
# under its older name.
LockType = allocate_lock


class _ThreadHandle:
    """A handle on a started thread.

    Since the target has already run by the time the handle is observed, a handle
    is created done unless it is an unstarted placeholder.
    """

    __slots__ = ('_ident', '_done')

    def __init__(self, ident=None):
        self._ident = ident
        self._done = ident is not None

    @property
    def ident(self):
        return self._ident

    def is_done(self):
        return self._done

    def _set_done(self):
        self._done = True

    def join(self, timeout=None):
        # The target ran to completion during `start_joinable_thread`, so there
        # is nothing to wait for.
        return None


def _make_thread_handle(ident):
    return _ThreadHandle(ident)


def start_joinable_thread(function, handle=None, daemon=True):
    """Run `function` to completion and return a handle on the finished thread."""
    global _next_ident, _running_ident
    _next_ident += 1
    ident = _next_ident
    if handle is None:
        handle = _ThreadHandle(ident)
    else:
        handle._ident = ident
    previous = _running_ident
    _running_ident = ident
    try:
        function()
    finally:
        _running_ident = previous
        handle._set_done()
    return handle


def daemon_threads_allowed():
    return True


def _get_main_thread_ident():
    return _MAIN_IDENT


def _is_main_interpreter():
    return True


def _shutdown():
    """Wait for non-daemon threads to finish. Nothing is ever outstanding."""
    return None


def stack_size(size=None):
    """The thread stack size. Threads here run on the interpreter's own stack."""
    return 0


def interrupt_main(signum=2):
    raise KeyboardInterrupt


class _local:
    """Thread-local storage.

    With one thread there is exactly one set of values, so this is an ordinary
    attribute bag. `threading` prefers this over the pure-Python
    `_threading_local`, whose fallback imports `current_thread` back out of a
    half-built `threading` — a cycle CPython avoids the same way.
    """

    def __init__(self, *args, **kwargs):
        if (args or kwargs) and type(self).__init__ is _local.__init__:
            raise TypeError("Initialization arguments are not supported")
"#
}

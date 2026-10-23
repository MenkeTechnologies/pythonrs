//! The parts of `asyncio` that CPython writes in Python over its event loop,
//! written in Python here over the native loop (`crate::async_rt`): `sleep`
//! (`asyncio/tasks.py`), `timeout`/`Timeout` (`asyncio/timeouts.py`),
//! `TaskGroup` (`asyncio/taskgroups.py`), `to_thread` (`asyncio/threads.py`)
//! and `iscoroutinefunction` (`asyncio/coroutines.py`).
//!
//! They are compiled once per host into a namespace whose free names are the
//! native primitives below; `asyncio.<name>` resolves to them on first use.

/// The names the source below defines, which `asyncio.<name>` hands over to.
pub const NAMES: &[&str] = &[
    "sleep",
    "timeout",
    "Timeout",
    "TaskGroup",
    "to_thread",
    "iscoroutinefunction",
];

/// The source. Its globals provide `_sleep_future`, `_current_task`,
/// `_create_task`, `_wait`, `_get_running_loop`, `CancelledError`.
pub fn module_source() -> &'static str {
    r#"
async def sleep(delay, result=None):
    """Coroutine that completes after a given time (in seconds)."""
    return await _sleep_future(delay, result)


class Timeout:
    """Asynchronous context manager for cancelling overdue coroutines."""

    def __init__(self, when):
        self._when = when
        self._state = "created"
        self._task = None
        self._expired = False

    def when(self):
        return self._when

    def expired(self):
        return self._expired

    def reschedule(self, when):
        self._when = when
        if self._state == "entered" and when is not None:
            self._arm()

    def _arm(self):
        loop = _get_running_loop()
        token = object()
        self._token = token
        delay = max(0.0, self._when - loop.time())

        def on_timeout():
            if self._token is token and self._state == "entered":
                self._expired = True
                self._state = "expiring"
                self._task.cancel()

        loop.call_later(delay, on_timeout)

    def __repr__(self):
        return f"<Timeout [{self._state}] when={self._when}>"

    async def __aenter__(self):
        if self._state != "created":
            raise RuntimeError("Timeout has already been entered")
        task = _current_task()
        if task is None:
            raise RuntimeError("Timeout should be used inside a task")
        self._task = task
        self._state = "entered"
        if self._when is not None:
            self._arm()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        self._token = None
        if self._state == "expiring":
            self._state = "expired"
            if exc_type is not None and issubclass(exc_type, CancelledError):
                raise TimeoutError from exc_val
        elif self._state == "entered":
            self._state = "exited"
        return None


def timeout(delay):
    """Timeout async context manager: cancel the block after `delay` seconds."""
    loop = _get_running_loop()
    return Timeout(loop.time() + delay if delay is not None else None)


class TaskGroup:
    """Asynchronous context manager for managing a group of tasks."""

    def __init__(self):
        self._entered = False
        self._exiting = False
        self._aborting = False
        self._parent_task = None
        self._tasks = set()
        self._errors = []

    def __repr__(self):
        info = ['']
        if self._tasks:
            info.append(f'tasks={len(self._tasks)}')
        if self._errors:
            info.append(f'errors={len(self._errors)}')
        if self._aborting:
            info.append('cancelling')
        elif self._entered:
            info.append('entered')
        return f'<TaskGroup{" ".join(info)}>'

    async def __aenter__(self):
        if self._entered:
            raise RuntimeError(f"TaskGroup {self!r} has already been entered")
        self._parent_task = _current_task()
        if self._parent_task is None:
            raise RuntimeError(f'TaskGroup {self!r} cannot determine the parent task')
        self._entered = True
        return self

    async def __aexit__(self, et, exc, tb):
        self._exiting = True
        if exc is not None and not isinstance(exc, CancelledError):
            self._errors.append(exc)
            self._abort()
        while self._tasks:
            await _wait(list(self._tasks))
        if self._errors:
            errors = self._errors
            self._errors = None
            if all(isinstance(e, Exception) for e in errors):
                raise ExceptionGroup('unhandled errors in a TaskGroup', errors)
            raise BaseExceptionGroup('unhandled errors in a TaskGroup', errors)
        return None

    def create_task(self, coro, *, name=None, context=None):
        if not self._entered:
            coro.close()
            raise RuntimeError(f"TaskGroup {self!r} has not been entered")
        if self._exiting and not self._tasks:
            coro.close()
            raise RuntimeError(f"TaskGroup {self!r} is finished")
        if self._aborting:
            coro.close()
            raise RuntimeError(f"TaskGroup {self!r} is shutting down")
        task = _create_task(coro, name=name)
        self._tasks.add(task)
        task.add_done_callback(self._on_task_done)
        return task

    def _abort(self):
        self._aborting = True
        for t in self._tasks:
            if not t.done():
                t.cancel()

    def _on_task_done(self, task):
        self._tasks.discard(task)
        if task.cancelled():
            return
        exc = task.exception()
        if exc is None:
            return
        self._errors.append(exc)
        if not self._aborting:
            self._abort()
            if not self._exiting:
                self._parent_task.cancel()


async def to_thread(func, /, *args, **kwargs):
    """Run `func` and return its result (pythonrs runs one thread: in place)."""
    return func(*args, **kwargs)


def iscoroutinefunction(func):
    """Return True if func is a decorated coroutine function."""
    import warnings
    # `warnings._deprecated("asyncio.iscoroutinefunction", ..., remove=(3, 16))`,
    # issued from here so the warning names the caller's line.
    warnings.warn("'asyncio.iscoroutinefunction' is deprecated and slated for "
                  "removal in Python 3.16; use inspect.iscoroutinefunction() instead",
                  DeprecationWarning, stacklevel=2)
    import inspect
    return inspect.iscoroutinefunction(func)
"#
}

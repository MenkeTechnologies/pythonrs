//! Native `asyncio` runtime for pythonrs: a single-thread, cooperative event
//! loop (ready-queue + timer-heap) that drives `async def` coroutines, plus the
//! Future/Task machinery and the `asyncio.*` module surface.
//!
//! Design (faithful to CPython's asyncio, adapted to the fusevm generator infra):
//!   - An `async def` call returns a *coroutine object* — a stackful `corosensei`
//!     coroutine that suspends at each `await` (see `host::make_coroutine`). The
//!     body does NOT run until the loop drives it.
//!   - `await x` (see [`await_value`]) drives `x`: an `asyncio.Future`/`Task`
//!     suspends the running coroutine (yielding the future up to its Task) until
//!     the future settles; a coroutine or a `__await__` iterator is delegated
//!     into (yield-from), forwarding whatever *it* yields (ultimately a Future)
//!     up to the loop. Futures are the only leaves that reach the loop.
//!   - A `Task` wraps a coroutine and steps it: `gen_resume` runs the body to the
//!     next awaited Future; the Task registers a done-callback that re-steps it
//!     when the Future settles. When the body returns/raises, the Task's own
//!     Future is fulfilled/failed.
//!   - The loop's virtual clock jumps to the earliest pending timer when the
//!     ready-queue empties, so `asyncio.sleep` ordering matches CPython without
//!     real waiting.
//!
//! Everything is thread-local (matching the thread-local `PyHost`); no locks, no
//! real threads — `asyncio` semantics are single-threaded by construction.

use crate::host::{self, with_host, PyObj};
use fusevm::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

// ── future / task cells ──────────────────────────────────────────────────────

/// One `asyncio.Future` (or `Task`, which is a Future that also drives a
/// coroutine). Indexed by `PyObj::Future.id`.
struct FutureCell {
    done: bool,
    cancelled: bool,
    /// The fulfilled result (`set_result`), `Undef` until done.
    result: Value,
    /// The failing exception object (`set_exception` / a coroutine raised).
    exc: Option<Value>,
    /// User `add_done_callback` callables (invoked with the future).
    py_callbacks: Vec<Value>,
    /// Native continuations (Task wakeups, gather joins) run on settlement.
    native_callbacks: Vec<Box<dyn FnOnce()>>,
    /// A Task's driven coroutine object (`None` for a plain Future).
    coro: Option<Value>,
    is_task: bool,
    /// Whether a Task's next step has been scheduled (guards double-step).
    step_scheduled: bool,
    name: String,
}

// ── event loop ───────────────────────────────────────────────────────────────

/// A queued unit of work: a native continuation or a scheduled Python callback.
enum Callback {
    Native(Box<dyn FnOnce()>),
    Py { func: Value, args: Vec<Value> },
}

/// A `call_later`/`call_at` timer. Fires when the virtual clock reaches `when`;
/// ties broken by insertion `seq` (FIFO), matching CPython's timer heap.
struct Timer {
    when: f64,
    seq: u64,
    cancelled: bool,
    cb: Callback,
}

#[derive(Default)]
struct EventLoop {
    futures: Vec<FutureCell>,
    ready: VecDeque<Callback>,
    timers: Vec<Timer>,
    time: f64,
    seq: u64,
    running: bool,
}

thread_local! {
    static LOOP: RefCell<EventLoop> = RefCell::new(EventLoop::default());
}

fn with_loop<R>(f: impl FnOnce(&mut EventLoop) -> R) -> R {
    LOOP.with(|l| f(&mut l.borrow_mut()))
}

/// Reset the async runtime (called from `host::reset_host`).
pub fn reset() {
    with_loop(|l| *l = EventLoop::default());
}

// ── future primitives ────────────────────────────────────────────────────────

fn new_cell(is_task: bool, coro: Option<Value>, name: String) -> Value {
    let id = with_loop(|l| {
        let id = l.futures.len() as u32;
        l.futures.push(FutureCell {
            done: false,
            cancelled: false,
            result: Value::Undef,
            exc: None,
            py_callbacks: Vec::new(),
            native_callbacks: Vec::new(),
            coro,
            is_task,
            step_scheduled: false,
            name,
        });
        id
    });
    with_host(|h| h.alloc(PyObj::Future { id }))
}

/// A fresh pending `asyncio.Future`.
pub fn new_future() -> Value {
    new_cell(false, None, "Future".to_string())
}

fn future_id(v: &Value) -> Option<u32> {
    match with_host(|h| h.get(v).cloned()) {
        Some(PyObj::Future { id }) => Some(id),
        _ => None,
    }
}

/// Whether `v` is an `asyncio` Future or Task.
pub fn is_future(v: &Value) -> bool {
    future_id(v).is_some()
}

fn future_done(id: u32) -> bool {
    with_loop(|l| l.futures[id as usize].done)
}

fn future_coro(id: u32) -> Option<Value> {
    with_loop(|l| l.futures[id as usize].coro.clone())
}

/// The exception a settled future failed with (`None` if it fulfilled cleanly).
fn future_exc(id: u32) -> Option<Value> {
    with_loop(|l| l.futures[id as usize].exc.clone())
}

fn future_result(id: u32) -> Value {
    with_loop(|l| l.futures[id as usize].result.clone())
}

/// Settle a future (fulfill or fail) and schedule its done-callbacks. A second
/// settlement is ignored (a Task whose coroutine ends after cancellation, etc.).
fn settle(id: u32, result: Value, exc: Option<Value>, cancelled: bool) -> bool {
    let already = with_loop(|l| l.futures[id as usize].done);
    if already {
        return false;
    }
    let (pys, natives) = with_loop(|l| {
        let f = &mut l.futures[id as usize];
        f.done = true;
        f.result = result;
        f.exc = exc;
        f.cancelled = cancelled;
        (
            std::mem::take(&mut f.py_callbacks),
            std::mem::take(&mut f.native_callbacks),
        )
    });
    // Native continuations first (Task wakeups) then user callbacks, each via
    // call_soon so they run on a later loop turn (CPython schedules them all).
    for n in natives {
        call_soon_native(n);
    }
    let fut = with_host(|h| h.alloc(PyObj::Future { id }));
    for cb in pys {
        call_soon_py(cb, vec![fut.clone()]);
    }
    true
}

/// `future.set_result(v)`.
pub fn set_result(fut: &Value, v: Value) -> Result<Value, String> {
    let id = future_id(fut).ok_or_else(|| host::type_error("set_result on non-future"))?;
    if future_done(id) {
        return Err("InvalidStateError: invalid state".to_string());
    }
    settle(id, v, None, false);
    Ok(Value::Undef)
}

/// `future.set_exception(exc)`.
pub fn set_exception(fut: &Value, exc: Value) -> Result<Value, String> {
    let id = future_id(fut).ok_or_else(|| host::type_error("set_exception on non-future"))?;
    if future_done(id) {
        return Err("InvalidStateError: invalid state".to_string());
    }
    settle(id, Value::Undef, Some(exc), false);
    Ok(Value::Undef)
}

/// Fail a future without raising if it is already settled (used by gather/join).
fn fail_quietly(id: u32, exc: Value) {
    settle(id, Value::Undef, Some(exc), false);
}

/// Register a native continuation, firing immediately (via call_soon) if the
/// future is already settled.
fn add_done_native(id: u32, f: Box<dyn FnOnce()>) {
    if future_done(id) {
        call_soon_native(f);
    } else {
        with_loop(|l| l.futures[id as usize].native_callbacks.push(f));
    }
}

// ── event-loop scheduling ────────────────────────────────────────────────────

fn call_soon_native(f: Box<dyn FnOnce()>) {
    with_loop(|l| l.ready.push_back(Callback::Native(f)));
}

fn call_soon_py(func: Value, args: Vec<Value>) {
    with_loop(|l| l.ready.push_back(Callback::Py { func, args }));
}

fn call_later(delay: f64, cb: Callback) {
    with_loop(|l| {
        let when = l.time + delay.max(0.0);
        let seq = l.seq;
        l.seq += 1;
        l.timers.push(Timer {
            when,
            seq,
            cancelled: false,
            cb,
        });
    });
}

fn run_callback(cb: Callback) -> Result<(), String> {
    match cb {
        Callback::Native(f) => {
            f();
            Ok(())
        }
        Callback::Py { func, args } => host::invoke(&func, args, vec![]).map(|_| ()),
    }
}

/// One turn of the loop: advance the virtual clock to the earliest due timer if
/// nothing is immediately ready, move due timers into the ready-queue in
/// `(when, seq)` order, then run a snapshot of the ready callbacks.
fn run_once() -> Result<(), String> {
    // If nothing is ready, jump the clock to the next timer deadline.
    let jump = with_loop(|l| {
        if !l.ready.is_empty() {
            return None;
        }
        l.timers
            .iter()
            .filter(|t| !t.cancelled)
            .map(|t| t.when)
            .fold(None, |acc: Option<f64>, w| {
                Some(acc.map_or(w, |a| a.min(w)))
            })
    });
    if let Some(w) = jump {
        with_loop(|l| l.time = l.time.max(w));
    }
    // Move all due timers into ready (ordered by when, then seq).
    let due: Vec<Callback> = with_loop(|l| {
        let now = l.time;
        // Partition: keep future/cancelled timers, collect due ones.
        let mut due: Vec<Timer> = Vec::new();
        let mut kept: Vec<Timer> = Vec::new();
        for t in l.timers.drain(..) {
            if !t.cancelled && t.when <= now {
                due.push(t);
            } else {
                kept.push(t);
            }
        }
        l.timers = kept;
        due.sort_by(|a, b| {
            (a.when, a.seq)
                .partial_cmp(&(b.when, b.seq))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        due.into_iter().map(|t| t.cb).collect()
    });
    for cb in due {
        with_loop(|l| l.ready.push_back(cb));
    }
    // Run a snapshot of ready callbacks (new call_soons go to the next turn).
    let ntodo = with_loop(|l| l.ready.len());
    for _ in 0..ntodo {
        let cb = with_loop(|l| l.ready.pop_front());
        if let Some(cb) = cb {
            run_callback(cb)?;
        }
    }
    Ok(())
}

/// Whether the loop still has work (ready callbacks or live timers).
fn has_work() -> bool {
    with_loop(|l| !l.ready.is_empty() || l.timers.iter().any(|t| !t.cancelled))
}

/// Drive the loop until `fut` completes, then return its result (or raise its
/// exception). This is `loop.run_until_complete`.
pub fn run_until_complete(aw: Value) -> Result<Value, String> {
    if with_loop(|l| l.running) {
        return Err("RuntimeError: This event loop is already running".to_string());
    }
    let fut = ensure_future(aw)?;
    let id = future_id(&fut).ok_or_else(|| host::type_error("run_until_complete: not a future"))?;
    with_loop(|l| l.running = true);
    let result = loop {
        if future_done(id) {
            break Ok(());
        }
        if !has_work() {
            break Err(
                "RuntimeError: Event loop stopped before Future completed.".to_string(),
            );
        }
        if let Err(e) = run_once() {
            with_loop(|l| l.running = false);
            return Err(e);
        }
    };
    with_loop(|l| l.running = false);
    result?;
    // Fulfilled → return result; failed → re-raise the exception at the caller.
    if let Some(exc) = future_exc(id) {
        return Err(host::raise_value(&exc).unwrap_or_else(|e| e));
    }
    Ok(future_result(id))
}

// ── task creation / stepping ─────────────────────────────────────────────────

/// Wrap a coroutine in a Task and schedule its first step; return the Task.
pub fn create_task(coro: Value, name: Option<String>) -> Result<Value, String> {
    if !host::is_coroutine(&coro) {
        return Err(host::type_error("a coroutine was expected"));
    }
    let name = name.unwrap_or_else(|| "Task".to_string());
    let task = new_cell(true, Some(coro), name);
    schedule_step(task.clone());
    Ok(task)
}

/// Coerce an awaitable to a Future: a coroutine becomes a Task; a Future passes
/// through; anything else is an error.
pub fn ensure_future(aw: Value) -> Result<Value, String> {
    if is_future(&aw) {
        Ok(aw)
    } else if host::is_coroutine(&aw) {
        create_task(aw, None)
    } else {
        Err(host::type_error(&format!(
            "An asyncio.Future, a coroutine or an awaitable is required (got {})",
            with_host(|h| h.type_name(&aw))
        )))
    }
}

fn schedule_step(task: Value) {
    let id = match future_id(&task) {
        Some(id) => id,
        None => return,
    };
    let already = with_loop(|l| l.futures[id as usize].step_scheduled);
    if already {
        return;
    }
    with_loop(|l| l.futures[id as usize].step_scheduled = true);
    call_soon_native(Box::new(move || task_step(task)));
}

/// Resume a Task's coroutine to its next awaited Future (or to completion).
fn task_step(task: Value) {
    let id = match future_id(&task) {
        Some(id) => id,
        None => return,
    };
    with_loop(|l| l.futures[id as usize].step_scheduled = false);
    if future_done(id) {
        return;
    }
    let coro = match future_coro(id) {
        Some(c) => c,
        None => return,
    };
    match host::gen_resume(&coro, Value::Undef) {
        Ok(Some(awaited)) => {
            // The coroutine yielded a Future it is waiting on. Re-step this Task
            // when that Future settles.
            match future_id(&awaited) {
                Some(aid) => {
                    let task2 = task.clone();
                    add_done_native(
                        aid,
                        Box::new(move || schedule_step(task2)),
                    );
                }
                None => {
                    // A coroutine yielded a non-Future to the loop (e.g. a bare
                    // `yield` in an async context) — asyncio rejects this.
                    let e = with_host(|h| {
                        let msg = h.new_str("Task got bad yield: awaited a non-future".to_string());
                        h.alloc(PyObj::Exception {
                            class: "RuntimeError".into(),
                            args: vec![msg],
                        })
                    });
                    fail_quietly(id, e);
                }
            }
        }
        Ok(None) => {
            let rv = host::coro_return_value(&coro);
            settle(id, rv, None, false);
        }
        Err(e) => {
            let exc = take_pending_exc(&e);
            fail_quietly(id, exc);
        }
    }
}

/// Pull the in-flight exception object off the host (set by `raise`), or
/// synthesize one from the abort string if none is live.
fn take_pending_exc(err: &str) -> Value {
    with_host(|h| {
        if let Some(e) = h.exc.take() {
            e
        } else {
            // Split "Class: message" into an exception object.
            let (class, msg) = match err.split_once(": ") {
                Some((c, m)) => (c.to_string(), m.to_string()),
                None => (err.to_string(), String::new()),
            };
            let args = if msg.is_empty() {
                vec![]
            } else {
                vec![h.new_str(msg)]
            };
            let class = if crate::builtins::is_exception_class(&class) {
                class
            } else {
                "Exception".to_string()
            };
            h.alloc(PyObj::Exception { class, args })
        }
    })
}

// ── await ────────────────────────────────────────────────────────────────────

/// The `AWAIT` op body (runs inside an async coroutine). Drive `x` to its
/// result, suspending the coroutine (up to the driving Task) as needed.
pub fn await_value(x: Value) -> Result<Value, String> {
    // An asyncio Future/Task: suspend until it settles, then return / raise.
    if let Some(id) = future_id(&x) {
        loop {
            if future_done(id) {
                if let Some(exc) = future_exc(id) {
                    return Err(host::raise_value(&exc).unwrap_or_else(|e| e));
                }
                return Ok(future_result(id));
            }
            // Yield the future up to the Task driving this coroutine; the Task
            // re-steps us (send value ignored) once the future is settled.
            host::gen_yield(x.clone())?;
        }
    }
    // A coroutine: delegate into it (yield-from), forwarding its yields up.
    if host::is_coroutine(&x) {
        return drive_delegate(&x);
    }
    // A custom awaitable: `__await__()` returns an iterator/generator to drive.
    let has_await = with_host(|h| match h.get(&x) {
        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__await__").is_some(),
        _ => false,
    });
    if has_await {
        let it = host::call_method(&x, "__await__", vec![], vec![])?;
        return drive_delegate(&it);
    }
    Err(host::type_error(&format!(
        "object {} can't be used in 'await' expression",
        with_host(|h| h.type_name(&x))
    )))
}

/// Delegate into a sub-coroutine / `__await__` iterator: pump it, forwarding
/// each yielded value (ultimately a Future) up to our own resumer.
fn drive_delegate(sub: &Value) -> Result<Value, String> {
    let mut send = Value::Undef;
    loop {
        match host::gen_resume(sub, send) {
            Ok(Some(y)) => {
                // Forward the yield up to the loop; the sent value is ignored
                // (Futures publish their result on the object, not via send).
                send = host::gen_yield(y)?;
            }
            Ok(None) => return Ok(host::coro_return_value(sub)),
            Err(e) => return Err(e),
        }
    }
}

// ── asyncio module surface ───────────────────────────────────────────────────

/// `asyncio.run(coro)` — run `coro` on a fresh loop turn to completion.
pub fn run(coro: Value) -> Result<Value, String> {
    if !host::is_coroutine(&coro) {
        return Err(host::type_error(&format!(
            "asyncio.run() requires a coroutine, got {}",
            with_host(|h| h.type_name(&coro))
        )));
    }
    run_until_complete(coro)
}

/// `asyncio.sleep(delay[, result])` — an awaitable that completes with `result`
/// after `delay` seconds of virtual time (a single loop turn for `delay <= 0`).
pub fn sleep(delay: f64, result: Value) -> Value {
    let fut = new_future();
    let id = future_id(&fut).unwrap();
    if delay <= 0.0 {
        call_soon_native(Box::new(move || {
            settle(id, result, None, false);
        }));
    } else {
        call_later(
            delay,
            Callback::Native(Box::new(move || {
                settle(id, result, None, false);
            })),
        );
    }
    fut
}

/// `asyncio.gather(*aws)` — run all awaitables concurrently; the returned Future
/// completes with the list of results in argument order, or fails on the first
/// exception (default `return_exceptions=False`).
pub fn gather(aws: Vec<Value>, return_exceptions: bool) -> Result<Value, String> {
    let outer = new_future();
    let oid = future_id(&outer).unwrap();
    let n = aws.len();
    if n == 0 {
        let empty = with_host(|h| h.new_list(vec![]));
        settle(oid, empty, None, false);
        return Ok(outer);
    }
    let results: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(vec![Value::Undef; n]));
    let remaining = Rc::new(std::cell::Cell::new(n));
    for (i, aw) in aws.into_iter().enumerate() {
        let child = ensure_future(aw)?;
        let cid = future_id(&child).unwrap();
        let results_c = results.clone();
        let remaining_c = remaining.clone();
        add_done_native(
            cid,
            Box::new(move || {
                let exc = future_exc(cid);
                let value = match exc {
                    Some(e) if !return_exceptions => {
                        fail_quietly(oid, e);
                        return;
                    }
                    Some(e) => e, // return_exceptions: store the exception itself
                    None => future_result(cid),
                };
                results_c.borrow_mut()[i] = value;
                let rem = remaining_c.get() - 1;
                remaining_c.set(rem);
                if rem == 0 {
                    let list = with_host(|h| h.new_list(results_c.borrow().clone()));
                    settle(oid, list, None, false);
                }
            }),
        );
    }
    Ok(outer)
}

/// `asyncio.wait_for(aw, timeout)` — await `aw`, raising `TimeoutError` if it is
/// not done within `timeout` seconds. `timeout=None` just awaits.
pub fn wait_for(aw: Value, timeout: Option<f64>) -> Result<Value, String> {
    let inner = ensure_future(aw)?;
    let iid = future_id(&inner).unwrap();
    let outer = new_future();
    let oid = future_id(&outer).unwrap();
    // Propagate the inner result/exception to the outer future.
    add_done_native(
        iid,
        Box::new(move || {
            if let Some(e) = future_exc(iid) {
                fail_quietly(oid, e);
            } else {
                settle(oid, future_result(iid), None, false);
            }
        }),
    );
    if let Some(t) = timeout {
        call_later(
            t,
            Callback::Native(Box::new(move || {
                if !future_done(oid) {
                    let e = with_host(|h| {
                        h.alloc(PyObj::Exception {
                            class: "TimeoutError".into(),
                            args: vec![],
                        })
                    });
                    fail_quietly(oid, e);
                }
            })),
        );
    }
    Ok(outer)
}

// ── event-loop object ────────────────────────────────────────────────────────

/// The singleton `asyncio` event loop object (`get_event_loop`/`get_running_loop`).
pub fn event_loop() -> Value {
    with_host(|h| h.alloc(PyObj::EventLoop))
}

/// Dispatch a method call on the event-loop object.
pub fn loop_method(name: &str, args: Vec<Value>) -> Result<Value, String> {
    match name {
        "run_until_complete" => {
            let aw = args.into_iter().next().unwrap_or(Value::Undef);
            run_until_complete(aw)
        }
        "create_task" => {
            let coro = args.into_iter().next().unwrap_or(Value::Undef);
            create_task(coro, None)
        }
        "create_future" => Ok(new_future()),
        "call_soon" => {
            let mut it = args.into_iter();
            let func = it.next().unwrap_or(Value::Undef);
            call_soon_py(func, it.collect());
            Ok(handle_obj())
        }
        "call_later" => {
            let mut it = args.into_iter();
            let delay = it.next().and_then(as_f).unwrap_or(0.0);
            let func = it.next().unwrap_or(Value::Undef);
            let rest: Vec<Value> = it.collect();
            call_later(delay, Callback::Py { func, args: rest });
            Ok(handle_obj())
        }
        "time" => Ok(Value::Float(with_loop(|l| l.time))),
        "is_running" => Ok(Value::Bool(with_loop(|l| l.running))),
        "is_closed" => Ok(Value::Bool(false)),
        "stop" | "close" | "run_forever" => Ok(Value::Undef),
        "get_debug" => Ok(Value::Bool(false)),
        "set_debug" => Ok(Value::Undef),
        _ => Err(format!(
            "AttributeError: '_UnixSelectorEventLoop' object has no attribute '{name}'"
        )),
    }
}

/// A minimal `TimerHandle`/`Handle` stand-in (`call_soon`/`call_later` return
/// something with a `.cancel()`); we return a fresh Future used only as an
/// opaque handle whose `cancel()` is a harmless no-op.
fn handle_obj() -> Value {
    new_future()
}

// ── future object surface ────────────────────────────────────────────────────

/// Dispatch a method call on a Future/Task object.
pub fn future_method(recv: &Value, name: &str, args: Vec<Value>) -> Result<Value, String> {
    let id = future_id(recv).ok_or_else(|| host::type_error("not a future"))?;
    match name {
        "set_result" => set_result(recv, args.into_iter().next().unwrap_or(Value::Undef)),
        "set_exception" => {
            set_exception(recv, args.into_iter().next().unwrap_or(Value::Undef))
        }
        "result" => {
            if !future_done(id) {
                return Err("InvalidStateError: Result is not set.".to_string());
            }
            if let Some(exc) = future_exc(id) {
                return Err(host::raise_value(&exc).unwrap_or_else(|e| e));
            }
            Ok(future_result(id))
        }
        "exception" => {
            if !future_done(id) {
                return Err("InvalidStateError: Exception is not set.".to_string());
            }
            Ok(future_exc(id).unwrap_or(Value::Undef))
        }
        "done" => Ok(Value::Bool(future_done(id))),
        "cancelled" => Ok(Value::Bool(with_loop(|l| l.futures[id as usize].cancelled))),
        "add_done_callback" => {
            let cb = args.into_iter().next().unwrap_or(Value::Undef);
            if future_done(id) {
                let fut = recv.clone();
                call_soon_py(cb, vec![fut]);
            } else {
                with_loop(|l| l.futures[id as usize].py_callbacks.push(cb));
            }
            Ok(Value::Undef)
        }
        "cancel" => {
            if future_done(id) {
                Ok(Value::Bool(false))
            } else {
                let e = with_host(|h| {
                    h.alloc(PyObj::Exception {
                        class: "CancelledError".into(),
                        args: vec![],
                    })
                });
                settle(id, Value::Undef, Some(e), true);
                Ok(Value::Bool(true))
            }
        }
        "get_name" => Ok(with_host(|h| {
            let nm = with_loop(|l| l.futures[id as usize].name.clone());
            h.new_str(nm)
        })),
        "__await__" | "__iter__" => Ok(recv.clone()),
        _ => Err(format!(
            "AttributeError: '{}' object has no attribute '{name}'",
            future_type_name(id)
        )),
    }
}

/// The type name for a Future cell (`Task` vs `Future`).
pub fn future_type_name(id: u32) -> &'static str {
    if with_loop(|l| l.futures[id as usize].is_task) {
        "Task"
    } else {
        "Future"
    }
}

/// `repr` of a Future/Task: `<Task pending name='…'>` / `<Future finished result=…>`.
pub fn future_repr(id: u32) -> String {
    let (is_task, done, cancelled, name) = with_loop(|l| {
        let f = &l.futures[id as usize];
        (f.is_task, f.done, f.cancelled, f.name.clone())
    });
    let kind = if is_task { "Task" } else { "Future" };
    let state = if cancelled {
        "cancelled".to_string()
    } else if done {
        if let Some(exc) = future_exc(id) {
            format!("finished exception={}", with_host(|h| h.repr_of(&exc)))
        } else {
            format!("finished result={}", with_host(|h| h.repr_of(&future_result(id))))
        }
    } else {
        "pending".to_string()
    };
    if is_task {
        format!("<{kind} {state} name='{name}'>")
    } else {
        format!("<{kind} {state}>")
    }
}

fn as_f(v: Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(n as f64),
        Value::Float(f) => Some(f),
        Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

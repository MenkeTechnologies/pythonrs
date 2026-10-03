//! How much native stack the running code has left, measured the way CPython
//! 3.14 measures it.
//!
//! CPython bounds recursion in its parser and compiler by the machine stack
//! itself rather than by a counter. `hardware_stack_limits` (Python/ceval.c)
//! reads the running thread's real bounds — `pthread_get_stackaddr_np` /
//! `pthread_get_stacksize_np` on macOS, `pthread_getattr_np` elsewhere — and
//! `tstate_set_stack` puts a hard limit one margin above the bottom of the stack
//! and a soft limit one margin above that. Two consumers read them:
//!
//! * the PEG parser, beside its own level counter, asks
//!   `_Py_ReachedRecursionLimitWithMargin(tstate, 1)` on entry to every rule and
//!   raises `MemoryError: Parser stack overflowed - Python source too complex to
//!   parse` when it answers true ([`parser_exhausted`]);
//! * every recursive compiler walk calls `_Py_EnterRecursiveCall(" during
//!   compilation")`, which raises `RecursionError: Stack overflow (used N kB)
//!   during compilation` once the stack pointer is below the soft limit, `N`
//!   being the distance from the top of the stack ([`enter_compile`]).
//!
//! Both limits therefore follow the stack the code is actually running on: the
//! 512 MB interpreter thread `src/main.rs` spawns, an embedder's 2 MB worker, or
//! a generator's own coroutine stack (registered with [`with_bounds`]), and a
//! source too deep for the stack in hand is a catchable exception on all of
//! them rather than a `SIGABRT`.
//!
//! The margin is pythonrs's own. CPython's (`_PyOS_STACK_MARGIN_BYTES`, 16 KB on
//! a 64-bit release build) is sized for C frames; a single unoptimised pythonrs
//! frame can be as large, so the margin here is the deepest run of frames a walk
//! makes between two checks, with room for the error to be built and returned.

use std::cell::{Cell, RefCell};

/// One margin. The hard limit is one margin above the bottom of the stack and
/// the soft limit two, as in `tstate_set_stack`.
const MARGIN: usize = 128 * 1024;

/// The stack the current code runs on, `[base, top)` (stacks grow down on every
/// target pythonrs builds for), and its soft limit.
#[derive(Clone, Copy)]
struct Bounds {
    base: usize,
    top: usize,
    soft: usize,
}

impl Bounds {
    /// `tstate_set_stack` for a stack spanning `[base, top)`.
    fn new(base: usize, top: usize) -> Bounds {
        Bounds {
            base,
            top,
            soft: base + 2 * MARGIN,
        }
    }

    /// Whether `sp` lies on this stack at all. A stack pointer elsewhere means
    /// the code is running on a stack nobody registered, whose room is unknown.
    fn holds(&self, sp: usize) -> bool {
        (self.base..self.top).contains(&sp)
    }
}

thread_local! {
    /// The bounds of the stack in use: the thread's own, read once, or a
    /// coroutine's while [`with_bounds`] runs one.
    static BOUNDS: Cell<Option<Bounds>> = const { Cell::new(None) };
    /// The highest address at which any check here can fire — the parser's
    /// threshold, one margin above the soft limit — so that every check made
    /// with room to spare is one comparison. `usize::MAX` until the bounds are
    /// known.
    static WATERMARK: Cell<usize> = const { Cell::new(usize::MAX) };
    /// A `RecursionError` raised by a walk that cannot return one. See
    /// [`compile_overflowed`].
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The current stack pointer, as `_Py_get_machine_stack_pointer` reads it: the
/// address of a local is within a frame of it.
#[inline(always)]
fn stack_pointer() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// `hardware_stack_limits`: the running thread's stack as `(base, top)`.
#[cfg(target_os = "macos")]
fn thread_stack() -> Option<(usize, usize)> {
    // SAFETY: both calls only read the calling thread's own attributes.
    unsafe {
        let this = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(this) as usize;
        let size = libc::pthread_get_stacksize_np(this);
        Some((top - size, top))
    }
}

/// `hardware_stack_limits`: the running thread's stack as `(base, top)`, the
/// base raised past the guard pages.
#[cfg(all(unix, not(target_os = "macos")))]
fn thread_stack() -> Option<(usize, usize)> {
    // SAFETY: `attr` is initialised by `pthread_getattr_np` before it is read
    // and destroyed exactly once.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let mut guard: libc::size_t = 0;
        let err = libc::pthread_attr_getstack(&attr, &mut addr, &mut size)
            | libc::pthread_attr_getguardsize(&attr, &mut guard);
        libc::pthread_attr_destroy(&mut attr);
        (err == 0).then(|| (addr as usize + guard, addr as usize + size))
    }
}

#[cfg(not(unix))]
fn thread_stack() -> Option<(usize, usize)> {
    None
}

/// Make `b` the stack in use.
fn set_bounds(b: Option<Bounds>) {
    BOUNDS.set(b);
    WATERMARK.set(b.map_or(usize::MAX, |b| b.soft + MARGIN));
}

/// Whether `sp` is far enough from the bottom of the stack that no check can
/// fire: the fast path every check takes first.
fn roomy(sp: usize) -> bool {
    sp > WATERMARK.get()
}

/// The bounds of the stack `sp` is on, reading the thread's on first use.
fn bounds(sp: usize) -> Option<Bounds> {
    let b = match BOUNDS.get() {
        Some(b) => b,
        None => {
            let (base, top) = thread_stack()?;
            let b = Bounds::new(base, top);
            set_bounds(Some(b));
            b
        }
    };
    b.holds(sp).then_some(b)
}

/// Run `f` on a coroutine stack spanning `[base, top)`, so the checks made
/// inside it measure that stack rather than the thread's. A generator body runs
/// on a stack of its own (see `host::gen_resume`).
pub fn with_bounds<R>(base: usize, top: usize, f: impl FnOnce() -> R) -> R {
    let outer = BOUNDS.get();
    set_bounds(Some(Bounds::new(base, top)));
    let r = f();
    set_bounds(outer);
    r
}

/// The stack the parser and the compiler are given when the caller's has less
/// than [`FRONTEND_ROOM`] left: the size of the interpreter thread `src/main.rs`
/// spawns, so a compile reaches the same depth wherever it runs. Reserved, not
/// committed: pages are touched only as deep as a parse actually goes.
const FRONTEND_STACK: usize = 512 * 1024 * 1024;

/// How much room the parser and the compiler need before they run on the
/// caller's stack as they find it.
///
/// Their frames are an interpreter's, not a C compiler's: an unoptimised
/// pythonrs build spends some 25 KB of stack per bracket of nesting where
/// CPython spends under 1 KB, so the 200 brackets CPython's tokenizer allows
/// need more than an ordinary 2 MB thread holds, and an embedder calling
/// [`crate::eval_str`] from one would be refused source CPython accepts. The
/// limits CPython states — pegen's `MAXSTACK` levels, the tokenizer's 200
/// brackets — are reproduced by count (see `parser::MAXSTACK`); only the
/// stack that hosts the count is pythonrs's own. The 512 MB interpreter thread
/// has the room and runs them in place.
const FRONTEND_ROOM: usize = 64 * 1024 * 1024;

/// Run the parser or the compiler: in place when the current stack has
/// `FRONTEND_ROOM` to spare, otherwise on a fresh `FRONTEND_STACK` of its
/// own, on the same thread (the object heap is thread-local). A panic in `f`
/// propagates to the caller either way.
pub fn with_frontend_stack<R>(f: impl FnOnce() -> R) -> R {
    use corosensei::stack::{DefaultStack, Stack};
    let sp = stack_pointer();
    if bounds(sp).is_some_and(|b| sp - b.base >= FRONTEND_ROOM) {
        return f();
    }
    let Ok(mut stack) = DefaultStack::new(FRONTEND_STACK) else {
        return f();
    };
    let (base, top) = (stack.limit().get(), stack.base().get());
    corosensei::on_stack(&mut stack, || with_bounds(base, top, f))
}

/// `_Py_ReachedRecursionLimitWithMargin(tstate, 1)`: whether the parser has
/// come within one margin of the soft limit. The parser stops a margin before
/// the compiler does, so a source the parser just accepted leaves its walks
/// room to report their own limit.
pub fn parser_exhausted() -> bool {
    let sp = stack_pointer();
    !roomy(sp) && bounds(sp).is_some_and(|b| sp <= b.soft + MARGIN)
}

/// The `RecursionError` a compiler walk raises below the soft limit:
/// `_Py_CheckRecursiveCall(tstate, " during compilation")`.
fn compile_error(b: Bounds, sp: usize) -> String {
    format!(
        "RecursionError: Stack overflow (used {} kB) during compilation",
        (b.top - sp) / 1024
    )
}

/// `_Py_EnterRecursiveCall(" during compilation")`, for a walk that can return
/// an error: called on entry to each recursive step.
pub fn enter_compile() -> Result<(), String> {
    let sp = stack_pointer();
    if roomy(sp) {
        return Ok(());
    }
    match bounds(sp) {
        Some(b) if sp < b.soft => Err(compile_error(b, sp)),
        _ => Ok(()),
    }
}

/// [`enter_compile`] for a walk that cannot return an error — an analysis that
/// answers a `bool` or fills a set. Past the soft limit it records the error
/// (the first one recorded wins, as the first raised does in CPython) and
/// answers `true`; the walk then stops descending and returns whatever it has,
/// and the compile reports the recorded error in place of its result
/// ([`take_pending`]), so the partial answer is never used.
pub fn compile_overflowed() -> bool {
    let sp = stack_pointer();
    if roomy(sp) {
        return false;
    }
    match bounds(sp) {
        Some(b) if sp < b.soft => {
            PENDING.with(|p| {
                p.borrow_mut().get_or_insert_with(|| compile_error(b, sp));
            });
            true
        }
        _ => false,
    }
}

/// Clear any error a previous compile left behind.
pub fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().take());
}

/// The error [`compile_overflowed`] recorded, if any.
pub fn take_pending() -> Option<String> {
    PENDING.with(|p| p.borrow_mut().take())
}

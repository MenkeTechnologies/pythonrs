//! Mark-and-sweep collection of the object heap.
//!
//! An object is `Value::Obj(id)`, an index into `PyHost::heap`, so the
//! collector never moves one: it marks everything reachable from the roots,
//! turns every other slot into `PyObj::Freed`, and `alloc` reuses those slots.
//! An object's id — so `is`, `id()` and a dict key — is stable for its life.
//!
//! Rust code holds `Value`s the collector cannot see: a builtin's popped
//! operands, the arguments of a call back into Python, the frames on a
//! suspended generator's coroutine stack. So a collection runs only where
//! none can exist — at a safepoint (the entry of a call or loop builtin, before
//! it pops anything) on the program's top-level VM with no other VM run on the
//! Rust stack, no generator suspended mid-body, and nothing in the asyncio
//! loop.
//! Everything else that holds a `Value` is a root (`trace_roots`). The
//! `stdlib-ffi` build never collects: CPython objects hold pythonrs ids where
//! nothing here can read them.

use super::*;
use std::cell::Cell;

/// Allocations before the first collection. After one, the next waits for as
/// many allocations as objects survived, so collecting costs time in
/// proportion to allocating.
const MIN_THRESHOLD: usize = 100_000;

thread_local! {
    /// Set once enough has been allocated since the last collection; the
    /// safepoint's whole cost while it is clear.
    static DUE: Cell<bool> = const { Cell::new(false) };
    /// VM runs active on this thread's Rust stack (`finish_run`).
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// The depth of the program's top-level run (`run_main`); 0 when none is
    /// running. Only code on that VM, with nothing nested in it, can collect.
    static ROOT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// `PYTHONRS_GC_STRESS=1`: collect at every safepoint that allows it, so a
/// root the collector misses shows up as a freed-object panic on the next
/// read rather than one program in a thousand reading a recycled object.
fn stress() -> bool {
    static STRESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRESS.get_or_init(|| std::env::var_os("PYTHONRS_GC_STRESS").is_some_and(|v| v == "1"))
}

/// The collector's bookkeeping on the host.
#[derive(Default)]
pub(super) struct GcState {
    /// Freed slots, for `alloc` to reuse.
    pub(super) free: Vec<u32>,
    /// Allocations since the last collection (or refused attempt).
    since: usize,
    /// How many allocations make a collection due; 0 until the first one.
    threshold: usize,
    /// The previous collection's mark bits and work stack, kept so a
    /// collection allocates nothing once the heap stops growing.
    marks: Marks,
    pending: Vec<u32>,
}

/// One mark bit per heap slot.
#[derive(Default)]
struct Marks(Vec<u64>);

impl Marks {
    /// Clear, sized for `n` slots.
    fn reset(&mut self, n: usize) {
        self.0.clear();
        self.0.resize(n.div_ceil(64), 0);
    }

    /// Mark `id`; whether it was unmarked before.
    fn set(&mut self, id: u32) -> bool {
        let (word, bit) = (id as usize / 64, 1u64 << (id % 64));
        match self.0.get_mut(word) {
            Some(w) if *w & bit == 0 => {
                *w |= bit;
                true
            }
            _ => false,
        }
    }

    fn get(&self, id: u32) -> bool {
        self.0[id as usize / 64] & (1u64 << (id % 64)) != 0
    }
}

impl GcState {
    /// Count one allocation, flagging a collection due at the threshold.
    #[inline]
    pub(super) fn allocated(&mut self) {
        self.since += 1;
        if self.since >= self.threshold.max(MIN_THRESHOLD) || stress() {
            DUE.with(|d| d.set(true));
        }
    }
}

/// Counts a VM run for as long as it lives (`finish_run`).
pub(super) struct RunGuard;

impl RunGuard {
    pub(super) fn enter() -> RunGuard {
        DEPTH.with(|d| d.set(d.get() + 1));
        RunGuard
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Run `f`, whose VM run is the program's top level (`run_main`): the one run
/// a collection may happen inside.
pub(super) fn as_root_run<R>(f: impl FnOnce() -> R) -> R {
    let saved = ROOT_DEPTH.with(|r| r.replace(DEPTH.with(Cell::get) + 1));
    let out = f();
    ROOT_DEPTH.with(|r| r.set(saved));
    out
}

/// Collect if a collection is due and every live `Value` is a root here.
///
/// Called on entry to the call and loop builtins, before they pop anything,
/// so their operands are still on `vm`'s stack.
#[inline]
pub(crate) fn safepoint(vm: &VM) {
    if DUE.with(Cell::get) {
        collect_if_safe(vm);
    }
}

#[cold]
fn collect_if_safe(vm: &VM) {
    DUE.with(|d| d.set(false));
    let root = ROOT_DEPTH.with(Cell::get);
    let top_level = root != 0 && DEPTH.with(Cell::get) == root;
    let safe = cfg!(not(feature = "stdlib-ffi"))
        && top_level
        && !DEBUG_MODE.with(Cell::get)
        && async_rt::loop_is_idle();
    with_host(|h| {
        // A generator suspended mid-body holds `Value`s on its coroutine stack; one
        // not yet started holds only its body chunk, and its frame is a root.
        if safe && !h.generators.iter().any(|g| g.started && !g.done) {
            h.collect(vm);
        } else {
            // Not here: wait for another threshold's worth before asking again.
            h.gc.since = 0;
        }
    });
}

/// The reachable set, built as the roots and the objects they reach are traced.
struct Marker {
    marks: Marks,
    /// Marked objects whose own references are still to trace.
    pending: Vec<u32>,
    /// Environments already traced; closures share their parents' chains.
    envs: HashSet<*const RefCell<EnvData>>,
}

impl Marker {
    fn object(&mut self, id: u32) {
        if self.marks.set(id) {
            self.pending.push(id);
        }
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Obj(id) => self.object(*id),
            Value::Array(items) => items.iter().for_each(|x| self.value(x)),
            Value::Hash(m) => m.values().for_each(|x| self.value(x)),
            Value::Ref(x) => self.value(x),
            Value::Undef
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Str(_)
            | Value::Status(_)
            | Value::NativeFn(_) => {}
        }
    }

    fn values<'a>(&mut self, vs: impl IntoIterator<Item = &'a Value>) {
        vs.into_iter().for_each(|v| self.value(v));
    }

    /// A dict or set key: an instance or foreign key keeps the object it is
    /// keyed by alive.
    fn key(&mut self, k: &PKey) {
        match k {
            PKey::Instance { id, .. } | PKey::Foreign { id, .. } => self.object(*id),
            PKey::Tuple(ks) | PKey::Frozenset(ks) | PKey::Slice(ks) | PKey::Range(ks) => {
                ks.iter().for_each(|k| self.key(k))
            }
            PKey::Method { recv, func } => {
                self.key(recv);
                self.key(func);
            }
            PKey::None
            | PKey::Int(_)
            | PKey::Big(_)
            | PKey::FloatBits(_)
            | PKey::Complex(..)
            | PKey::Str(_)
            | PKey::Bytes(_)
            | PKey::Class(_)
            | PKey::Singleton(_) => {}
        }
    }

    fn env(&mut self, env: &Env) {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if !self.envs.insert(Rc::as_ptr(&e)) {
                return;
            }
            let data = e.borrow();
            match &data.vars {
                EnvVars::Small(vars) => vars.iter().for_each(|(_, v)| self.value(v)),
                EnvVars::Spilled(vars) => self.values(vars.values()),
            }
            cur = data.parent.clone();
        }
    }

    fn frame(&mut self, f: &Frame) {
        self.env(&f.env);
        self.values(&f.self_obj);
        if let Some(id) = f.init_of {
            self.object(id);
        }
    }

    fn func(&mut self, fv: &FuncVal) {
        if let Some(env) = &fv.env {
            self.env(env);
        }
        self.values(&fv.defaults);
        self.values(&fv.kwonly_defaults);
        self.values(&fv.bound);
        self.value(&fv.annotations);
        self.value(&fv.annotate);
    }

    fn signal(&mut self, s: &Option<Signal>) {
        if let Some(Signal::Return(v)) = s {
            self.value(v);
        }
    }
}

impl PyHost {
    /// Mark from the roots — `vm`, the program's top-level VM at a safepoint,
    /// and everything `trace_roots` names — and free what was not reached.
    fn collect(&mut self, vm: &VM) {
        let mut marks = std::mem::take(&mut self.gc.marks);
        marks.reset(self.heap.len());
        let mut m = Marker {
            marks,
            pending: std::mem::take(&mut self.gc.pending),
            envs: HashSet::new(),
        };
        m.values(&vm.stack);
        for f in &vm.frames {
            m.values(&f.slots);
        }
        m.values(&vm.globals);
        self.trace_roots(&mut m);
        while let Some(id) = m.pending.pop() {
            self.trace_object(id, &mut m);
        }
        self.sweep(&m.marks);
        self.gc.marks = m.marks;
        self.gc.pending = m.pending;
        self.gc.threshold = self.heap.len() - self.gc.free.len();
        self.gc.since = 0;
    }

    /// Everything outside the heap that holds a `Value` at a safepoint.
    fn trace_roots(&self, m: &mut Marker) {
        for cd in self.classes.values() {
            m.values(cd.ns.values());
        }
        m.values(self.foreign_class_cells.values());
        for globals in &self.module_globals {
            m.values(globals.values());
        }
        m.values(self.module_dicts.values());
        for f in &self.frames {
            m.frame(f);
        }
        m.values(&self.exc);
        m.signal(&self.signal);
        for g in &self.generators {
            for f in &g.ctx.frames {
                m.frame(f);
            }
            m.values(&g.ctx.exc);
            m.signal(&g.ctx.signal);
            m.values(&g.pending_throw);
            m.value(&g.ret_value);
            if let Some(AGenOp::Send(v) | AGenOp::Throw(v)) = &g.agen_op {
                m.value(v);
            }
        }
        for lru in &self.lru_caches {
            for (k, v) in &lru.map {
                m.key(k);
                m.value(v);
            }
            lru.order.iter().for_each(|k| m.key(k));
        }
        m.values(self.builtin_objects.values());
        m.values(self.slot_members.values());
        m.values(&self.codec_search);
        m.values(self.codec_cache.values());
        m.values(self.codec_errors.values());
        match &self.suggest {
            Some(SuggestCtx::Name { env, self_obj, .. }) => {
                m.env(env);
                m.values(self_obj);
            }
            Some(SuggestCtx::Attr { recv, self_obj, .. }) => {
                m.value(recv);
                m.values(self_obj);
            }
            None => {}
        }
        for env in &self.spare_envs {
            m.env(env);
        }
        m.values(&self.stdout_target);
        m.values(&self.stderr_target);
        m.values(&self.sys_flags);
        m.values(self.modules.values());
        m.values(&self.sys_modules);
        for (func, args, kwargs) in &self.atexit_callbacks {
            m.value(func);
            m.values(args);
            kwargs.iter().for_each(|(_, v)| m.value(v));
        }
        m.values(self.re_cache.values());
        crate::stdlib::pysignal::for_each_handler(|v| m.value(v));
    }

    /// Mark what object `id` refers to: its fields, and its entries in the
    /// side tables keyed by heap id.
    fn trace_object(&self, id: u32, m: &mut Marker) {
        match &self.heap[id as usize] {
            PyObj::List(items)
            | PyObj::Tuple(items)
            | PyObj::Union { args: items }
            | PyObj::StructTime { fields: items }
            | PyObj::CsvReader { rows: items, .. }
            | PyObj::Exception { args: items, .. }
            | PyObj::Zip { sources: items, .. }
            | PyObj::Template {
                interpolations: items,
                ..
            } => m.values(items),
            PyObj::Dict(d) => {
                for (k, (kv, v)) in d {
                    m.key(k);
                    m.value(kv);
                    m.value(v);
                }
            }
            PyObj::Set(s) | PyObj::Frozenset(s) => {
                for (k, v) in s {
                    m.key(k);
                    m.value(v);
                }
            }
            PyObj::DictView { dict: v, .. }
            | PyObj::MappingProxy { dict: v }
            | PyObj::Cell { value: v }
            | PyObj::CsvWriter { stream: v, .. }
            | PyObj::EnumerateObj { source: v, .. }
            | PyObj::SeqIter { obj: v, .. }
            | PyObj::Interpolation { value: v, .. }
            | PyObj::Memoryview { obj: v, .. }
            | PyObj::LruCache { func: v, .. }
            | PyObj::Super { instance: v, .. }
            | PyObj::StaticMethod(v)
            | PyObj::ClassMethod(v)
            | PyObj::CachedProperty { func: v, .. }
            | PyObj::TypeVarLike { attrs: v, .. } => m.value(v),
            PyObj::Slice { lo, hi, step } => m.values([lo, hi, step]),
            PyObj::Func(fv) => m.func(fv),
            PyObj::GenericAlias { origin, args } => {
                m.value(origin);
                m.values(args);
            }
            PyObj::Match { re, string, .. } => m.values([re, string]),
            PyObj::ReScanner {
                pattern, string, ..
            } => m.values([pattern, string]),
            PyObj::TypeAlias {
                module,
                params,
                evaluate,
                value,
                ..
            } => m.values([module, params, evaluate, value]),
            PyObj::Namespace { attrs } => m.values(attrs.values()),
            PyObj::Descriptor { recv, .. } => m.values(recv),
            PyObj::ContextVar { default, value, .. } => {
                m.values(default.as_deref());
                m.values(value.as_deref());
            }
            PyObj::ContextToken { var, old } => {
                m.value(var);
                m.values(old.as_deref());
            }
            PyObj::ItertoolsIter {
                sources, func, buf, ..
            } => {
                m.values(sources);
                m.value(func);
                m.values(buf);
            }
            PyObj::Instance(inst) => m.values([&inst.dict, &inst.payload]),
            PyObj::BoundMethod { recv, func } => m.values([recv, func]),
            PyObj::Iter(IterState::Seq { items, .. }) => m.values(items),
            PyObj::Iter(IterState::DictKeys { keys, .. }) => m.values(keys),
            PyObj::Iter(IterState::RangeIter { .. } | IterState::BigRangeIter { .. }) => {}
            PyObj::MapObj { func, sources, .. } => {
                m.value(func);
                m.values(sources);
            }
            PyObj::FilterObj { func, source, .. } => m.values([func, source]),
            PyObj::CallIter { func, sentinel, .. } => m.values([func, sentinel]),
            PyObj::Deque { items, .. } => m.values(items),
            PyObj::Partial { func, args, kwargs } => {
                m.value(func);
                m.values(args);
                kwargs.iter().for_each(|(_, v)| m.value(v));
            }
            PyObj::Property { fget, fset, fdel, .. } => m.values([fget, fset, fdel]),
            PyObj::Redirect { target, saved, .. } => {
                m.value(target);
                m.values(saved);
            }
            // No `Value` inside. A `Module`/`ModuleDict` reaches its globals,
            // a `Generator`/`LruCache`/`Pattern`/`File` its side table, and a
            // `Class` its namespace, all of which are roots.
            PyObj::CompiledSource { .. }
            | PyObj::Str(_)
            | PyObj::Bytes(_)
            | PyObj::Range { .. }
            | PyObj::BigRange { .. }
            | PyObj::Code { .. }
            | PyObj::Pattern { .. }
            | PyObj::Traceback { .. }
            | PyObj::PyFrame { .. }
            | PyObj::FrameCode { .. }
            | PyObj::Lock { .. }
            | PyObj::StructFmt(_)
            | PyObj::Hasher { .. }
            | PyObj::CsvDialect(_)
            | PyObj::Unbound
            | PyObj::ContextObj
            | PyObj::Builtin(_)
            | PyObj::Class(_)
            | PyObj::Module { .. }
            | PyObj::BytesIO { .. }
            | PyObj::StringIO { .. }
            | PyObj::ModuleDict { .. }
            | PyObj::BigInt(_)
            | PyObj::Complex(..)
            | PyObj::Generator { .. }
            | PyObj::Future { .. }
            | PyObj::EventLoop
            | PyObj::AsyncObj { .. }
            | PyObj::Bytearray(_)
            | PyObj::File { .. }
            | PyObj::NamedTupleType { .. }
            | PyObj::NotImplemented
            | PyObj::Ellipsis
            | PyObj::Freed => {}
            #[cfg(feature = "stdlib-ffi")]
            PyObj::Foreign(_) => {}
        }
        if let Some(DictMeta {
            factory: Some(f), ..
        }) = self.dict_meta.get(&id)
        {
            m.value(f);
        }
        if let Some(nt) = self.nt_meta.get(&id) {
            nt.extra.iter().for_each(|(_, v)| m.value(v));
        }
        if let Some((cause, context)) = self.exc_links.get(&id) {
            m.values([cause, context]);
        }
        if let Some(root) = self.eg_split_root.get(&id) {
            m.object(*root);
        }
        if let Some(attrs) = self.func_attrs.get(&id) {
            m.values(attrs.values());
        }
        if let Some(slots) = self.slot_values.get(&id) {
            m.values(slots.values());
        }
    }

    /// Free every slot `marks` leaves unmarked, and forget the per-object
    /// metadata of each, which an object reusing its id must not inherit.
    fn sweep(&mut self, marks: &Marks) {
        for id in 0..self.heap.len() as u32 {
            if !marks.get(id) && !matches!(self.heap[id as usize], PyObj::Freed) {
                self.heap[id as usize] = PyObj::Freed;
                self.gc.free.push(id);
            }
        }
        let live = |id: &u32| marks.get(*id);
        self.dict_meta.retain(|id, _| live(id));
        self.nt_meta.retain(|id, _| live(id));
        self.set_start.retain(|id, _| live(id));
        self.exc_links.retain(|id, _| live(id));
        self.exc_tb.retain(|id, _| live(id));
        self.eg_split_root.retain(|id, _| live(id));
        self.tb_starts_empty.retain(live);
        self.func_attrs.retain(|id, _| live(id));
        self.slot_values.retain(|id, _| live(id));
        self.suppress_context.retain(live);
        self.user_str_lines.retain(|id, _| live(id));
        self.mt_states.retain(|id, _| live(id));
        let mut str_index = self.str_index.borrow_mut();
        if str_index.as_ref().is_some_and(|s| !live(&s.id)) {
            *str_index = None;
        }
    }
}

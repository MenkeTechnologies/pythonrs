//! Lower the Python AST to `fusevm::Chunk`.
//!
//! Native fusevm ops carry arithmetic (`+ - *`), comparisons and boolean
//! short-circuit so the JIT can trace them; the strict numeric hook (host)
//! supplies Python semantics for non-numeric operands (str/list concat, bignum
//! promotion). Everything Python-specific — name access, attribute/item access,
//! calls, object construction, iteration — lowers to a `CallBuiltin` that lands
//! in `builtins.rs`.
//!
//! Conditions are normalized through the `TRUTHY` builtin before a native
//! `JumpIfFalse`, because Python truthiness (empty containers, `None`, `0`, `""`
//! are falsy) differs from fusevm's default numeric truthiness. Compiler-internal
//! name strings (variable/attribute/method names) travel as native `Value::Str`
//! constants; Python-level `str` values are always heap `PyObj::Str` built by
//! `MKSTR`.

use crate::ast::*;
use crate::host::{binop as bop, iop, ops, unop, FuncDef, TryDef};
use fusevm::{Chunk, ChunkBuilder, Op, Value};
use std::collections::{HashMap, HashSet};

/// A compiled program: the top-level chunk plus the def/lambda/class-body
/// template table (indexed by def id) and the try-block table.
#[derive(Default)]
pub struct Program {
    pub main: Chunk,
    pub functions: Vec<(String, FuncDef)>,
    pub procs: Vec<FuncDef>,
    pub tries: Vec<TryDef>,
    /// Compile-time `SyntaxWarning`s to emit to stderr before running (e.g.
    /// `'return' in a 'finally' block`). `(line, keyword)`; carried through the
    /// bytecode cache so a cache hit warns identically to a fresh compile.
    pub warnings: Vec<(u32, String)>,
    /// Traceback-caret position tables: `(chunk op_hash, op-index → span)` for the
    /// main chunk and every function/block chunk. Registered into the host's
    /// position registry at run time (`load_merged`) and carried through the
    /// bytecode cache so a cache hit draws identical carets.
    pub positions: Vec<(u64, Vec<Span>)>,
}

/// Rebase every func-id and try-id reference in `prog` so its ids sit above the
/// ids already loaded on the host. Func ids appear as the `LoadInt` immediately
/// before a `CallBuiltin(MKFUNC|MKLAMBDA, _)`; try ids as the `LoadInt` before
/// `CallBuiltin(TRY, 1)`. A no-op for the common single-run case (both offsets
/// 0), needed only for the REPL and `import`.
pub fn rebase_program(prog: &mut Program, func_off: usize, try_off: usize) {
    if func_off == 0 && try_off == 0 {
        return;
    }
    rebase_chunk(&mut prog.main, func_off, try_off);
    for (_, f) in &mut prog.functions {
        rebase_chunk(&mut f.chunk, func_off, try_off);
    }
    for t in &mut prog.tries {
        rebase_chunk(&mut t.body, func_off, try_off);
        for (tc, _, hb) in &mut t.handlers {
            if let Some(tc) = tc {
                rebase_chunk(tc, func_off, try_off);
            }
            rebase_chunk(hb, func_off, try_off);
        }
        if let Some(e) = &mut t.orelse {
            rebase_chunk(e, func_off, try_off);
        }
        if let Some(f) = &mut t.finalbody {
            rebase_chunk(f, func_off, try_off);
        }
    }
}

fn rebase_chunk(chunk: &mut Chunk, func_off: usize, try_off: usize) {
    for i in 1..chunk.ops.len() {
        let off = match chunk.ops[i] {
            Op::CallBuiltin(id, _) if id == ops::MKFUNC || id == ops::MKLAMBDA => func_off,
            Op::CallBuiltin(id, 1) if id == ops::TRY || id == ops::LOOP_BODY => try_off,
            _ => continue,
        };
        if off == 0 {
            continue;
        }
        if let Op::LoadInt(v) = &mut chunk.ops[i - 1] {
            *v += off as i64;
        }
    }
    for sub in &mut chunk.sub_chunks {
        rebase_chunk(sub, func_off, try_off);
    }
}

/// Break/continue jump fixups for a native loop. When `signal` is set the loop
/// body runs as a `LOOP_BODY` sub-chunk, so `break`/`continue` emit control
/// signals instead of in-chunk jumps (they must cross a `try`/`with` boundary);
/// the `breaks`/`continues` fixup lists stay empty in that mode.
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    signal: bool,
}

#[derive(Default)]
pub struct Compiler {
    functions: Vec<(String, FuncDef)>,
    tries: Vec<TryDef>,
    loops: Vec<LoopCtx>,
    /// Active only while lowering the body of a *native* integer for-range loop
    /// (see `try_compile_native_range_for`): maps each loop-local Python name to a
    /// fusevm frame slot so `Name` loads/stores become `GetSlot`/`SetSlot` (direct
    /// Vec indexing) instead of `CallBuiltin(GETLOCAL/SETLOCAL)` (builtin dispatch
    /// + string-keyed env dict). The hot loop then contains only native ops, which
    /// the interpreter runs far faster and the AOT/JIT native tier can lower.
    native_slots: Option<HashMap<String, u16>>,
    /// Next free fusevm slot index to hand out for a loop bound while emitting a
    /// native for-range nest (names take slots `0..k`; each loop's `stop` bound
    /// takes the next slot). Meaningful only while `native_slots` is `Some`.
    native_next_slot: u16,
    tmp: usize,
    debug: bool,
    /// The source line of the statement currently being lowered. Call ops carry
    /// it so an uncaught exception's traceback can name each call-site frame's
    /// line (expression ops otherwise emit line 0).
    cur_line: u32,
    /// Number of enclosing real scopes (`def`/`lambda`/class body) — comprehension
    /// hidden functions do NOT count. Decides whether a walrus (`:=`) inside a
    /// comprehension leaks to module scope (`global`, depth 0) or the enclosing
    /// function (`nonlocal`, depth > 0), per PEP 572.
    fn_depth: usize,
    /// Bound-name set of each enclosing *function* scope (`def`/`lambda`/
    /// comprehension), innermost last. A class body is NOT pushed — `nonlocal`
    /// resolution skips class scope. Used to validate `nonlocal` targets at
    /// compile time (CPython raises `SyntaxError` for an unbound / module-level
    /// `nonlocal`).
    func_scopes: Vec<HashSet<String>>,
    /// The dotted prefix for the `__qualname__` of an entity defined at the
    /// current point: `""` at module level, `"outer.<locals>."` inside a
    /// function `outer`, `"C."` inside class `C`. Appended with the entity name
    /// to form its qualified name (CPython's `co_qualname`).
    qual_prefix: String,
    /// Compile-time `SyntaxWarning`s collected while lowering (see `Program`).
    warnings: Vec<(u32, String)>,
    /// Interactive ("single") compile mode: a top-level (`fn_depth == 0`)
    /// expression statement is routed through `sys.displayhook` (`ops::DISPLAYHOOK`)
    /// instead of being discarded, so the REPL echoes `repr(value)` for non-`None`
    /// results. Off for ordinary script compiles.
    interactive: bool,
    /// True while lowering a class body's own statements (not a nested def/class):
    /// a simple annotation (`x: int`) there records `x` into the class's
    /// `__annotations__` dict, so `dataclass`/`typing.NamedTuple` and
    /// `Cls.__annotations__` see the fields.
    in_class_body: bool,
    /// The source span of the caret-bearing expression currently being lowered,
    /// peeled from its `Expr::Spanned` wrapper. Recorded against the raising op's
    /// index so an uncaught exception can underline the exact sub-expression
    /// (CPython 3.11+ traceback carets). `Span::NONE` for synthetic nodes.
    node_span: Span,
    /// Set before lowering the direct call value of an `x = f(...)` / `return
    /// f(...)` statement; consumed by the outermost `Spanned` peel to mark that
    /// call's span `suppress` (CPython omits the caret when such a call raises).
    suppress_hint: bool,
    /// Per-chunk table of op-index → span, mirroring the currently-building
    /// `ChunkBuilder`. A stack because function/loop bodies build nested chunks;
    /// each frame is registered under its chunk's `op_hash` when the chunk is
    /// finished (see `begin_chunk`/`finish_chunk`).
    positions: Vec<Vec<Span>>,
    /// Finished per-chunk position tables paired with their chunk `op_hash`,
    /// accumulated as each chunk is built and handed to `Program.positions`.
    collected_positions: Vec<(u64, Vec<Span>)>,
}

/// The kind of scope a hidden function represents, controlling whether it is a
/// `nonlocal` resolution target (class bodies are transparent to `nonlocal`).
#[derive(Clone, Copy, PartialEq)]
enum ScopeKind {
    /// A `def`/`lambda`/comprehension: a real function scope for `nonlocal`.
    Function,
    /// A class body: names resolve dynamically and it is skipped by `nonlocal`.
    ClassBody,
}

/// Compile a parsed program. `debug` enables per-statement DAP line markers.
pub fn compile(stmts: &[Stmt], debug: bool) -> Result<Program, String> {
    compile_ex(stmts, debug, false)
}

/// Compile with the interactive ("single") flag set: top-level expression
/// statements echo their value via `sys.displayhook`. Used only by the REPL.
pub fn compile_interactive(stmts: &[Stmt]) -> Result<Program, String> {
    compile_ex(stmts, false, true)
}

fn compile_ex(stmts: &[Stmt], debug: bool, interactive: bool) -> Result<Program, String> {
    let mut c = Compiler {
        debug,
        interactive,
        ..Default::default()
    };
    let mut b = ChunkBuilder::new();
    c.begin_chunk();
    c.compile_stmts(&mut b, stmts)?;
    let main = c.finish_chunk(b);
    Ok(Program {
        main,
        functions: c.functions,
        procs: Vec::new(),
        tries: c.tries,
        warnings: c.warnings,
        positions: c.collected_positions,
    })
}

fn argc(n: usize) -> Result<u8, String> {
    u8::try_from(n).map_err(|_| "too many arguments (>255) for one call".to_string())
}

impl Compiler {
    // ── traceback-caret position tables ──────────────────────────────────
    /// Open a fresh position frame for a chunk about to be built. Pair with
    /// `finish_chunk` so the frame is registered and popped in balance.
    fn begin_chunk(&mut self) {
        self.positions.push(Vec::new());
    }
    /// Finalize `b` into a `Chunk` and register the open position frame under the
    /// chunk's `op_hash` (stable across the clones made per call), padded to the
    /// op count. `record_err_line` looks the table up at raise time.
    fn finish_chunk(&mut self, b: ChunkBuilder) -> Chunk {
        let c = b.build();
        let mut frame = self.positions.pop().unwrap_or_default();
        frame.resize(c.ops.len(), Span::NONE);
        // Collect rather than register: `load_merged` registers the whole set at
        // run time, so a cache-loaded program (which skips compilation) registers
        // identically from `Program.positions`.
        self.collected_positions.push((c.op_hash, frame));
        c
    }
    /// Record `self.node_span` (the caret-bearing expr being lowered) against the
    /// raising op at `idx` in the current chunk's position frame.
    fn record_span(&mut self, idx: usize) {
        let sp = self.node_span;
        if !sp.is_some() {
            return;
        }
        if let Some(frame) = self.positions.last_mut() {
            if frame.len() <= idx {
                frame.resize(idx + 1, Span::NONE);
            }
            frame[idx] = sp;
        }
    }

    // ── emit helpers ─────────────────────────────────────────────────────
    fn name_const(&self, b: &mut ChunkBuilder, s: &str) {
        let k = b.add_constant(Value::str(s));
        b.emit(Op::LoadConst(k), 0);
    }
    fn strlit(&self, b: &mut ChunkBuilder, s: &str) {
        let k = b.add_constant(Value::str(s));
        b.emit(Op::LoadConst(k), 0);
        b.emit(Op::CallBuiltin(ops::MKSTR, 1), 0);
    }

    fn compile_stmts(&mut self, b: &mut ChunkBuilder, stmts: &[Stmt]) -> Result<(), String> {
        for s in stmts {
            self.compile_stmt(b, s)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, b: &mut ChunkBuilder, s: &Stmt) -> Result<(), String> {
        if s.line != 0 {
            self.cur_line = s.line;
        }
        if self.debug && s.line != 0 {
            b.emit(Op::LoadInt(s.line as i64), s.line);
            b.emit(Op::CallBuiltin(ops::DBG_LINE, 1), s.line);
            b.emit(Op::Pop, s.line);
        }
        let line = s.line;
        match &s.kind {
            StmtKind::Expr(e) => {
                self.compile_expr(b, e)?;
                // Interactive REPL: echo a top-level expression's value through
                // `sys.displayhook` (CPython "single" mode). Nested scopes
                // (`def`/`lambda`/class body, `fn_depth > 0`) discard as normal.
                if self.interactive && self.fn_depth == 0 {
                    b.emit(Op::CallBuiltin(ops::DISPLAYHOOK, 1), line);
                    b.emit(Op::Pop, line);
                } else {
                    b.emit(Op::Pop, line);
                }
            }
            StmtKind::Pass => {}
            StmtKind::Assign { targets, value } => {
                // CPython omits the caret when a single `name = call(...)`'s call
                // raises; flag the value's call span so the renderer hides it.
                self.suppress_hint = targets.len() == 1
                    && matches!(targets[0].unspanned(), Expr::Name(_))
                    && matches!(value.unspanned(), Expr::Call { .. });
                self.compile_expr(b, value)?;
                self.suppress_hint = false;
                // Store to every target (dup for all but the last).
                for (i, t) in targets.iter().enumerate() {
                    if i + 1 < targets.len() {
                        b.emit(Op::Dup, line);
                    }
                    self.compile_assign(b, t)?;
                }
            }
            StmtKind::AnnAssign {
                target,
                annotation,
                value,
            } => {
                // In a class body, a *simple* (bare-name) annotation records
                // `__annotations__[name] = <annotation>` so dataclass /
                // typing.NamedTuple and `Cls.__annotations__` see the field.
                if self.in_class_body {
                    if let Expr::Name(n) = target.unspanned() {
                        // The annotation is compiled as a thunk so a forward
                        // reference (`x: SomeLaterName`) does not abort the class
                        // body — `TRY_ANNOTATION` records `__annotations__[key] =
                        // thunk()` and skips the entry on a NameError.
                        self.name_const(b, "__annotations__");
                        b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), self.cur_line); // [dict]
                        self.strlit(b, n); // [dict, key]
                        let ann_body =
                            vec![Stmt::from(StmtKind::Return(Some(annotation.clone())))];
                        let empty = Params::default();
                        self.fn_depth += 1;
                        let thunk_id = self.build_function("<annotate>", &empty, &ann_body);
                        self.fn_depth -= 1;
                        self.emit_make_func(b, thunk_id?, &empty)?; // [dict, key, thunk]
                        b.emit(Op::CallBuiltin(ops::TRY_ANNOTATION, 3), 0);
                    }
                }
                if let Some(v) = value {
                    self.compile_expr(b, v)?;
                    self.compile_assign(b, target)?;
                }
            }
            StmtKind::AugAssign { target, op, value } => {
                self.compile_augassign(b, target, *op, value)?;
            }
            StmtKind::If { test, body, orelse } => {
                self.compile_if(b, test, body, orelse)?;
            }
            StmtKind::While { test, body, orelse } => {
                self.compile_while(b, test, body, orelse)?;
            }
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                is_async,
            } => {
                if *is_async {
                    self.compile_async_for(b, target, iter, body, orelse)?;
                } else {
                    self.compile_for(b, target, iter, body, orelse)?;
                }
            }
            StmtKind::FuncDef {
                name,
                params,
                body,
                decorators,
                is_async,
            } => {
                self.compile_funcdef(b, name, params, body, decorators, *is_async)?;
            }
            StmtKind::ClassDef {
                name,
                bases,
                keywords,
                body,
                decorators,
            } => {
                self.compile_classdef(b, name, bases, keywords, body, decorators)?;
            }
            StmtKind::Return(e) => {
                match e {
                    Some(e) => {
                        // CPython omits the caret when `return name(...)`'s call
                        // raises (value is a `Call` on a bare `Name` func).
                        self.suppress_hint = matches!(
                            e.unspanned(),
                            Expr::Call { func, .. } if matches!(func.unspanned(), Expr::Name(_))
                        );
                        self.compile_expr(b, e)?;
                        self.suppress_hint = false;
                    }
                    None => {
                        b.emit(Op::LoadUndef, line);
                    }
                }
                b.emit(Op::CallBuiltin(ops::SIG_RETURN, 1), line);
            }
            StmtKind::Break => {
                let lc = self
                    .loops
                    .last_mut()
                    .ok_or("SyntaxError: 'break' outside loop")?;
                if lc.signal {
                    b.emit(Op::CallBuiltin(ops::SIG_BREAK, 0), line);
                    b.emit(Op::Pop, line);
                } else {
                    let j = b.emit(Op::Jump(0), line);
                    self.loops.last_mut().unwrap().breaks.push(j);
                }
            }
            StmtKind::Continue => {
                let lc = self
                    .loops
                    .last_mut()
                    .ok_or("SyntaxError: 'continue' outside loop")?;
                if lc.signal {
                    b.emit(Op::CallBuiltin(ops::SIG_CONTINUE, 0), line);
                    b.emit(Op::Pop, line);
                } else {
                    let j = b.emit(Op::Jump(0), line);
                    self.loops.last_mut().unwrap().continues.push(j);
                }
            }
            StmtKind::Delete(targets) => {
                for t in targets {
                    self.compile_delete(b, t)?;
                }
            }
            StmtKind::Global(names) => {
                for n in names {
                    self.name_const(b, n);
                    b.emit(Op::CallBuiltin(ops::DECLARE_GLOBAL, 1), line);
                    b.emit(Op::Pop, line);
                }
            }
            StmtKind::Nonlocal(names) => {
                for n in names {
                    self.check_nonlocal_binding(n)?;
                    self.name_const(b, n);
                    b.emit(Op::CallBuiltin(ops::DECLARE_NONLOCAL, 1), line);
                    b.emit(Op::Pop, line);
                }
            }
            StmtKind::Match { subject, cases } => {
                self.compile_match(b, subject, cases)?;
            }
            StmtKind::Raise { exc, cause } => match exc {
                Some(e) => match cause {
                    Some(c) => {
                        // `raise E from C`: push cause then exc; b_raise(2) pops
                        // exc first, then cause, and wires `__cause__`.
                        self.compile_expr(b, c)?;
                        self.compile_expr(b, e)?;
                        b.emit(Op::CallBuiltin(ops::RAISE, 2), line);
                    }
                    None => {
                        self.compile_expr(b, e)?;
                        b.emit(Op::CallBuiltin(ops::RAISE, 1), line);
                    }
                },
                None => {
                    b.emit(Op::CallBuiltin(ops::RERAISE, 0), line);
                    b.emit(Op::Pop, line);
                }
            },
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                self.compile_try(b, body, handlers, orelse, finalbody)?;
            }
            StmtKind::With {
                items,
                body,
                is_async,
            } => {
                self.compile_with(b, items, body, *is_async)?;
            }
            StmtKind::Assert { test, msg } => {
                self.compile_assert(b, test, msg)?;
            }
            StmtKind::Import(aliases) => {
                for a in aliases {
                    match &a.asname {
                        // `import a.b.c as x` binds the LEAF submodule to `x`.
                        Some(asname) => {
                            self.strlit(b, &a.name);
                            b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                            self.store_name(b, asname);
                        }
                        // `import a.b.c` imports the whole chain (each level bound
                        // on its parent) but binds only the TOP package `a` — so
                        // `a.b.c` is then reached by attribute access. CPython's
                        // IMPORT_NAME returns the top module when fromlist is empty.
                        None => {
                            let top = a.name.split('.').next().unwrap_or(&a.name).to_string();
                            self.strlit(b, &a.name);
                            b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                            if top != a.name {
                                // Discard the leaf, import the top package for the binding.
                                b.emit(Op::Pop, 0);
                                self.strlit(b, &top);
                                b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                            }
                            self.store_name(b, &top);
                        }
                    }
                }
            }
            StmtKind::ImportFrom {
                module,
                names,
                level,
            } => {
                let m = module.clone().unwrap_or_default();
                for a in names {
                    // A relative import (`from . import x`, `from ..pkg import y`)
                    // resolves at runtime against the module's `__package__`, since
                    // the leading dots' meaning depends on where the module lives.
                    if *level > 0 {
                        if a.name == "*" {
                            b.emit(Op::LoadInt(*level as i64), line);
                            self.strlit(b, &m);
                            self.strlit(b, "*");
                            b.emit(Op::CallBuiltin(ops::IMPORT_RELATIVE, 3), line);
                            b.emit(Op::CallBuiltin(ops::IMPORT_STAR, 1), line);
                            b.emit(Op::Pop, 0);
                            continue;
                        }
                        b.emit(Op::LoadInt(*level as i64), line);
                        self.strlit(b, &m);
                        self.strlit(b, &a.name);
                        b.emit(Op::CallBuiltin(ops::IMPORT_RELATIVE, 3), line);
                        let bind = a.asname.clone().unwrap_or_else(|| a.name.clone());
                        self.store_name(b, &bind);
                        continue;
                    }
                    // `from m import *`: import the module, then bind all of its
                    // public names in one op (which leaves an `Undef` to pop).
                    if a.name == "*" {
                        self.strlit(b, &m);
                        b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                        b.emit(Op::CallBuiltin(ops::IMPORT_STAR, 1), line);
                        b.emit(Op::Pop, 0);
                        continue;
                    }
                    self.strlit(b, &m);
                    b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                    self.strlit(b, &a.name);
                    b.emit(Op::CallBuiltin(ops::IMPORT_FROM, 2), line);
                    let bind = a.asname.clone().unwrap_or_else(|| a.name.clone());
                    self.store_name(b, &bind);
                }
            }
        }
        Ok(())
    }

    fn store_name(&self, b: &mut ChunkBuilder, name: &str) {
        // stack: [value] -> SETLOCAL([name, value]) -> value ; pop
        // Push name UNDER value: emit name then swap.
        if let Some(slot) = self.native_slots.as_ref().and_then(|m| m.get(name)) {
            b.emit(Op::SetSlot(*slot), 0); // consumes the value on top of stack
            return;
        }
        self.name_const(b, name);
        b.emit(Op::Swap, 0);
        b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
        b.emit(Op::Pop, 0);
    }

    fn compile_assign(&mut self, b: &mut ChunkBuilder, target: &Expr) -> Result<(), String> {
        // Value is on top of stack. Peel the parser's span wrapper so a target
        // like `x`, `a.b`, or `a[i]` (all `Spanned`) matches structurally.
        let target = target.unspanned();
        match target {
            Expr::Name(n) => {
                if let Some(slot) = self.native_slots.as_ref().and_then(|m| m.get(n)) {
                    b.emit(Op::SetSlot(*slot), 0); // consumes the value on top
                } else {
                    self.name_const(b, n);
                    b.emit(Op::Swap, 0);
                    b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
                    b.emit(Op::Pop, 0);
                }
            }
            Expr::Attribute(recv, attr) => {
                // stack: [value]; need [recv, name, value]
                self.compile_expr(b, recv)?; // [value, recv]
                self.name_const(b, attr); // [value, recv, name]
                b.emit(Op::Rot, 0); // rotate value to top: [recv, name, value]
                b.emit(Op::CallBuiltin(ops::SETATTR, 3), 0);
                b.emit(Op::Pop, 0);
            }
            Expr::Subscript(recv, idx) => {
                self.compile_expr(b, recv)?; // [value, recv]
                self.compile_subscript_index(b, idx)?; // [value, recv, idx]
                b.emit(Op::Rot, 0); // [recv, idx, value]
                b.emit(Op::CallBuiltin(ops::SETITEM, 3), 0);
                b.emit(Op::Pop, 0);
            }
            Expr::Tuple(items) | Expr::List(items) => {
                self.compile_unpack_assign(b, items)?;
            }
            Expr::Starred(inner) => self.compile_assign(b, inner)?,
            _ => return Err("SyntaxError: cannot assign to this expression".into()),
        }
        Ok(())
    }

    fn compile_unpack_assign(
        &mut self,
        b: &mut ChunkBuilder,
        items: &[Expr],
    ) -> Result<(), String> {
        let star_idx = items
            .iter()
            .position(|e| matches!(e, Expr::Starred(_)))
            .map(|i| i as i64)
            .unwrap_or(-1);
        // stack [iterable] -> UNPACK pushes items with items[0] on top.
        b.emit(Op::LoadInt(items.len() as i64), 0);
        b.emit(Op::LoadInt(star_idx), 0);
        b.emit(Op::CallBuiltin(ops::UNPACK, 3), 0);
        for t in items {
            self.compile_assign(b, t)?;
        }
        Ok(())
    }

    fn compile_delete(&mut self, b: &mut ChunkBuilder, target: &Expr) -> Result<(), String> {
        let target = target.unspanned();
        match target {
            Expr::Name(n) => {
                self.name_const(b, n);
                b.emit(Op::CallBuiltin(ops::DELNAME, 1), 0);
                b.emit(Op::Pop, 0);
            }
            Expr::Subscript(recv, idx) => {
                self.compile_expr(b, recv)?;
                self.compile_subscript_index(b, idx)?;
                b.emit(Op::CallBuiltin(ops::DELITEM, 2), 0);
                b.emit(Op::Pop, 0);
            }
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                b.emit(Op::CallBuiltin(ops::DELATTR, 2), 0);
                b.emit(Op::Pop, 0);
            }
            _ => return Err("SyntaxError: cannot delete this expression".into()),
        }
        Ok(())
    }

    /// The `ops::iop` tag for an augmented-assignment operator.
    fn iop_tag(op: BinOp) -> i64 {
        use iop::*;
        match op {
            BinOp::Add => ADD,
            BinOp::Sub => SUB,
            BinOp::Mul => MUL,
            BinOp::Div => DIV,
            BinOp::FloorDiv => FLOORDIV,
            BinOp::Mod => MOD,
            BinOp::Pow => POW,
            BinOp::MatMul => MATMUL,
            BinOp::BitAnd => BITAND,
            BinOp::BitOr => BITOR,
            BinOp::BitXor => BITXOR,
            BinOp::Shl => SHL,
            BinOp::Shr => SHR,
        }
    }

    /// `t op= v`: apply the in-place protocol (`INPLACE`) and rebind. `INPLACE`
    /// tries `type(t).__i<op>__(t, v)` — mutating `t` in place and preserving its
    /// identity for the mutable built-ins (`list`, `set`, `dict`, `bytearray`) and
    /// user `__iadd__`/… — falling back to `t = t <op> v` otherwise. The receiver
    /// and index of a subscript/attribute target are evaluated EXACTLY ONCE
    /// (CPython semantics) by binding them to synthetic temps first.
    fn compile_augassign(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        op: BinOp,
        value: &Expr,
    ) -> Result<(), String> {
        let tag = Self::iop_tag(op);
        // Peel for structural matching; the `Name` arm still lowers the original
        // (wrapped) `target` so its load records a caret span.
        match target.unspanned() {
            Expr::Name(n) => {
                // Native for-range loop body: a slot-local int target with `+`/`-`/`*`
                // lowers to GetSlot; <value>; Add/Sub/Mul; SetSlot — all native, no
                // INPLACE dispatch. (The interpreter still routes the arithmetic op
                // through pythonrs's numeric hook, so bignum promotion is preserved.)
                if let (Some(slot), Some(nop)) = (
                    self.native_slots.as_ref().and_then(|m| m.get(n).copied()),
                    native_arith_op(op),
                ) {
                    self.compile_expr(b, target)?; // GetSlot(slot)
                    self.compile_expr(b, value)?;
                    b.emit(nop, self.cur_line);
                    b.emit(Op::SetSlot(slot), 0);
                    return Ok(());
                }
                // A name target has no receiver side effect: load, apply, store.
                b.emit(Op::LoadInt(tag), 0);
                self.compile_expr(b, target)?;
                self.compile_expr(b, value)?;
                b.emit(Op::CallBuiltin(ops::INPLACE, 3), self.cur_line);
                self.compile_assign(b, target)?;
            }
            Expr::Attribute(recv, attr) => {
                // `recv.attr op= v`: evaluate `recv` once into a temp.
                let rt = format!(".aug{}", self.tmp);
                self.tmp += 1;
                self.compile_expr(b, recv)?;
                self.compile_assign(b, &Expr::Name(rt.clone()))?;
                let lval = Expr::Attribute(Box::new(Expr::Name(rt.clone())), attr.clone());
                b.emit(Op::LoadInt(tag), 0);
                self.compile_expr(b, &lval)?;
                self.compile_expr(b, value)?;
                b.emit(Op::CallBuiltin(ops::INPLACE, 3), self.cur_line);
                self.compile_assign(b, &lval)?;
            }
            Expr::Subscript(recv, idx) => {
                // `recv[idx] op= v`: evaluate `recv` and `idx` once into temps
                // (a slice index materializes to a slice object, still once).
                let rt = format!(".aug{}", self.tmp);
                self.tmp += 1;
                let it = format!(".aug{}", self.tmp);
                self.tmp += 1;
                self.compile_expr(b, recv)?;
                self.compile_assign(b, &Expr::Name(rt.clone()))?;
                self.compile_subscript_index(b, idx)?;
                self.compile_assign(b, &Expr::Name(it.clone()))?;
                let lval = Expr::Subscript(
                    Box::new(Expr::Name(rt.clone())),
                    Box::new(Expr::Name(it.clone())),
                );
                b.emit(Op::LoadInt(tag), 0);
                self.compile_expr(b, &lval)?;
                self.compile_expr(b, value)?;
                b.emit(Op::CallBuiltin(ops::INPLACE, 3), self.cur_line);
                self.compile_assign(b, &lval)?;
            }
            _ => return Err("SyntaxError: cannot assign to this expression".into()),
        }
        Ok(())
    }

    // ── control flow ─────────────────────────────────────────────────────
    fn compile_condition(&mut self, b: &mut ChunkBuilder, e: &Expr) -> Result<(), String> {
        self.compile_expr(b, e)?;
        b.emit(Op::CallBuiltin(ops::TRUTHY, 1), 0);
        Ok(())
    }

    fn compile_if(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        self.compile_condition(b, test)?;
        let jfalse = b.emit(Op::JumpIfFalse(0), 0);
        self.compile_stmts(b, body)?;
        if orelse.is_empty() {
            let end = b.current_pos();
            b.patch_jump(jfalse, end);
        } else {
            let jend = b.emit(Op::Jump(0), 0);
            let else_start = b.current_pos();
            b.patch_jump(jfalse, else_start);
            self.compile_stmts(b, orelse)?;
            let end = b.current_pos();
            b.patch_jump(jend, end);
        }
        Ok(())
    }

    fn compile_while(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        // Native fast path: a slot-safe integer `while` (no else) lowers to a
        // native loop over fusevm slots (native comparison + Add/Sub/Mul).
        if orelse.is_empty() && self.try_compile_native_while(b, test, body)? {
            return Ok(());
        }
        if loop_needs_signal(body) {
            return self.compile_while_signal(b, test, body, orelse);
        }
        let start = b.current_pos();
        self.compile_condition(b, test)?;
        let jfalse = b.emit(Op::JumpIfFalse(0), 0);
        self.loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
            signal: false,
        });
        self.compile_stmts(b, body)?;
        b.emit(Op::Jump(start), 0);
        let ctx = self.loops.pop().unwrap();
        for c in ctx.continues {
            b.patch_jump(c, start);
        }
        let after_cond = b.current_pos();
        b.patch_jump(jfalse, after_cond);
        if !orelse.is_empty() {
            self.compile_stmts(b, orelse)?;
        }
        let end = b.current_pos();
        for br in ctx.breaks {
            b.patch_jump(br, end);
        }
        Ok(())
    }

    /// Try to lower `for <name> in range(...): <body>` as a native counted loop
    /// over fusevm frame slots (no iterator protocol, no per-name env-dict
    /// dispatch, native `Add`/`Sub`/`Mul`). Returns `Ok(true)` if it emitted the
    /// native form, `Ok(false)` if the pattern didn't qualify (caller falls back
    /// to the general iterator loop). Applies only when: the target is a plain
    /// name; the iterable is `range(a)`/`range(a,b)`/`range(a,b,1)` with `range`
    /// unshadowed in an enclosing function scope; and every body statement is a
    /// slot-safe integer assign/aug-assign (`analyze_native_body`) that does not
    /// rebind the loop variable. Semantics preserved: the loop var is left bound
    /// to its last value (and unbound on an empty range — all write-backs sit
    /// behind the entry guard), and arithmetic stays bignum-correct because the
    /// interpreter runs `Add`/`Sub`/`Mul` through pythonrs's numeric hook (and the
    /// chunk is marked `int_overflow_deopt` so native compilation deopts on i64
    /// overflow rather than wrapping).
    fn try_compile_native_range_for(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        iter: &Expr,
        body: &[Stmt],
    ) -> Result<bool, String> {
        // Only the OUTERMOST loop of a nest reaches here (nested loops are emitted
        // directly by `emit_native_range_loop`), so `native_slots` must be inactive.
        if self.native_slots.is_some() {
            return Ok(false);
        }
        let target_name = match target.unspanned() {
            Expr::Name(n) => n.clone(),
            _ => return Ok(false),
        };
        let (start_expr, stop_expr) = match native_range_call(iter) {
            Some(b) => b,
            None => return Ok(false),
        };
        // A locally-rebound `range` is not the builtin — bail (rare). The check is
        // scope-wide, so it covers every `range(...)` in the nest.
        if self.func_scopes.iter().any(|s| s.contains("range")) {
            return Ok(false);
        }

        // Validate + collect the whole nest. `loop_vars` are bound by loops (get
        // their value from the loop, not the namespace); `reads` need a pre-loop
        // load; `writes` (accumulators + loop vars) are stored back afterward.
        let mut loop_vars = vec![target_name.clone()];
        let mut reads: Vec<String> = Vec::new();
        let mut writes = vec![target_name.clone()];
        if let Some(e) = start_expr {
            if !native_safe_value(e, &mut reads) {
                return Ok(false);
            }
        }
        if !native_safe_value(stop_expr, &mut reads) {
            return Ok(false);
        }
        if !analyze_native_tree(body, &mut loop_vars, &mut reads, &mut writes) {
            return Ok(false);
        }
        // Namespace loads = names read before being written (definite-assignment).
        let mut defined = vec![target_name.clone()];
        let mut load_names: Vec<String> = Vec::new();
        collect_ns_loads(body, &mut defined, &mut load_names);
        // For-range write-backs sit behind the empty-range guard, so an
        // unconditionally-`defined` name is valid too.
        if !native_writebacks_valid(&writes, &load_names, &defined, &loop_vars, true) {
            return Ok(false);
        }

        // Slot assignment: every distinct local name gets a slot; loop `stop`
        // bounds take slots above them (`native_next_slot`, per loop).
        let mut slots: HashMap<String, u16> = HashMap::new();
        let mut next: u16 = 0;
        for name in loop_vars.iter().chain(reads.iter()).chain(writes.iter()) {
            if !slots.contains_key(name) {
                slots.insert(name.clone(), next);
                next += 1;
            }
        }
        let target_slot = slots[&target_name];
        let outer_stop_slot = next;
        next += 1;

        // Namespace boundary: seed read-before-write locals; write back every
        // modified name (accumulators + all loop vars, deduped) once, at the end.
        let ns_loads: Vec<(String, u16)> =
            load_names.iter().map(|n| (n.clone(), slots[n])).collect();
        let mut wb: Vec<(String, u16)> = Vec::new();
        for name in &writes {
            if !wb.iter().any(|(w, _)| w == name) {
                wb.push((name.clone(), slots[name]));
            }
        }

        b.set_int_overflow_deopt(true);

        // Outer bounds are evaluated on the normal namespace path (before slot
        // redirection is active), then the nest runs with `native_slots` on.
        match start_expr {
            Some(e) => self.compile_expr(b, e)?,
            None => {
                b.emit(Op::LoadInt(0), 0);
            }
        }
        b.emit(Op::SetSlot(target_slot), 0);
        self.compile_expr(b, stop_expr)?;
        b.emit(Op::SetSlot(outer_stop_slot), 0);

        self.native_slots = Some(slots);
        self.native_next_slot = next;
        let res = self.emit_loop_core(b, target_slot, outer_stop_slot, body, &ns_loads, &wb);
        self.native_slots = None;
        res?;
        Ok(true)
    }

    /// Emit a nested native range loop (called only while `native_slots` is
    /// active). Its bounds are evaluated through slot redirection (so they may
    /// reference enclosing loop variables), then the shared loop core runs with no
    /// namespace load/write-back — the outermost loop owns that boundary.
    fn emit_native_range_loop(
        &mut self,
        b: &mut ChunkBuilder,
        target_name: &str,
        start_expr: Option<&Expr>,
        stop_expr: &Expr,
        body: &[Stmt],
    ) -> Result<(), String> {
        let target_slot = self.native_slots.as_ref().unwrap()[target_name];
        let stop_slot = self.native_next_slot;
        self.native_next_slot += 1;
        match start_expr {
            Some(e) => self.compile_expr(b, e)?,
            None => {
                b.emit(Op::LoadInt(0), 0);
            }
        }
        b.emit(Op::SetSlot(target_slot), 0);
        self.compile_expr(b, stop_expr)?;
        b.emit(Op::SetSlot(stop_slot), 0);
        self.emit_loop_core(b, target_slot, stop_slot, body, &[], &[])
    }

    /// The shared counted-loop body: entry guard (empty range skips everything),
    /// optional post-guard namespace loads, the loop body (nested range loops
    /// recurse; other statements lower through slot redirection), induction +
    /// continue test, last-value restore, and optional namespace write-backs. The
    /// target/stop slots must already hold the evaluated bounds.
    fn emit_loop_core(
        &mut self,
        b: &mut ChunkBuilder,
        target_slot: u16,
        stop_slot: u16,
        body: &[Stmt],
        ns_loads: &[(String, u16)],
        ns_writebacks: &[(String, u16)],
    ) -> Result<(), String> {
        b.emit(Op::GetSlot(target_slot), 0);
        b.emit(Op::GetSlot(stop_slot), 0);
        b.emit(Op::NumLt, 0);
        let jskip = b.emit(Op::JumpIfFalse(0), 0);

        for (name, slot) in ns_loads {
            self.name_const(b, name);
            b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0);
            b.emit(Op::SetSlot(*slot), 0);
        }

        let top = b.current_pos();
        self.emit_native_body(b, body)?;

        b.emit(Op::GetSlot(target_slot), 0);
        b.emit(Op::LoadInt(1), 0);
        b.emit(Op::Add, 0);
        b.emit(Op::SetSlot(target_slot), 0);
        b.emit(Op::GetSlot(target_slot), 0);
        b.emit(Op::GetSlot(stop_slot), 0);
        b.emit(Op::NumLt, 0);
        b.emit(Op::JumpIfTrue(top), 0);
        // Fell out one step past the last value; restore to last body value.
        b.emit(Op::GetSlot(target_slot), 0);
        b.emit(Op::LoadInt(1), 0);
        b.emit(Op::Sub, 0);
        b.emit(Op::SetSlot(target_slot), 0);

        // Write slots back to the Python namespace. Emit the store RAW (not via
        // `store_name`) because `native_slots` is still active here and would
        // otherwise redirect the store right back into the slot.
        for (name, slot) in ns_writebacks {
            b.emit(Op::GetSlot(*slot), 0); // [value]
            self.name_const(b, name); // [value, name]
            b.emit(Op::Swap, 0); // [name, value]
            b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
            b.emit(Op::Pop, 0);
        }

        let after = b.current_pos();
        b.patch_jump(jskip, after);
        Ok(())
    }

    /// Emit the statements of a native loop/branch body (slot redirection already
    /// active): nested range-fors, native `while`s, and native `if`s recurse into
    /// their own emitters; every other statement (slot-safe assign/aug-assign/
    /// pass) lowers through the normal path with `native_slots` on.
    fn emit_native_body(&mut self, b: &mut ChunkBuilder, body: &[Stmt]) -> Result<(), String> {
        for stmt in body {
            match &stmt.kind {
                StmtKind::For {
                    target, iter, body: inner, ..
                } => {
                    let tname = match target.unspanned() {
                        Expr::Name(n) => n.clone(),
                        _ => unreachable!("native nest validated"),
                    };
                    let (istart, istop) =
                        native_range_call(iter).expect("native nest validated");
                    self.emit_native_range_loop(b, &tname, istart, istop, inner)?;
                }
                StmtKind::While { test, body: wbody, .. } => {
                    self.emit_native_while(b, test, wbody)?;
                }
                StmtKind::If { test, body: tb, orelse } => {
                    self.emit_native_if(b, test, tb, orelse)?;
                }
                _ => self.compile_stmts(b, std::slice::from_ref(stmt))?,
            }
        }
        Ok(())
    }

    /// Push a native boolean for a validated native condition (see
    /// `native_safe_cond`): a single arithmetic comparison (`NumLt`/`NumEq`/…), or
    /// the truthiness of an integer expression (`x != 0`). No `TRUTHY` builtin, so
    /// the branch stays lowerable.
    fn emit_native_cond(&mut self, b: &mut ChunkBuilder, test: &Expr) -> Result<(), String> {
        match test.unspanned() {
            Expr::Compare(lhs, rest)
                if rest.len() == 1 && native_cmp_op(rest[0].0).is_some() =>
            {
                self.compile_expr(b, lhs)?;
                self.compile_expr(b, &rest[0].1)?;
                b.emit(native_cmp_op(rest[0].0).unwrap(), 0);
            }
            other => {
                // Integer truthiness: `x != 0`.
                self.compile_expr(b, other)?;
                b.emit(Op::LoadInt(0), 0);
                b.emit(Op::NumNe, 0);
            }
        }
        Ok(())
    }

    /// Emit a native `if`/`else` inside a native loop body (no `TRUTHY`).
    fn emit_native_if(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        then_body: &[Stmt],
        else_body: &[Stmt],
    ) -> Result<(), String> {
        self.emit_native_cond(b, test)?;
        let jelse = b.emit(Op::JumpIfFalse(0), 0);
        self.emit_native_body(b, then_body)?;
        if else_body.is_empty() {
            let end = b.current_pos();
            b.patch_jump(jelse, end);
        } else {
            let jend = b.emit(Op::Jump(0), 0);
            let else_start = b.current_pos();
            b.patch_jump(jelse, else_start);
            self.emit_native_body(b, else_body)?;
            let end = b.current_pos();
            b.patch_jump(jend, end);
        }
        Ok(())
    }

    /// Emit a native `while <cond>:` inside a native loop body (the enclosing
    /// range loop owns the namespace boundary; the condition is re-tested each
    /// iteration through slot reads).
    fn emit_native_while(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        body: &[Stmt],
    ) -> Result<(), String> {
        // Bottom-tested: an entry guard checks the condition once, then the hot
        // path is `body; cond; JumpIfTrue(top)` — a single backward branch and no
        // forward exit branch, so the tracing JIT records the whole iteration
        // (a top-tested `JumpIfFalse` exit in the hot path would break the trace).
        self.emit_native_cond(b, test)?;
        let jskip = b.emit(Op::JumpIfFalse(0), 0);
        let top = b.current_pos();
        self.emit_native_body(b, body)?;
        self.emit_native_cond(b, test)?;
        b.emit(Op::JumpIfTrue(top), 0);
        let end = b.current_pos();
        b.patch_jump(jskip, end);
        Ok(())
    }

    /// Try to lower a standalone `while <cond>: <body>` as a native loop over
    /// fusevm slots. Qualifies only when the condition and body are native-safe
    /// (`native_safe_cond` / `analyze_native_tree`) AND every assigned name is
    /// also read (so its slot is always loaded from the namespace and the
    /// write-back is valid — a `while` has no counted-loop init to seed a
    /// write-only slot). Returns `Ok(true)` if it emitted the native form.
    fn try_compile_native_while(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        body: &[Stmt],
    ) -> Result<bool, String> {
        if self.native_slots.is_some() || loop_needs_signal(body) {
            return Ok(false);
        }
        if self.func_scopes.iter().any(|s| s.contains("range")) {
            return Ok(false);
        }
        let mut loop_vars: Vec<String> = Vec::new();
        let mut reads: Vec<String> = Vec::new();
        let mut writes: Vec<String> = Vec::new();
        if !native_safe_cond(test, &mut reads) {
            return Ok(false);
        }
        if !analyze_native_tree(body, &mut loop_vars, &mut reads, &mut writes) {
            return Ok(false);
        }
        // A `while` has no guard-gated write-back (it can run zero times), so every
        // written name must be read-before-write — loaded before the loop — for its
        // write-back to be valid. Condition reads come first, then the body.
        let mut defined: Vec<String> = Vec::new();
        let mut load_names: Vec<String> = Vec::new();
        reads_needing_load(test, &defined, &mut load_names);
        collect_ns_loads(body, &mut defined, &mut load_names);
        if !native_writebacks_valid(&writes, &load_names, &defined, &loop_vars, false) {
            return Ok(false);
        }

        let mut slots: HashMap<String, u16> = HashMap::new();
        let mut next: u16 = 0;
        for name in reads.iter().chain(writes.iter()) {
            if !slots.contains_key(name) {
                slots.insert(name.clone(), next);
                next += 1;
            }
        }
        let ns_loads: Vec<(String, u16)> =
            load_names.iter().map(|n| (n.clone(), slots[n])).collect();
        let mut wb: Vec<(String, u16)> = Vec::new();
        for name in &writes {
            if !wb.iter().any(|(w, _)| w == name) {
                wb.push((name.clone(), slots[name]));
            }
        }

        b.set_int_overflow_deopt(true);
        self.native_slots = Some(slots);
        self.native_next_slot = next;

        // Load reads once, loop with the condition re-tested each iteration, then
        // write every modified name back (emitted raw — native_slots still on).
        for (name, slot) in &ns_loads {
            self.name_const(b, name);
            b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0);
            b.emit(Op::SetSlot(*slot), 0);
        }
        let res = self.emit_native_while(b, test, body);
        if res.is_ok() {
            for (name, slot) in &wb {
                b.emit(Op::GetSlot(*slot), 0);
                self.name_const(b, name);
                b.emit(Op::Swap, 0);
                b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
                b.emit(Op::Pop, 0);
            }
        }
        self.native_slots = None;
        res?;
        Ok(true)
    }

    fn compile_for(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        iter: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        // Fast path: a `for i in range(...)` whose body is slot-safe integer
        // arithmetic lowers to a native counted loop (fusevm slots + Add/Sub/Mul),
        // no iterator protocol and no per-name env-dict dispatch. Only when there
        // is no `else` clause and no signal-crossing body.
        if orelse.is_empty() && !loop_needs_signal(body) && self.try_compile_native_range_for(b, target, iter, body)? {
            return Ok(());
        }
        if loop_needs_signal(body) {
            return self.compile_for_signal(b, target, iter, body, orelse);
        }
        self.compile_expr(b, iter)?;
        b.emit(Op::CallBuiltin(ops::GETITER, 1), 0); // [iterator]
        let start = b.current_pos();
        b.emit(Op::CallBuiltin(ops::FORITER, 0), 0); // [iterator, value, has_next]
        let jdone = b.emit(Op::JumpIfFalse(0), 0); // pops has_next
                                                   // [iterator, value] — assign to target.
        self.compile_assign(b, target)?; // pops value -> [iterator]
        self.loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
            signal: false,
        });
        self.compile_stmts(b, body)?;
        b.emit(Op::Jump(start), 0);
        let ctx = self.loops.pop().unwrap();
        for c in ctx.continues {
            b.patch_jump(c, start);
        }
        // Normal exhaustion: [iterator] -> pop, run else.
        let done = b.current_pos();
        b.patch_jump(jdone, done);
        b.emit(Op::Pop, 0); // drop iterator
        if !orelse.is_empty() {
            self.compile_stmts(b, orelse)?;
        }
        let jafter = b.emit(Op::Jump(0), 0);
        // break target: [iterator] -> pop.
        let break_target = b.current_pos();
        b.emit(Op::Pop, 0);
        let end = b.current_pos();
        b.patch_jump(jafter, end);
        for br in ctx.breaks {
            b.patch_jump(br, break_target);
        }
        Ok(())
    }

    /// Compile `body` (in signal mode) into a sub-chunk and register it in the
    /// try table, returning its id for a `LOOP_BODY` op. `break`/`continue` in
    /// `body` emit control signals (they cross a `try`/`with` boundary).
    ///
    /// A `yield` inside `body` suspends correctly through the `LOOP_BODY` sub-chunk
    /// (the coroutine yielder is thread-published, not chunk-scoped), so this path
    /// serves generators too — the signal lowering is chosen purely on whether a
    /// `break`/`continue` crosses a `try`/`with`, independent of `yield`. (A loop
    /// with `yield` AND such a crossing must use this path: the native jump-patch
    /// path would try to patch a `continue` compiled into a try-handler's own
    /// chunk, panicking `patch_jump on non-jump op`.)
    fn register_loop_body(&mut self, body: &[Stmt]) -> Result<usize, String> {
        self.loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
            signal: true,
        });
        let chunk = self.compile_block_chunk(body);
        self.loops.pop();
        let chunk = chunk?;
        let id = self.tries.len();
        self.tries.push(TryDef {
            body: chunk,
            handlers: Vec::new(),
            orelse: None,
            finalbody: None,
        });
        Ok(id)
    }

    /// `while` whose body `break`/`continue` cross a `try`/`with` boundary: the
    /// body runs as a `LOOP_BODY` sub-chunk that returns `0` (next iteration) or
    /// `1` (break); a `return` inside is propagated by the op itself.
    fn compile_while_signal(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        let id = self.register_loop_body(body)?;
        let start = b.current_pos();
        self.compile_condition(b, test)?;
        let jfalse = b.emit(Op::JumpIfFalse(0), 0);
        b.emit(Op::LoadInt(id as i64), 0);
        b.emit(Op::CallBuiltin(ops::LOOP_BODY, 1), 0); // [code]
        let jbreak = b.emit(Op::JumpIfTrue(0), 0); // pops code; 1 -> break
        b.emit(Op::Jump(start), 0); // 0 -> re-test
        let after_cond = b.current_pos();
        b.patch_jump(jfalse, after_cond);
        if !orelse.is_empty() {
            self.compile_stmts(b, orelse)?;
        }
        let end = b.current_pos();
        b.patch_jump(jbreak, end);
        Ok(())
    }

    /// `for` counterpart of `compile_while_signal`.
    fn compile_for_signal(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        iter: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        let id = self.register_loop_body(body)?;
        self.compile_expr(b, iter)?;
        b.emit(Op::CallBuiltin(ops::GETITER, 1), 0); // [iterator]
        let start = b.current_pos();
        b.emit(Op::CallBuiltin(ops::FORITER, 0), 0); // [iterator, value, has_next] | [iterator, false]
        let jdone = b.emit(Op::JumpIfFalse(0), 0); // pops has_next
        self.compile_assign(b, target)?; // pops value -> [iterator]
        b.emit(Op::LoadInt(id as i64), 0);
        b.emit(Op::CallBuiltin(ops::LOOP_BODY, 1), 0); // [iterator, code]
        let jbreak = b.emit(Op::JumpIfTrue(0), 0); // pops code; 1 -> break; [iterator]
        b.emit(Op::Jump(start), 0); // 0 -> next iteration
        let done = b.current_pos();
        b.patch_jump(jdone, done);
        b.emit(Op::Pop, 0); // drop iterator on exhaustion
        if !orelse.is_empty() {
            self.compile_stmts(b, orelse)?;
        }
        let jafter = b.emit(Op::Jump(0), 0);
        let break_target = b.current_pos();
        b.emit(Op::Pop, 0); // drop iterator on break
        let end = b.current_pos();
        b.patch_jump(jafter, end);
        b.patch_jump(jbreak, break_target);
        Ok(())
    }

    /// `async for target in aiter: body [else: orelse]` — desugar to the CPython
    /// protocol: `_it = aiter.__aiter__()` then a loop driving
    /// `await _it.__anext__()`, breaking on `StopAsyncIteration`. A `.run` flag
    /// routes normal exhaustion through the `while ... else` so the `else` clause
    /// fires on exhaustion but not on a body `break`.
    fn compile_async_for(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        iter: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
        // `_run` flag routes exhaustion through `while ... else` (so `else` fires
        // on `StopAsyncIteration` but not on a body `break`). The except handler
        // only clears `_run` — no `break`/`continue` crosses the try boundary (a
        // handler body compiles to its own chunk, so a jump out would dangle);
        // an `if _run:` guard then gates the loop body.
        let ait = format!(".aiter{}", self.tmp);
        let run = format!(".arun{}", self.tmp);
        let val = format!(".aval{}", self.tmp);
        self.tmp += 1;
        let mut pre: Vec<Stmt> = Vec::new();
        // _it = aiter.__aiter__()
        pre.push(
            StmtKind::Assign {
                targets: vec![Expr::Name(ait.clone())],
                value: Expr::Call {
                    func: Box::new(Expr::Attribute(Box::new(iter.clone()), "__aiter__".into())),
                    args: vec![],
                    keywords: vec![],
                },
            }
            .into(),
        );
        // _run = True
        pre.push(
            StmtKind::Assign {
                targets: vec![Expr::Name(run.clone())],
                value: Expr::True,
            }
            .into(),
        );
        // try: _val = await _it.__anext__()  except StopAsyncIteration: _run=False
        let anext = Expr::Await(Box::new(Expr::Call {
            func: Box::new(Expr::Attribute(
                Box::new(Expr::Name(ait)),
                "__anext__".into(),
            )),
            args: vec![],
            keywords: vec![],
        }));
        let try_stmt: Stmt = StmtKind::Try {
            body: vec![StmtKind::Assign {
                targets: vec![Expr::Name(val.clone())],
                value: anext,
            }
            .into()],
            handlers: vec![ExceptHandler {
                typ: Some(Expr::Name("StopAsyncIteration".into())),
                name: None,
                body: vec![StmtKind::Assign {
                    targets: vec![Expr::Name(run.clone())],
                    value: Expr::False,
                }
                .into()],
                star: false,
            }],
            orelse: vec![],
            finalbody: vec![],
        }
        .into();
        // if _run: <target> = _val; <body>
        let mut guarded: Vec<Stmt> = vec![StmtKind::Assign {
            targets: vec![target.clone()],
            value: Expr::Name(val),
        }
        .into()];
        guarded.extend_from_slice(body);
        let loop_body: Vec<Stmt> = vec![
            try_stmt,
            StmtKind::If {
                test: Expr::Name(run.clone()),
                body: guarded,
                orelse: vec![],
            }
            .into(),
        ];
        pre.push(
            StmtKind::While {
                test: Expr::Name(run),
                body: loop_body,
                orelse: orelse.to_vec(),
            }
            .into(),
        );
        self.compile_stmts(b, &pre)
    }

    fn compile_assert(
        &mut self,
        b: &mut ChunkBuilder,
        test: &Expr,
        msg: &Option<Expr>,
    ) -> Result<(), String> {
        self.compile_condition(b, test)?;
        let jok = b.emit(Op::JumpIfTrue(0), 0);
        match msg {
            Some(m) => self.compile_expr(b, m)?,
            None => {
                b.emit(Op::LoadUndef, 0);
            }
        }
        b.emit(Op::CallBuiltin(ops::ASSERT_FAIL, 1), 0);
        let ok = b.current_pos();
        b.patch_jump(jok, ok);
        Ok(())
    }

    // ── functions / classes ──────────────────────────────────────────────
    fn compile_funcdef(
        &mut self,
        b: &mut ChunkBuilder,
        name: &str,
        params: &Params,
        body: &[Stmt],
        decorators: &[Expr],
        is_async: bool,
    ) -> Result<(), String> {
        self.fn_depth += 1;
        let def_id = self.build_function_ex(name, params, body, is_async, ScopeKind::Function);
        self.fn_depth -= 1;
        let def_id = def_id?;
        self.emit_make_func(b, def_id, params)?;
        // Apply decorators (innermost first).
        for d in decorators.iter().rev() {
            self.compile_expr(b, d)?; // [func, dec]
            b.emit(Op::Swap, 0); // [dec, func]
            b.emit(Op::CallBuiltin(ops::CALL_VALUE, 2), 0);
        }
        self.store_name(b, name);
        Ok(())
    }

    /// Emit the `MKFUNC` sequence for `def_id`: push the `__annotations__` dict
    /// (bottom, evaluated at def time), the evaluated positional defaults, then the
    /// keyword-only defaults, a count of them, and the func id (kept immediately
    /// below `MKFUNC` so id-rebasing still finds it). Assumes nothing this call
    /// needs is already on the stack.
    fn emit_make_func(
        &mut self,
        b: &mut ChunkBuilder,
        def_id: usize,
        params: &Params,
    ) -> Result<(), String> {
        // `__annotations__` — the deepest arg, so it does not disturb the func-id
        // `LoadInt` that must stay right below `MKFUNC`. With annotations present,
        // emit a THUNK that builds the `{name: value, …}` dict instead of building
        // it inline: `MKFUNC` evaluates the thunk with any forward-reference
        // `NameError` caught, so a self-referential annotation (`def m(self) ->
        // IO[AnyStr]`, or a package's forward-ref type alias) no longer aborts the
        // definition. An unannotated function keeps its plain empty dict.
        if params.annotations.is_empty() {
            b.emit(Op::CallBuiltin(ops::MKDICT, 0), 0);
        } else {
            let dict_expr = Expr::Dict(
                params
                    .annotations
                    .iter()
                    .map(|(name, ann)| (Some(Expr::Str(name.clone())), ann.clone()))
                    .collect(),
            );
            let bodystmt = vec![Stmt::from(StmtKind::Return(Some(dict_expr)))];
            let empty = Params::default();
            self.fn_depth += 1;
            let thunk_id = self.build_function("<annotate>", &empty, &bodystmt);
            self.fn_depth -= 1;
            self.emit_make_func(b, thunk_id?, &empty)?; // pushes the thunk value
        }
        for d in &params.defaults {
            self.compile_expr(b, d)?;
        }
        let mut nkw = 0usize;
        for e in params.kwonly_defaults.iter().flatten() {
            self.compile_expr(b, e)?;
            nkw += 1;
        }
        b.emit(Op::LoadInt(nkw as i64), 0); // keyword-only default count
        b.emit(Op::LoadInt(def_id as i64), 0); // func id (immediately below MKFUNC)
        let total = 1 + params.defaults.len() + nkw + 2; // annotations + defaults + count + func id
        b.emit(Op::CallBuiltin(ops::MKFUNC, argc(total)?), 0);
        Ok(())
    }

    fn build_function(
        &mut self,
        name: &str,
        params: &Params,
        body: &[Stmt],
    ) -> Result<usize, String> {
        self.build_function_ex(name, params, body, false, ScopeKind::Function)
    }

    /// Validate a `nonlocal name` declaration at compile time, mirroring CPython:
    /// at module level (no enclosing function) it is a `SyntaxError`, and if no
    /// enclosing function scope binds `name` it is `no binding for nonlocal`.
    fn check_nonlocal_binding(&self, name: &str) -> Result<(), String> {
        let n = self.func_scopes.len();
        // `func_scopes.last()` is the current function; a `nonlocal` targets an
        // ENCLOSING one, so the current scope is excluded from the search.
        if n == 0 {
            return Err("SyntaxError: nonlocal declaration not allowed at module level".into());
        }
        let bound = self.func_scopes[..n - 1].iter().any(|s| s.contains(name));
        if !bound {
            return Err(format!(
                "SyntaxError: no binding for nonlocal '{name}' found"
            ));
        }
        Ok(())
    }

    fn build_function_ex(
        &mut self,
        name: &str,
        params: &Params,
        body: &[Stmt],
        is_async: bool,
        kind: ScopeKind,
    ) -> Result<usize, String> {
        self.build_function_qn(name, name, params, body, is_async, kind)
    }

    /// As [`build_function_ex`], but `qual_name` (the entity's user-facing name)
    /// drives `__qualname__` and the nested prefix independently of `name` (the
    /// chunk/traceback name). They differ for a class body, whose chunk name is
    /// `<class C>` while its qualname component is `C`.
    fn build_function_qn(
        &mut self,
        name: &str,
        qual_name: &str,
        params: &Params,
        body: &[Stmt],
        is_async: bool,
        kind: ScopeKind,
    ) -> Result<usize, String> {
        // `__qualname__`: the dotted path to this entity. Its own name is joined
        // to the enclosing prefix; its body then defines nested entities under an
        // extended prefix (`.<locals>.` for a function, `.` for a class body).
        let qualname = format!("{}{}", self.qual_prefix, qual_name);
        let saved_prefix = self.qual_prefix.clone();
        self.qual_prefix = match kind {
            ScopeKind::ClassBody => format!("{qualname}."),
            ScopeKind::Function => format!("{qualname}.<locals>."),
        };
        // The scope's local names: everything assigned in the body, minus names
        // it declares `global`/`nonlocal`. Reading one before it is bound is an
        // `UnboundLocalError` at runtime. A class body resolves names dynamically,
        // so it carries no local set (unbound reads there are `NameError`).
        let locals = if kind == ScopeKind::ClassBody {
            Vec::new()
        } else {
            scope_locals(body)
        };
        // Free variables (`co_freevars`) — computed against the ENCLOSING function
        // scopes, before this scope is pushed. Class bodies form no closure.
        let freevars = if kind == ScopeKind::Function {
            scope_freevars(params, body, &self.func_scopes)
        } else {
            Vec::new()
        };
        // Push this scope's bound names (locals + params) so a nested `nonlocal`
        // can resolve against it. Class bodies are transparent to `nonlocal`.
        let mut pushed = false;
        if kind == ScopeKind::Function {
            let mut bound: HashSet<String> = locals.iter().cloned().collect();
            for p in param_names(params) {
                bound.insert(p);
            }
            self.func_scopes.push(bound);
            pushed = true;
        }
        let mut fb = ChunkBuilder::new();
        self.begin_chunk();
        // A class body captures its simple annotations into `__annotations__`; a
        // nested def/class resets the flag so its own annotations don't leak into
        // the enclosing class's dict.
        let saved_icb = self.in_class_body;
        self.in_class_body = kind == ScopeKind::ClassBody;
        if self.in_class_body && body.iter().any(is_ann_assign) {
            // `__annotations__ = {}` at the top of the class body.
            fb.emit(Op::CallBuiltin(ops::MKDICT, 0), 0);
            self.name_const(&mut fb, "__annotations__");
            fb.emit(Op::Swap, 0);
            fb.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
            fb.emit(Op::Pop, 0);
        }
        let compiled = self.compile_stmts(&mut fb, body);
        self.in_class_body = saved_icb;
        if pushed {
            self.func_scopes.pop();
        }
        self.qual_prefix = saved_prefix;
        compiled?;
        let is_generator = body_has_yield(body);
        let def = FuncDef {
            name: name.to_string(),
            qualname,
            params: params.names.clone(),
            posonly: params.posonly,
            ndefaults: params.defaults.len(),
            star: params.star.clone(),
            kwonly: params.kwonly.clone(),
            kwonly_required: params.kwonly_defaults.iter().map(|d| d.is_none()).collect(),
            kwargs: params.kwargs.clone(),
            chunk: self.finish_chunk(fb),
            locals,
            is_generator,
            is_async,
            doc: docstring(body),
            freevars,
        };
        self.functions.push((name.to_string(), def));
        Ok(self.functions.len() - 1)
    }

    fn compile_classdef(
        &mut self,
        b: &mut ChunkBuilder,
        name: &str,
        bases: &[Expr],
        keywords: &[Keyword],
        body: &[Stmt],
        decorators: &[Expr],
    ) -> Result<(), String> {
        // Class body compiles as a parameterless function that assigns members
        // into its local env; BUILD_CLASS captures that env as the namespace.
        let empty = Params::default();
        self.fn_depth += 1;
        let def_id = self.build_function_qn(
            &format!("<class {name}>"),
            name,
            &empty,
            body,
            false,
            ScopeKind::ClassBody,
        );
        self.fn_depth -= 1;
        let def_id = def_id?;
        // The explicit metaclass (`class A(metaclass=M)`), or `None` — BUILD_CLASS
        // pops it below the other args and, if a real type, drives construction.
        match keywords
            .iter()
            .find(|k| k.name.as_deref() == Some("metaclass"))
        {
            Some(k) => self.compile_expr(b, &k.value)?,
            None => {
                b.emit(Op::LoadUndef, 0);
            }
        }
        // bases list
        for base in bases {
            self.compile_expr(b, base)?;
        }
        b.emit(Op::CallBuiltin(ops::MKLIST, argc(bases.len())?), 0); // [meta, bases]
        self.name_const(b, name); // [meta, bases, name]
        self.emit_make_func(b, def_id, &empty)?; // [meta, bases, name, bodyfunc]
                                                 // The remaining (non-`metaclass`) class keywords become a dict passed to
                                                 // `__init_subclass__` (`class C(P, tag="x")`). Named keywords only; a
                                                 // `**`-spread in a class header is uncommon and left unexpanded.
        let ckw: Vec<&Keyword> = keywords
            .iter()
            .filter(|k| k.name.is_some() && k.name.as_deref() != Some("metaclass"))
            .collect();
        for kw in &ckw {
            self.strlit(b, kw.name.as_ref().unwrap());
            self.compile_expr(b, &kw.value)?;
        }
        b.emit(Op::CallBuiltin(ops::MKDICT, argc(ckw.len() * 2)?), 0); // [meta,bases,name,bodyfunc,kwargs]
        b.emit(Op::CallBuiltin(ops::BUILD_CLASS, 5), 0); // -> class value
        for d in decorators.iter().rev() {
            self.compile_expr(b, d)?;
            b.emit(Op::Swap, 0);
            b.emit(Op::CallBuiltin(ops::CALL_VALUE, 2), 0);
        }
        self.store_name(b, name);
        Ok(())
    }

    // ── try / with ───────────────────────────────────────────────────────
    fn compile_try(
        &mut self,
        b: &mut ChunkBuilder,
        body: &[Stmt],
        handlers: &[ExceptHandler],
        orelse: &[Stmt],
        finalbody: &[Stmt],
    ) -> Result<(), String> {
        // CPython's `SyntaxWarning: '<kw>' in a 'finally' block` for a
        // `return`/`break`/`continue` that would jump out of THIS `finally`.
        if !finalbody.is_empty() {
            collect_finally_escapes(finalbody, &mut self.warnings);
        }
        let body_chunk = self.compile_block_chunk(body)?;
        let mut hs = Vec::new();
        for h in handlers {
            let type_chunk = match &h.typ {
                Some(t) => Some(self.compile_expr_chunk(t)?),
                None => None,
            };
            let hbody = self.compile_block_chunk(&h.body)?;
            hs.push((type_chunk, h.name.clone(), hbody));
        }
        let else_chunk = if orelse.is_empty() {
            None
        } else {
            Some(self.compile_block_chunk(orelse)?)
        };
        let final_chunk = if finalbody.is_empty() {
            None
        } else {
            Some(self.compile_block_chunk(finalbody)?)
        };
        let id = self.tries.len();
        self.tries.push(TryDef {
            body: body_chunk,
            handlers: hs,
            orelse: else_chunk,
            finalbody: final_chunk,
        });
        b.emit(Op::LoadInt(id as i64), 0);
        b.emit(Op::CallBuiltin(ops::TRY, 1), 0);
        b.emit(Op::Pop, 0);
        Ok(())
    }

    /// Compile a suite into a standalone chunk that runs in the current scope.
    fn compile_block_chunk(&mut self, stmts: &[Stmt]) -> Result<Chunk, String> {
        let mut cb = ChunkBuilder::new();
        self.begin_chunk();
        self.compile_stmts(&mut cb, stmts)?;
        Ok(self.finish_chunk(cb))
    }

    /// Compile a single expression into a chunk leaving its value on the stack.
    fn compile_expr_chunk(&mut self, e: &Expr) -> Result<Chunk, String> {
        let mut cb = ChunkBuilder::new();
        self.begin_chunk();
        self.compile_expr(&mut cb, e)?;
        Ok(self.finish_chunk(cb))
    }

    fn compile_with(
        &mut self,
        b: &mut ChunkBuilder,
        items: &[WithItem],
        body: &[Stmt],
        is_async: bool,
    ) -> Result<(), String> {
        // `with A, B: BODY` nests each manager independently (B inside A), so an
        // inner manager suppressing an exception hides it from the outer one —
        // exactly CPython's semantics. Fold from the innermost item outward.
        let mut cur: Vec<Stmt> = body.to_vec();
        for it in items.iter().rev() {
            cur = self.desugar_with_single(it, cur, is_async);
        }
        self.compile_stmts(b, &cur)
    }

    /// Desugar a single `with item as VAR: body` to the CPython protocol:
    /// ```text
    /// .ctx = <context expr>            # evaluated EXACTLY once
    /// VAR  = .ctx.__enter__()
    /// .hit = False
    /// try:
    ///     body
    /// except BaseException as .exc:
    ///     .hit = True
    ///     if not .ctx.__exit__(type(.exc), .exc, None):   # real 3-tuple
    ///         raise                                        # truthy → suppressed
    /// finally:
    ///     if not .hit:
    ///         .ctx.__exit__(None, None, None)              # normal exit, once
    /// ```
    /// `async with` drives `__aenter__`/`__aexit__` through `await`. The traceback
    /// slot is `None` (pythonrs has no traceback objects); the type and value are
    /// the real exception's.
    fn desugar_with_single(&mut self, it: &WithItem, body: Vec<Stmt>, is_async: bool) -> Vec<Stmt> {
        let (enter_m, exit_m) = if is_async {
            ("__aenter__", "__aexit__")
        } else {
            ("__enter__", "__exit__")
        };
        let awaited = |e: Expr| -> Expr {
            if is_async {
                Expr::Await(Box::new(e))
            } else {
                e
            }
        };
        let ctx = format!(".with{}", self.tmp);
        self.tmp += 1;
        let hit = format!(".withhit{}", self.tmp);
        self.tmp += 1;
        let exc = format!(".withexc{}", self.tmp);
        self.tmp += 1;

        let call = |recv: &str, method: &str, args: Vec<Expr>| -> Expr {
            Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(Expr::Name(recv.into())),
                    method.into(),
                )),
                args,
                keywords: vec![],
            }
        };

        let mut out: Vec<Stmt> = Vec::new();
        // .ctx = <context expr>   (once)
        out.push(
            StmtKind::Assign {
                targets: vec![Expr::Name(ctx.clone())],
                value: it.context.clone(),
            }
            .into(),
        );
        // VAR = [await] .ctx.__enter__()   (or eval for effect if no `as`)
        let enter = awaited(call(&ctx, enter_m, vec![]));
        match &it.vars {
            Some(v) => out.push(
                StmtKind::Assign {
                    targets: vec![v.clone()],
                    value: enter,
                }
                .into(),
            ),
            None => out.push(StmtKind::Expr(enter).into()),
        }
        // .hit = False
        out.push(
            StmtKind::Assign {
                targets: vec![Expr::Name(hit.clone())],
                value: Expr::False,
            }
            .into(),
        );
        // except handler body: .hit = True; if not .ctx.__exit__(type(.exc),.exc,None): raise
        let exit_exc = awaited(call(
            &ctx,
            exit_m,
            vec![
                Expr::Call {
                    func: Box::new(Expr::Name("type".into())),
                    args: vec![Expr::Name(exc.clone())],
                    keywords: vec![],
                },
                Expr::Name(exc.clone()),
                Expr::None,
            ],
        ));
        let handler_body: Vec<Stmt> = vec![
            StmtKind::Assign {
                targets: vec![Expr::Name(hit.clone())],
                value: Expr::True,
            }
            .into(),
            StmtKind::If {
                test: Expr::UnaryOp(UnOp::Not, Box::new(exit_exc)),
                body: vec![StmtKind::Raise {
                    exc: None,
                    cause: None,
                }
                .into()],
                orelse: vec![],
            }
            .into(),
        ];
        // finally: if not .hit: .ctx.__exit__(None, None, None)
        let exit_none = awaited(call(&ctx, exit_m, vec![Expr::None, Expr::None, Expr::None]));
        let finalbody: Vec<Stmt> = vec![StmtKind::If {
            test: Expr::UnaryOp(UnOp::Not, Box::new(Expr::Name(hit.clone()))),
            body: vec![StmtKind::Expr(exit_none).into()],
            orelse: vec![],
        }
        .into()];
        out.push(
            StmtKind::Try {
                body,
                handlers: vec![ExceptHandler {
                    typ: Some(Expr::Name("BaseException".into())),
                    name: Some(exc),
                    body: handler_body,
                    star: false,
                }],
                orelse: vec![],
                finalbody,
            }
            .into(),
        );
        out
    }

    // ── expressions ──────────────────────────────────────────────────────
    fn compile_expr(&mut self, b: &mut ChunkBuilder, e: &Expr) -> Result<(), String> {
        // Peel the parser's span wrapper: set it as the active `node_span` while
        // lowering the inner node, so the raising op (name load / binop / subscript
        // / call / attribute / unary) records it. Children save/restore around
        // themselves, so when this node emits its op, `node_span` still holds this
        // node's span. The suppress hint (assign/return direct call value) is
        // consumed by the outermost peel only.
        if let Expr::Spanned(inner, sp) = e {
            let mut sp = *sp;
            if std::mem::take(&mut self.suppress_hint)
                && matches!(inner.unspanned(), Expr::Call { .. })
            {
                sp.suppress = true;
            }
            let prev = std::mem::replace(&mut self.node_span, sp);
            let r = self.compile_expr(b, inner);
            self.node_span = prev;
            return r;
        }
        match e {
            Expr::Spanned(_, _) => unreachable!("peeled above"),
            Expr::None => {
                b.emit(Op::LoadUndef, 0);
            }
            Expr::True => {
                b.emit(Op::LoadTrue, 0);
            }
            Expr::False => {
                b.emit(Op::LoadFalse, 0);
            }
            Expr::Int(n) => {
                b.emit(Op::LoadInt(*n), 0);
            }
            Expr::Float(f) => {
                b.emit(Op::LoadFloat(*f), 0);
            }
            Expr::BigInt(s) => {
                // int("<digits>") — the builtin promotes past i64 into a BigInt.
                self.name_const(b, "int");
                self.strlit(b, s);
                b.emit(Op::CallBuiltin(ops::CALL, 2), 0);
            }
            Expr::Complex(f) => {
                // complex(0.0, imag)
                self.name_const(b, "complex");
                b.emit(Op::LoadFloat(0.0), 0);
                b.emit(Op::LoadFloat(*f), 0);
                b.emit(Op::CallBuiltin(ops::CALL, 3), 0);
            }
            Expr::Ellipsis => {
                b.emit(Op::CallBuiltin(ops::ELLIPSIS, 0), 0);
            }
            Expr::Str(s) => self.strlit(b, s),
            Expr::Bytes(bytes) => {
                // Pack the bytes into a latin-1 string constant (one code point
                // per byte); `MKBYTES` unpacks it back to raw bytes at runtime.
                let packed: String = bytes.iter().map(|&byte| byte as char).collect();
                let k = b.add_constant(Value::str(&packed));
                b.emit(Op::LoadConst(k), 0);
                b.emit(Op::CallBuiltin(ops::MKBYTES, 1), 0);
            }
            Expr::FString(parts) => self.compile_fstring(b, parts)?,
            Expr::Name(n) => {
                if let Some(slot) = self.native_slots.as_ref().and_then(|m| m.get(n)) {
                    b.emit(Op::GetSlot(*slot), self.cur_line);
                } else {
                    self.name_const(b, n);
                    let idx = b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), self.cur_line);
                    self.record_span(idx);
                }
            }
            Expr::List(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    // BUILD_ARGS already yields a flat `list`.
                    self.compile_arg_spread(b, items)?;
                } else {
                    self.build_chunked(
                        b,
                        ops::MKLIST,
                        ops::EXTEND_LIST,
                        items.len(),
                        1,
                        |c, b, i| c.compile_expr(b, &items[i]),
                    )?;
                }
            }
            Expr::Tuple(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    // Flatten to a list, then convert to a tuple.
                    self.name_const(b, "tuple");
                    self.compile_arg_spread(b, items)?;
                    b.emit(Op::CallBuiltin(ops::CALL, 2), 0);
                } else {
                    self.build_chunked(
                        b,
                        ops::MKTUPLE,
                        ops::EXTEND_TUPLE,
                        items.len(),
                        1,
                        |c, b, i| c.compile_expr(b, &items[i]),
                    )?;
                }
            }
            Expr::Set(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    self.name_const(b, "set");
                    self.compile_arg_spread(b, items)?;
                    b.emit(Op::CallBuiltin(ops::CALL, 2), 0);
                } else {
                    self.build_chunked(
                        b,
                        ops::MKSET,
                        ops::EXTEND_SET,
                        items.len(),
                        1,
                        |c, b, i| c.compile_expr(b, &items[i]),
                    )?;
                }
            }
            Expr::Dict(pairs) => {
                if pairs.iter().any(|(k, _)| k.is_none()) {
                    // `{**a, "k": v, **b}` — each entry is (tag, a, b): tag 1 =
                    // `**` spread of `a` (b unused), tag 0 = plain (key a, val b).
                    for (k, v) in pairs {
                        match k {
                            Some(k) => {
                                b.emit(Op::LoadInt(0), 0);
                                self.compile_expr(b, k)?;
                                self.compile_expr(b, v)?;
                            }
                            None => {
                                b.emit(Op::LoadInt(1), 0);
                                self.compile_expr(b, v)?;
                                b.emit(Op::LoadUndef, 0);
                            }
                        }
                    }
                    b.emit(Op::CallBuiltin(ops::MKDICT_EX, argc(pairs.len() * 3)?), 0);
                } else {
                    // This branch is reached only when every key is `Some`
                    // (the `**`-spread case took the arm above), so each pair
                    // contributes exactly two stack slots.
                    self.build_chunked(
                        b,
                        ops::MKDICT,
                        ops::EXTEND_DICT,
                        pairs.len(),
                        2,
                        |c, b, i| {
                            let (k, v) = &pairs[i];
                            c.compile_expr(b, k.as_ref().expect("plain dict key"))?;
                            c.compile_expr(b, v)
                        },
                    )?;
                }
            }
            Expr::Starred(inner) => self.compile_expr(b, inner)?,
            Expr::BoolOp(op, values) => self.compile_boolop(b, *op, values)?,
            Expr::UnaryOp(op, e) => self.compile_unaryop(b, *op, e)?,
            Expr::BinOp(op, l, r) => self.compile_binop(b, *op, l, r)?,
            Expr::Compare(left, ops) => self.compile_compare(b, left, ops)?,
            Expr::IfExp { test, body, orelse } => {
                self.compile_condition(b, test)?;
                let jf = b.emit(Op::JumpIfFalse(0), 0);
                self.compile_expr(b, body)?;
                let je = b.emit(Op::Jump(0), 0);
                let els = b.current_pos();
                b.patch_jump(jf, els);
                self.compile_expr(b, orelse)?;
                let end = b.current_pos();
                b.patch_jump(je, end);
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => self.compile_call(b, func, args, keywords)?,
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                let idx = b.emit(Op::CallBuiltin(ops::GETATTR, 2), self.cur_line);
                self.record_span(idx);
            }
            Expr::Subscript(recv, idx) => {
                // CPython `SyntaxWarning` for a literal sequence subscripted by a
                // float/complex constant (a likely missing comma).
                if let (Some(ct), Some(it)) = (literal_container_type(recv), float_index_type(idx))
                {
                    self.warnings.push((
                        self.cur_line,
                        format!(
                            "{ct} indices must be integers or slices, not {it}; \
                             perhaps you missed a comma?"
                        ),
                    ));
                }
                self.compile_expr(b, recv)?;
                self.compile_subscript_index(b, idx)?;
                let op_idx = b.emit(Op::CallBuiltin(ops::GETITEM, 2), self.cur_line);
                self.record_span(op_idx);
            }
            Expr::Slice { lo, hi, step } => {
                self.compile_opt(b, lo)?;
                self.compile_opt(b, hi)?;
                self.compile_opt(b, step)?;
                b.emit(Op::CallBuiltin(ops::MKSLICE, 3), 0);
            }
            Expr::Lambda { params, body } => {
                let bodystmt = vec![Stmt::from(StmtKind::Return(Some((**body).clone())))];
                self.fn_depth += 1;
                let def_id = self.build_function("<lambda>", params, &bodystmt);
                self.fn_depth -= 1;
                let def_id = def_id?;
                self.emit_make_func(b, def_id, params)?;
            }
            Expr::ListComp(elt, comps) => {
                self.compile_comprehension(b, CompKind::List, elt, None, comps)?
            }
            Expr::SetComp(elt, comps) => {
                self.compile_comprehension(b, CompKind::Set, elt, None, comps)?
            }
            Expr::GenExp(elt, comps) => self.compile_genexp(b, elt, comps)?,
            Expr::DictComp(k, v, comps) => {
                self.compile_comprehension(b, CompKind::Dict, k, Some(v), comps)?
            }
            Expr::NamedExpr(target, value) => {
                self.compile_expr(b, value)?;
                b.emit(Op::Dup, 0);
                self.compile_assign(b, target)?;
            }
            Expr::Yield(val) => {
                match val {
                    Some(e) => self.compile_expr(b, e)?,
                    None => {
                        b.emit(Op::LoadUndef, 0);
                    }
                }
                // YIELDV suspends and leaves the value sent by `.send()`/`next`
                // on the stack (None for plain iteration).
                b.emit(Op::CallBuiltin(ops::YIELDV, 1), 0);
            }
            Expr::YieldFrom(inner) => {
                // `yield from E` — iterate E, yielding each item. The delegating
                // expression value (the sub-generator's return) is None here.
                self.compile_yield_from(b, inner)?;
            }
            Expr::Await(inner) => {
                // `await E` — evaluate the awaitable, then drive it: the AWAIT op
                // suspends the running coroutine (yielding up to the event loop)
                // until the awaitable settles, then leaves its result.
                self.compile_expr(b, inner)?;
                b.emit(Op::CallBuiltin(ops::AWAIT, 1), 0);
            }
        }
        Ok(())
    }

    /// `yield from iterable` — full PEP 380 delegation. The single `YIELD_FROM`
    /// op drives the sub-iterator in the host: it re-yields each value, forwards
    /// `.send()` values / `.throw()` exceptions / `.close()` into the
    /// sub-iterator, and leaves the sub-iterator's return value
    /// (its `StopIteration.value`) on the stack.
    fn compile_yield_from(&mut self, b: &mut ChunkBuilder, iter: &Expr) -> Result<(), String> {
        self.compile_expr(b, iter)?;
        b.emit(Op::CallBuiltin(ops::YIELD_FROM, 1), 0); // [iterable] -> retval
        Ok(())
    }

    fn compile_opt(&mut self, b: &mut ChunkBuilder, e: &Option<Box<Expr>>) -> Result<(), String> {
        match e {
            Some(e) => self.compile_expr(b, e)?,
            None => {
                b.emit(Op::LoadUndef, 0);
            }
        }
        Ok(())
    }

    fn compile_seq(&mut self, b: &mut ChunkBuilder, items: &[Expr]) -> Result<(), String> {
        for it in items {
            self.compile_expr(b, it)?;
        }
        Ok(())
    }

    /// Emit a collection literal whose element count may exceed the u8 `argc`
    /// cap of `CallBuiltin`. `slots` is the stack slots per element (2 for dict
    /// k,v pairs, else 1). When the whole literal fits one chunk it lowers to a
    /// single `mk`; otherwise the first chunk builds the accumulator with `mk`
    /// and each further chunk folds in via `extend` ([acc, items...]) — the same
    /// shape CPython uses for oversized literals (LIST_EXTEND/DICT_UPDATE/…).
    /// `emit(self, b, i)` pushes element `i`'s `slots` values.
    fn build_chunked(
        &mut self,
        b: &mut ChunkBuilder,
        mk: u16,
        extend: u16,
        count: usize,
        slots: usize,
        mut emit: impl FnMut(&mut Self, &mut ChunkBuilder, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        // Whole elements per chunk: the `mk` chunk fills all 255 slots; an
        // `extend` chunk reserves 1 slot for the accumulator beneath it.
        let mk_cap = 255 / slots;
        let ext_cap = (255 - 1) / slots;
        if count <= mk_cap {
            for i in 0..count {
                emit(self, b, i)?;
            }
            b.emit(Op::CallBuiltin(mk, (count * slots) as u8), 0);
            return Ok(());
        }
        for i in 0..mk_cap {
            emit(self, b, i)?;
        }
        b.emit(Op::CallBuiltin(mk, (mk_cap * slots) as u8), 0);
        let mut i = mk_cap;
        while i < count {
            let n = ext_cap.min(count - i);
            for j in 0..n {
                emit(self, b, i + j)?;
            }
            b.emit(Op::CallBuiltin(extend, (1 + n * slots) as u8), 0);
            i += n;
        }
        Ok(())
    }

    fn compile_subscript_index(&mut self, b: &mut ChunkBuilder, idx: &Expr) -> Result<(), String> {
        self.compile_expr(b, idx)
    }

    fn compile_fstring(&mut self, b: &mut ChunkBuilder, parts: &[FStrPart]) -> Result<(), String> {
        // Each part pushes exactly one string slot (a literal constant, or a
        // FORMAT result). `build_chunked` keeps a >255-part f-string within the
        // u8 argc cap by folding extra parts in with EXTEND_STR.
        self.build_chunked(b, ops::MKSTR, ops::EXTEND_STR, parts.len(), 1, |c, b, i| {
            match &parts[i] {
                FStrPart::Lit(s) => {
                    let k = b.add_constant(Value::str(s));
                    b.emit(Op::LoadConst(k), 0);
                }
                FStrPart::Expr { expr, conv, spec } => {
                    c.compile_expr(b, expr)?;
                    let conv_i = match conv {
                        Some('s') => 1,
                        Some('r') => 2,
                        Some('a') => 3,
                        _ => 0,
                    };
                    b.emit(Op::LoadInt(conv_i), 0);
                    c.compile_fstring_spec(b, spec)?;
                    b.emit(Op::CallBuiltin(ops::FORMAT, 3), 0);
                }
            }
            Ok(())
        })
    }

    /// Push an f-string field's format spec onto the stack as a string. The
    /// common case (no spec, or a purely-literal spec) folds to a single
    /// constant; a spec carrying a nested replacement field (`{w}` in
    /// `{x:{w}.2f}`) is emitted as its own joined-string, evaluated at runtime.
    fn compile_fstring_spec(
        &mut self,
        b: &mut ChunkBuilder,
        spec: &[FStrPart],
    ) -> Result<(), String> {
        if spec.iter().any(|p| matches!(p, FStrPart::Expr { .. })) {
            return self.compile_fstring(b, spec);
        }
        let mut s = String::new();
        for p in spec {
            if let FStrPart::Lit(l) = p {
                s.push_str(l);
            }
        }
        let k = b.add_constant(Value::str(s));
        b.emit(Op::LoadConst(k), 0);
        Ok(())
    }

    fn compile_boolop(
        &mut self,
        b: &mut ChunkBuilder,
        op: BoolOp,
        values: &[Expr],
    ) -> Result<(), String> {
        // Left-assoc fold with short-circuit; result is the deciding operand's
        // value (Python semantics), not a coerced bool.
        self.compile_expr(b, &values[0])?;
        let mut ends = Vec::new();
        for v in &values[1..] {
            b.emit(Op::Dup, 0);
            b.emit(Op::CallBuiltin(ops::TRUTHY, 1), 0);
            let jump = match op {
                BoolOp::And => b.emit(Op::JumpIfFalse(0), 0),
                BoolOp::Or => b.emit(Op::JumpIfTrue(0), 0),
            };
            ends.push(jump);
            b.emit(Op::Pop, 0);
            self.compile_expr(b, v)?;
        }
        let end = b.current_pos();
        for j in ends {
            b.patch_jump(j, end);
        }
        Ok(())
    }

    fn compile_unaryop(&mut self, b: &mut ChunkBuilder, op: UnOp, e: &Expr) -> Result<(), String> {
        match op {
            UnOp::Neg => {
                self.compile_expr(b, e)?;
                let idx = b.emit(Op::Negate, 0);
                self.record_span(idx);
            }
            UnOp::Not => {
                self.compile_condition(b, e)?;
                b.emit(Op::LogNot, 0);
            }
            UnOp::Invert => {
                b.emit(Op::LoadInt(unop::INVERT), 0);
                self.compile_expr(b, e)?;
                let idx = b.emit(Op::CallBuiltin(ops::UNARY, 2), 0);
                self.record_span(idx);
            }
            UnOp::Pos => {
                b.emit(Op::LoadInt(unop::POS), 0);
                self.compile_expr(b, e)?;
                let idx = b.emit(Op::CallBuiltin(ops::UNARY, 2), 0);
                self.record_span(idx);
            }
        }
        Ok(())
    }

    fn compile_binop(
        &mut self,
        b: &mut ChunkBuilder,
        op: BinOp,
        l: &Expr,
        r: &Expr,
    ) -> Result<(), String> {
        // Native fast path for + - * (JIT-traceable); everything else dispatches.
        let tag = match op {
            BinOp::Add => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                let idx = b.emit(Op::Add, self.cur_line);
                self.record_span(idx);
                return Ok(());
            }
            BinOp::Sub => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                let idx = b.emit(Op::Sub, self.cur_line);
                self.record_span(idx);
                return Ok(());
            }
            BinOp::Mul => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                let idx = b.emit(Op::Mul, self.cur_line);
                self.record_span(idx);
                return Ok(());
            }
            BinOp::Div => bop::DIV,
            BinOp::FloorDiv => bop::FLOORDIV,
            BinOp::Mod => bop::MOD,
            BinOp::Pow => bop::POW,
            BinOp::MatMul => bop::MATMUL,
            BinOp::BitAnd => bop::BITAND,
            BinOp::BitOr => bop::BITOR,
            BinOp::BitXor => bop::BITXOR,
            BinOp::Shl => bop::SHL,
            BinOp::Shr => bop::SHR,
        };
        b.emit(Op::LoadInt(tag), 0);
        self.compile_expr(b, l)?;
        self.compile_expr(b, r)?;
        let idx = b.emit(Op::CallBuiltin(ops::BINOP, 3), self.cur_line);
        self.record_span(idx);
        Ok(())
    }

    fn compile_compare(
        &mut self,
        b: &mut ChunkBuilder,
        left: &Expr,
        ops_list: &[(CmpOp, Expr)],
    ) -> Result<(), String> {
        if ops_list.len() == 1 {
            self.emit_single_compare(b, left, ops_list[0].0, &ops_list[0].1)?;
            return Ok(());
        }
        // Chained: `a<b<c` -> `(a<b) and (b<c)`, but each INTERIOR operand is
        // evaluated EXACTLY ONCE (CPython semantics). Bind each interior operand
        // to a synthetic temp with a walrus (`:=`) inside the link that first
        // reads it; the next link reads the temp. Because the conjunction folds
        // through `compile_boolop`'s short-circuit, a later operand's walrus is
        // never reached once an earlier link is False — so short-circuit
        // evaluation is preserved (the operand is not evaluated at all).
        let n = ops_list.len();
        let mut conj: Vec<Expr> = Vec::new();
        let mut prev = left.clone();
        for (i, (op, rhs)) in ops_list.iter().enumerate() {
            if i + 1 < n {
                let t = format!(".cmp{}", self.tmp);
                self.tmp += 1;
                // `prev op (t := rhs)`
                let walrus =
                    Expr::NamedExpr(Box::new(Expr::Name(t.clone())), Box::new(rhs.clone()));
                conj.push(Expr::Compare(Box::new(prev), vec![(*op, walrus)]));
                prev = Expr::Name(t);
            } else {
                conj.push(Expr::Compare(Box::new(prev), vec![(*op, rhs.clone())]));
                prev = rhs.clone();
            }
        }
        let _ = prev;
        self.compile_boolop(b, BoolOp::And, &conj)
    }

    fn emit_single_compare(
        &mut self,
        b: &mut ChunkBuilder,
        left: &Expr,
        op: CmpOp,
        rhs: &Expr,
    ) -> Result<(), String> {
        match op {
            CmpOp::Eq => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumEq, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Ne => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumNe, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Lt => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumLt, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Le => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumLe, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Gt => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumGt, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Ge => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                let idx = b.emit(Op::NumGe, self.cur_line);
                self.record_span(idx);
            }
            CmpOp::Is => {
                if let Some(t) = const_literal_type(left).or_else(|| const_literal_type(rhs)) {
                    self.warnings.push((
                        self.cur_line,
                        format!("\"is\" with '{t}' literal. Did you mean \"==\"?"),
                    ));
                }
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::CallBuiltin(ops::IS, 2), 0);
            }
            CmpOp::IsNot => {
                if let Some(t) = const_literal_type(left).or_else(|| const_literal_type(rhs)) {
                    self.warnings.push((
                        self.cur_line,
                        format!("\"is not\" with '{t}' literal. Did you mean \"!=\"?"),
                    ));
                }
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::CallBuiltin(ops::IS, 2), 0);
                b.emit(Op::LogNot, 0);
            }
            CmpOp::In => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::CallBuiltin(ops::CONTAINS, 2), 0);
            }
            CmpOp::NotIn => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::CallBuiltin(ops::CONTAINS, 2), 0);
                b.emit(Op::LogNot, 0);
            }
        }
        Ok(())
    }

    fn compile_call(
        &mut self,
        b: &mut ChunkBuilder,
        func: &Expr,
        args: &[Expr],
        keywords: &[Keyword],
    ) -> Result<(), String> {
        let has_star = args.iter().any(|a| matches!(a, Expr::Starred(_)));
        let has_kwsplat = keywords.iter().any(|k| k.name.is_none());
        if has_star || has_kwsplat {
            return self.compile_call_ex(b, func, args, keywords);
        }
        let named: Vec<&Keyword> = keywords.iter().collect();
        let build_kw = |c: &mut Self, b: &mut ChunkBuilder| -> Result<(), String> {
            for kw in &named {
                let n = kw.name.as_ref().unwrap();
                c.strlit(b, n);
                c.compile_expr(b, &kw.value)?;
            }
            b.emit(Op::CallBuiltin(ops::MKDICT, argc(named.len() * 2)?), 0);
            Ok(())
        };
        // The call's own span (set by the enclosing `Spanned` peel) is recorded
        // against whichever CALL op is emitted, so a call that raises underlines
        // `func(...)` with the CPython `~~~^^^` bracket anchor. `func` is peeled
        // so the parser's span wrapper does not defeat the method-call fast path.
        let span_idx;
        match func.unspanned() {
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL_METHOD, argc(2 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL_METHOD_KW, argc(3 + args.len())?),
                        self.cur_line,
                    );
                }
            }
            Expr::Name(n) => {
                self.name_const(b, n);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL, argc(1 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL_KW, argc(2 + args.len())?),
                        self.cur_line,
                    );
                }
            }
            _ => {
                self.compile_expr(b, func)?;
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL_VALUE, argc(1 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    span_idx = b.emit(
                        Op::CallBuiltin(ops::CALL_VALUE_KW, argc(2 + args.len())?),
                        self.cur_line,
                    );
                }
            }
        }
        self.record_span(span_idx);
        Ok(())
    }

    /// Emit the positional-args list for a call/literal that contains `*spread`
    /// elements: each element is `(tag, value)` where tag 1 means spread. The
    /// `BUILD_ARGS` handler flattens spreads and returns a `list`.
    fn compile_arg_spread(&mut self, b: &mut ChunkBuilder, items: &[Expr]) -> Result<(), String> {
        for it in items {
            match it {
                Expr::Starred(inner) => {
                    b.emit(Op::LoadInt(1), 0);
                    self.compile_expr(b, inner)?;
                }
                _ => {
                    b.emit(Op::LoadInt(0), 0);
                    self.compile_expr(b, it)?;
                }
            }
        }
        b.emit(Op::CallBuiltin(ops::BUILD_ARGS, argc(items.len() * 2)?), 0);
        Ok(())
    }

    /// Emit the kwargs dict for a call with `name=v` and `**mapping` keywords:
    /// each entry is `(key, value)` where a `None`-key (Undef) is a `**` spread.
    fn compile_kw_spread(&mut self, b: &mut ChunkBuilder, kws: &[Keyword]) -> Result<(), String> {
        for kw in kws {
            match &kw.name {
                Some(n) => self.strlit(b, n),
                None => {
                    b.emit(Op::LoadUndef, 0);
                }
            }
            self.compile_expr(b, &kw.value)?;
        }
        b.emit(Op::CallBuiltin(ops::BUILD_KWARGS, argc(kws.len() * 2)?), 0);
        Ok(())
    }

    /// Lower a call with `*args`/`**kwargs` unpacking through the EX ops.
    fn compile_call_ex(
        &mut self,
        b: &mut ChunkBuilder,
        func: &Expr,
        args: &[Expr],
        keywords: &[Keyword],
    ) -> Result<(), String> {
        match func.unspanned() {
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                self.compile_arg_spread(b, args)?;
                self.compile_kw_spread(b, keywords)?;
                b.emit(Op::CallBuiltin(ops::CALL_METHOD_EX, 4), self.cur_line);
            }
            Expr::Name(n) => {
                self.name_const(b, n);
                self.compile_arg_spread(b, args)?;
                self.compile_kw_spread(b, keywords)?;
                b.emit(Op::CallBuiltin(ops::CALL_EX, 3), self.cur_line);
            }
            _ => {
                self.compile_expr(b, func)?;
                self.compile_arg_spread(b, args)?;
                self.compile_kw_spread(b, keywords)?;
                b.emit(Op::CallBuiltin(ops::CALL_VALUE_EX, 3), self.cur_line);
            }
        }
        Ok(())
    }

    // ── comprehensions ───────────────────────────────────────────────────
    //
    // Python 3 runs a comprehension in its OWN function scope: the loop variable
    // never leaks to the enclosing scope, and enclosing variables are read via
    // the closure. So a comprehension lowers to a hidden nullary-ish function
    // whose single parameter `.0` is the outermost iterable (evaluated in the
    // enclosing scope, matching CPython), and whose body builds and returns the
    // container. This gives own-scope and no-leak for free.
    fn compile_comprehension(
        &mut self,
        b: &mut ChunkBuilder,
        kind: CompKind,
        elt: &Expr,
        val: Option<&Expr>,
        comps: &[Comprehension],
    ) -> Result<(), String> {
        let acc = ".result";
        // Accumulator init + append/add/insert element.
        let empty = match kind {
            CompKind::List => Expr::List(vec![]),
            CompKind::Set => Expr::Set(vec![]),
            CompKind::Dict => Expr::Dict(vec![]),
        };
        let add_stmt: Stmt = match kind {
            CompKind::List => StmtKind::Expr(Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(Expr::Name(acc.into())),
                    "append".into(),
                )),
                args: vec![elt.clone()],
                keywords: vec![],
            })
            .into(),
            CompKind::Set => StmtKind::Expr(Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(Expr::Name(acc.into())),
                    "add".into(),
                )),
                args: vec![elt.clone()],
                keywords: vec![],
            })
            .into(),
            CompKind::Dict => StmtKind::Assign {
                targets: vec![Expr::Subscript(
                    Box::new(Expr::Name(acc.into())),
                    Box::new(elt.clone()),
                )],
                value: val.unwrap().clone(),
            }
            .into(),
        };
        let mut inner = vec![add_stmt];
        inner = wrap_comp_clauses(inner, comps);
        let mut body = vec![StmtKind::Assign {
            targets: vec![Expr::Name(acc.into())],
            value: empty,
        }
        .into()];
        // A walrus (`:=`) inside a comprehension binds in the nearest enclosing
        // non-comprehension scope, not the hidden comp function (PEP 572). Inject
        // a `global`/`nonlocal` for each such target so the assignment leaks out.
        for decl in self.comp_walrus_decls(elt, val, comps) {
            body.insert(0, decl);
        }
        body.extend(inner);
        body.push(StmtKind::Return(Some(Expr::Name(acc.into()))).into());
        // An asynchronous comprehension (`[x async for x in ag()]` / an `await` in
        // any clause) runs the hidden function as a coroutine that the enclosing
        // (necessarily async) scope awaits.
        let is_async = comps.iter().any(|c| c.is_async);
        self.emit_comp_call(b, "<comp>", body, &comps[0].iter, is_async)
    }

    /// Build the `global`/`nonlocal` declarations for every walrus (`:=`) target
    /// appearing in a comprehension's element, value, and `if` clauses (but NOT
    /// its iterables, which run in the enclosing scope already). At module level
    /// (`fn_depth == 0`) the target is a `global`; inside a function it is a
    /// `nonlocal`, so the binding lands in the enclosing scope, not the hidden
    /// comprehension function.
    fn comp_walrus_decls(
        &self,
        elt: &Expr,
        val: Option<&Expr>,
        comps: &[Comprehension],
    ) -> Vec<Stmt> {
        let mut names: Vec<String> = Vec::new();
        collect_walrus_targets(elt, &mut names);
        if let Some(v) = val {
            collect_walrus_targets(v, &mut names);
        }
        for c in comps {
            for cond in &c.ifs {
                collect_walrus_targets(cond, &mut names);
            }
        }
        names.dedup();
        if names.is_empty() {
            return vec![];
        }
        let kind = if self.fn_depth == 0 {
            StmtKind::Global(names)
        } else {
            StmtKind::Nonlocal(names)
        };
        vec![kind.into()]
    }

    /// A generator expression `(elt for target in iter ...)` — lazy: a hidden
    /// generator function that yields each element.
    fn compile_genexp(
        &mut self,
        b: &mut ChunkBuilder,
        elt: &Expr,
        comps: &[Comprehension],
    ) -> Result<(), String> {
        let yield_stmt: Stmt = StmtKind::Expr(Expr::Yield(Some(Box::new(elt.clone())))).into();
        let body = wrap_comp_clauses(vec![yield_stmt], comps);
        self.emit_comp_call(b, "<genexpr>", body, &comps[0].iter, false)
    }

    /// Build the hidden comprehension/genexpr function `def name(.0): body` and
    /// emit code to call it with the outermost iterable — the shared tail of
    /// both comprehension and genexpr lowering.
    fn emit_comp_call(
        &mut self,
        b: &mut ChunkBuilder,
        name: &str,
        body: Vec<Stmt>,
        outer_iter: &Expr,
        is_async: bool,
    ) -> Result<(), String> {
        let params = Params {
            names: vec![".0".into()],
            ..Params::default()
        };
        let def_id = self.build_function_ex(name, &params, &body, is_async, ScopeKind::Function)?;
        self.emit_make_func(b, def_id, &params)?; // [func]
        self.compile_expr(b, outer_iter)?; // [func, iterable]
        b.emit(Op::CallBuiltin(ops::CALL_VALUE, 2), 0); // [result|coroutine]
        if is_async {
            // The hidden coroutine is awaited in the enclosing async scope.
            b.emit(Op::CallBuiltin(ops::AWAIT, 1), 0);
        }
        Ok(())
    }

    // ── match / case (PEP 634 structural pattern matching) ────────────────
    fn compile_match(
        &mut self,
        b: &mut ChunkBuilder,
        subject: &Expr,
        cases: &[MatchCase],
    ) -> Result<(), String> {
        // Compile-time structural validation (PEP 634): reject before running,
        // matching CPython which raises SyntaxError for the whole module.
        for case in cases {
            let mut names = Vec::new();
            validate_pattern(&case.pattern, &mut names)?;
        }
        let subj = format!(".match{}", self.tmp);
        self.tmp += 1;
        self.compile_expr(b, subject)?;
        self.store_name(b, &subj);
        let mut end_jumps = Vec::new();
        for case in cases {
            let mut fails = Vec::new();
            // Load the subject fresh for this case's pattern test.
            self.load_local(b, &subj);
            self.compile_pattern(b, &case.pattern, &mut fails)?;
            if let Some(g) = &case.guard {
                self.compile_condition(b, g)?;
                let jf = b.emit(Op::JumpIfFalse(0), 0);
                fails.push(jf);
            }
            self.compile_stmts(b, &case.body)?;
            end_jumps.push(b.emit(Op::Jump(0), 0));
            let next = b.current_pos();
            for f in fails {
                b.patch_jump(f, next);
            }
        }
        let end = b.current_pos();
        for j in end_jumps {
            b.patch_jump(j, end);
        }
        Ok(())
    }

    fn load_local(&self, b: &mut ChunkBuilder, name: &str) {
        self.name_const(b, name);
        b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0);
    }

    /// Compile one pattern. The value to match is on TOP of the stack and is
    /// consumed whether the pattern matches (fall through) or fails (a jump is
    /// pushed onto `fails`). This invariant keeps the operand stack balanced at
    /// every fail label.
    fn compile_pattern(
        &mut self,
        b: &mut ChunkBuilder,
        pat: &Pattern,
        fails: &mut Vec<usize>,
    ) -> Result<(), String> {
        match pat {
            Pattern::Wildcard => {
                b.emit(Op::Pop, 0);
            }
            Pattern::Capture(name) => {
                self.compile_assign(b, &Expr::Name(name.clone()))?;
            }
            Pattern::Value(e) => {
                self.compile_expr(b, e)?; // [v, e]
                                          // Singleton patterns (None/True/False) match by identity (`is`),
                                          // every other literal/value pattern by equality (`==`) — PEP 634.
                if matches!(e, Expr::None | Expr::True | Expr::False) {
                    b.emit(Op::CallBuiltin(ops::IS, 2), 0); // [bool]
                } else {
                    b.emit(Op::NumEq, 0); // [bool]
                }
                let jf = b.emit(Op::JumpIfFalse(0), 0);
                fails.push(jf);
            }
            Pattern::As(inner, name) => {
                // Bind name to the whole value, then re-match inner against it.
                self.compile_assign(b, &Expr::Name(name.clone()))?;
                self.load_local(b, name);
                self.compile_pattern(b, inner, fails)?;
            }
            Pattern::Or(alts) => {
                let orv = format!(".or{}", self.tmp);
                self.tmp += 1;
                self.store_name(b, &orv);
                let mut succ = Vec::new();
                for alt in alts {
                    self.load_local(b, &orv);
                    let mut altfails = Vec::new();
                    self.compile_pattern(b, alt, &mut altfails)?;
                    succ.push(b.emit(Op::Jump(0), 0));
                    let here = b.current_pos();
                    for f in altfails {
                        b.patch_jump(f, here);
                    }
                }
                fails.push(b.emit(Op::Jump(0), 0));
                let after = b.current_pos();
                for j in succ {
                    b.patch_jump(j, after);
                }
            }
            Pattern::Star(_) => {
                // A bare star is only meaningful inside a sequence pattern, which
                // handles it directly; reaching here means a stray value.
                b.emit(Op::Pop, 0);
            }
            Pattern::Sequence { elems, star } => {
                self.compile_seq_pattern(b, elems, *star, fails)?;
            }
            Pattern::Mapping { keys, rest } => {
                self.compile_map_pattern(b, keys, rest, fails)?;
            }
            Pattern::Class { cls, pos, kw } => {
                self.compile_class_pattern(b, cls, pos, kw, fails)?;
            }
        }
        Ok(())
    }

    fn compile_seq_pattern(
        &mut self,
        b: &mut ChunkBuilder,
        elems: &[Pattern],
        star: Option<usize>,
        fails: &mut Vec<usize>,
    ) -> Result<(), String> {
        // [subject] on top.
        b.emit(Op::LoadInt(elems.len() as i64), 0);
        b.emit(Op::LoadInt(star.map(|i| i as i64).unwrap_or(-1)), 0);
        b.emit(Op::CallBuiltin(ops::MATCH_SEQ, 3), 0); // [list, bool] | [bool(false)]
        let jf = b.emit(Op::JumpIfFalse(0), 0);
        fails.push(jf);
        let seqv = format!(".seq{}", self.tmp);
        self.tmp += 1;
        self.store_name(b, &seqv); // consume the destructured list
        for (k, sub) in elems.iter().enumerate() {
            self.load_local(b, &seqv);
            b.emit(Op::LoadInt(k as i64), 0);
            b.emit(Op::CallBuiltin(ops::GETITEM, 2), 0); // [element]
            match sub {
                Pattern::Star(Some(name)) => {
                    self.compile_assign(b, &Expr::Name(name.clone()))?;
                }
                Pattern::Star(None) => {
                    b.emit(Op::Pop, 0);
                }
                _ => self.compile_pattern(b, sub, fails)?,
            }
        }
        Ok(())
    }

    fn compile_map_pattern(
        &mut self,
        b: &mut ChunkBuilder,
        keys: &[(Expr, Pattern)],
        rest: &Option<String>,
        fails: &mut Vec<usize>,
    ) -> Result<(), String> {
        // [subject] on top.
        let mapv = format!(".map{}", self.tmp);
        self.tmp += 1;
        self.store_name(b, &mapv);
        self.load_local(b, &mapv);
        b.emit(Op::CallBuiltin(ops::MATCH_MAP_CHECK, 1), 0); // [bool]
        let jf = b.emit(Op::JumpIfFalse(0), 0);
        fails.push(jf);
        for (keyexpr, sub) in keys {
            self.load_local(b, &mapv);
            self.compile_expr(b, keyexpr)?; // [subject, key]
            b.emit(Op::CallBuiltin(ops::MATCH_KEY, 2), 0); // [value, bool] | [bool(false)]
            let jf = b.emit(Op::JumpIfFalse(0), 0);
            fails.push(jf);
            self.compile_pattern(b, sub, fails)?;
        }
        if let Some(rname) = rest {
            self.load_local(b, &mapv);
            for (keyexpr, _) in keys {
                self.compile_expr(b, keyexpr)?;
            }
            b.emit(Op::CallBuiltin(ops::MKLIST, argc(keys.len())?), 0);
            b.emit(Op::CallBuiltin(ops::MATCH_MAP_REST, 2), 0); // [rest_dict]
            self.compile_assign(b, &Expr::Name(rname.clone()))?;
        }
        Ok(())
    }

    fn compile_class_pattern(
        &mut self,
        b: &mut ChunkBuilder,
        cls: &Expr,
        pos: &[Pattern],
        kw: &[(String, Pattern)],
        fails: &mut Vec<usize>,
    ) -> Result<(), String> {
        // [subject] on top.
        self.compile_expr(b, cls)?; // [subject, class]
        b.emit(Op::LoadInt(pos.len() as i64), 0);
        for (name, _) in kw {
            self.strlit(b, name);
        }
        b.emit(Op::CallBuiltin(ops::MATCH_CLASS, argc(3 + kw.len())?), 0); // [list, bool] | [bool]
        let jf = b.emit(Op::JumpIfFalse(0), 0);
        fails.push(jf);
        let clsv = format!(".cls{}", self.tmp);
        self.tmp += 1;
        self.store_name(b, &clsv);
        let subs: Vec<&Pattern> = pos.iter().chain(kw.iter().map(|(_, p)| p)).collect();
        for (k, sub) in subs.iter().enumerate() {
            self.load_local(b, &clsv);
            b.emit(Op::LoadInt(k as i64), 0);
            b.emit(Op::CallBuiltin(ops::GETITEM, 2), 0); // [element]
            self.compile_pattern(b, sub, fails)?;
        }
        Ok(())
    }
}

/// PEP 634 compile-time pattern checks (CPython raises these as `SyntaxError`
/// for the whole module, before any case runs). `bound` accumulates the capture
/// names bound so far in the current case pattern. Rejects: a name bound twice,
/// a duplicate mapping key, a repeated class-keyword attribute, and OR
/// alternatives that bind different name sets.
fn validate_pattern(pat: &Pattern, bound: &mut Vec<String>) -> Result<(), String> {
    match pat {
        Pattern::Wildcard | Pattern::Value(_) | Pattern::Star(None) => Ok(()),
        Pattern::Capture(name) | Pattern::Star(Some(name)) => bind_pattern_name(name, bound),
        Pattern::As(inner, name) => {
            validate_pattern(inner, bound)?;
            bind_pattern_name(name, bound)
        }
        Pattern::Sequence { elems, .. } => {
            for e in elems {
                validate_pattern(e, bound)?;
            }
            Ok(())
        }
        Pattern::Mapping { keys, rest } => {
            for i in 0..keys.len() {
                for j in 0..i {
                    if keys[i].0 == keys[j].0 {
                        return Err(format!(
                            "SyntaxError: mapping pattern checks duplicate key ({})",
                            pattern_key_repr(&keys[i].0)
                        ));
                    }
                }
            }
            for (_, p) in keys {
                validate_pattern(p, bound)?;
            }
            if let Some(r) = rest {
                bind_pattern_name(r, bound)?;
            }
            Ok(())
        }
        Pattern::Class { pos, kw, .. } => {
            for i in 0..kw.len() {
                for j in 0..i {
                    if kw[i].0 == kw[j].0 {
                        return Err(format!(
                            "SyntaxError: attribute name repeated in class pattern: {}",
                            kw[i].0
                        ));
                    }
                }
            }
            for p in pos {
                validate_pattern(p, bound)?;
            }
            for (_, p) in kw {
                validate_pattern(p, bound)?;
            }
            Ok(())
        }
        Pattern::Or(alts) => {
            // Each alternative must bind an identical set of names.
            let base = bound.len();
            let mut expected: Option<Vec<String>> = None;
            for alt in alts {
                let mut alt_bound = bound.clone();
                validate_pattern(alt, &mut alt_bound)?;
                let mut added: Vec<String> = alt_bound[base..].to_vec();
                added.sort();
                match &expected {
                    None => expected = Some(added),
                    Some(exp) if *exp != added => {
                        return Err("SyntaxError: alternative patterns bind different names".into())
                    }
                    _ => {}
                }
            }
            if let Some(exp) = expected {
                bound.extend(exp);
            }
            Ok(())
        }
    }
}

fn bind_pattern_name(name: &str, bound: &mut Vec<String>) -> Result<(), String> {
    if bound.iter().any(|n| n == name) {
        return Err(format!(
            "SyntaxError: multiple assignments to name '{name}' in pattern"
        ));
    }
    bound.push(name.to_string());
    Ok(())
}

/// CPython-style repr of a mapping-pattern key for the duplicate-key error.
fn pattern_key_repr(e: &Expr) -> String {
    let e = e.unspanned();
    match e {
        Expr::Str(s) => format!("'{s}'"),
        Expr::Int(n) => n.to_string(),
        Expr::BigInt(s) => s.clone(),
        Expr::Float(f) => f.to_string(),
        Expr::None => "None".into(),
        Expr::True => "True".into(),
        Expr::False => "False".into(),
        _ => "...".into(),
    }
}

/// Wrap `inner` statements in the comprehension's `for`/`if` clauses. The first
/// clause's iterable is replaced by the injected parameter `.0` (the outermost
/// iterable is evaluated in the enclosing scope and passed in).
fn wrap_comp_clauses(mut inner: Vec<Stmt>, comps: &[Comprehension]) -> Vec<Stmt> {
    for (i, comp) in comps.iter().enumerate().rev() {
        // Innermost-out: apply guards, then the `for`.
        for cond in comp.ifs.iter().rev() {
            inner = vec![StmtKind::If {
                test: cond.clone(),
                body: inner,
                orelse: vec![],
            }
            .into()];
        }
        let iter = if i == 0 {
            Expr::Name(".0".into())
        } else {
            (*comp.iter).clone()
        };
        inner = vec![StmtKind::For {
            target: (*comp.target).clone(),
            iter,
            body: inner,
            orelse: vec![],
            is_async: comp.is_async,
        }
        .into()];
    }
    inner
}

/// The parameter names a `def`/`lambda` binds (positional, `*args`, keyword-only,
/// `**kwargs`), skipping the bare-`*` keyword-only marker (`Some("")`).
/// The CPython type name of an immutable *constant* literal (`int`/`float`/
/// `complex`/`str`/`bytes`, or a `tuple` of such), used for the `SyntaxWarning`
/// on `x is <literal>`. `None` for names, containers CPython doesn't fold
/// (`list`/`dict`/`set`), and the singletons (`None`/`True`/`False`).
fn const_literal_type(e: &Expr) -> Option<&'static str> {
    let e = e.unspanned();
    match e {
        Expr::Int(_) | Expr::BigInt(_) => Some("int"),
        Expr::Float(_) => Some("float"),
        Expr::Complex(_) => Some("complex"),
        Expr::Str(_) => Some("str"),
        Expr::Bytes(_) => Some("bytes"),
        Expr::Tuple(items) if items.iter().all(|x| const_literal_type(x).is_some()) => {
            Some("tuple")
        }
        _ => None,
    }
}

/// The type name of a literal *sequence* container (`list`/`tuple`/`str`/
/// `bytes`) whose subscription CPython can type-check at compile time; `None`
/// otherwise (a `dict` allows float keys, a `name`/call has unknown type).
fn literal_container_type(e: &Expr) -> Option<&'static str> {
    let e = e.unspanned();
    match e {
        Expr::List(_) => Some("list"),
        Expr::Tuple(_) => Some("tuple"),
        Expr::Str(_) => Some("str"),
        Expr::Bytes(_) => Some("bytes"),
        _ => None,
    }
}

/// The type name of a `float`/`complex` literal index (which can't index a
/// sequence), else `None`.
fn float_index_type(e: &Expr) -> Option<&'static str> {
    let e = e.unspanned();
    match e {
        Expr::Float(_) => Some("float"),
        Expr::Complex(_) => Some("complex"),
        _ => None,
    }
}

/// A top-level statement of a class body that is a simple (bare-name)
/// annotation, so the body needs an `__annotations__` dict.
fn is_ann_assign(s: &Stmt) -> bool {
    matches!(
        &s.kind,
        StmtKind::AnnAssign { target, .. } if matches!(target.unspanned(), Expr::Name(_))
    )
}

fn param_names(params: &Params) -> Vec<String> {
    let mut v = params.names.clone();
    if let Some(s) = &params.star {
        if !s.is_empty() {
            v.push(s.clone());
        }
    }
    v.extend(params.kwonly.iter().cloned());
    if let Some(k) = &params.kwargs {
        v.push(k.clone());
    }
    v
}

/// The local-name set of a function/comprehension scope: every name *bound* in
/// the body (assignments, `for`/`with`/`except`/`match` targets, nested
/// `def`/`class` names, imports, and walrus `:=` targets that leak out of a
/// comprehension per PEP 572) MINUS the names the scope declares `global`/
/// `nonlocal`. Reading one before it is bound is an `UnboundLocalError`. The walk
/// stays at this scope level — it does not descend into a nested `def`/`class`/
/// lambda body, each of which owns its own locals. Sorted for a stable bytecode
/// cache key.
fn scope_locals(body: &[Stmt]) -> Vec<String> {
    let mut bound = HashSet::new();
    let mut gdecl = HashSet::new();
    let mut ndecl = HashSet::new();
    for s in body {
        collect_bound_stmt(s, &mut bound);
        collect_scope_decls_stmt(s, &mut gdecl, &mut ndecl);
    }
    let mut out: Vec<String> = bound
        .into_iter()
        .filter(|n| !gdecl.contains(n) && !ndecl.contains(n))
        .collect();
    out.sort();
    out
}

/// The names a function closes over: referenced anywhere in its body (including
/// nested functions) AND bound in an enclosing function scope, minus its own
/// params/locals and any it declares `global`. This is `co_freevars` /
/// `func.__closure__`. The `∩ enclosing` filter means the collector need not
/// track nested-scope bindings — a nested local is never in an enclosing scope.
fn scope_freevars(
    params: &Params,
    body: &[Stmt],
    enclosing: &[HashSet<String>],
) -> Vec<String> {
    let mut bound_here: HashSet<String> = scope_locals(body).into_iter().collect();
    for p in param_names(params) {
        bound_here.insert(p);
    }
    let mut gdecl = HashSet::new();
    let mut ndecl = HashSet::new();
    for s in body {
        collect_scope_decls_stmt(s, &mut gdecl, &mut ndecl);
    }
    let enclosing_all: HashSet<String> = enclosing.iter().flatten().cloned().collect();
    let mut refs: HashSet<String> = HashSet::new();
    for s in body {
        collect_names_stmt(s, &mut refs);
    }
    // A `nonlocal` declaration makes a name free even if the body never uses it.
    for n in &ndecl {
        refs.insert(n.clone());
    }
    let mut fv: Vec<String> = refs
        .into_iter()
        .filter(|n| enclosing_all.contains(n) && !bound_here.contains(n) && !gdecl.contains(n))
        .collect();
    fv.sort();
    fv
}

/// Collect every `Name` referenced in `e`, descending into all sub-expressions
/// and nested scopes. Over-collection (nested-scope locals) is harmless: callers
/// filter by an enclosing-scope set those names can't be in.
fn collect_names_expr(e: &Expr, out: &mut HashSet<String>) {
    match e.unspanned() {
        Expr::Name(n) => {
            out.insert(n.clone());
        }
        Expr::List(xs) | Expr::Tuple(xs) | Expr::Set(xs) | Expr::BoolOp(_, xs) => {
            for x in xs {
                collect_names_expr(x, out);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                if let Some(k) = k {
                    collect_names_expr(k, out);
                }
                collect_names_expr(v, out);
            }
        }
        Expr::Starred(x)
        | Expr::UnaryOp(_, x)
        | Expr::YieldFrom(x)
        | Expr::Await(x)
        | Expr::Attribute(x, _) => collect_names_expr(x, out),
        Expr::Yield(o) => {
            if let Some(x) = o {
                collect_names_expr(x, out);
            }
        }
        Expr::BinOp(_, a, b) | Expr::Subscript(a, b) | Expr::NamedExpr(a, b) => {
            collect_names_expr(a, out);
            collect_names_expr(b, out);
        }
        Expr::Compare(a, rest) => {
            collect_names_expr(a, out);
            for (_, x) in rest {
                collect_names_expr(x, out);
            }
        }
        Expr::IfExp { test, body, orelse } => {
            collect_names_expr(test, out);
            collect_names_expr(body, out);
            collect_names_expr(orelse, out);
        }
        Expr::Call { func, args, keywords } => {
            collect_names_expr(func, out);
            for a in args {
                collect_names_expr(a, out);
            }
            for kw in keywords {
                collect_names_expr(&kw.value, out);
            }
        }
        Expr::Slice { lo, hi, step } => {
            for o in [lo, hi, step].into_iter().flatten() {
                collect_names_expr(o, out);
            }
        }
        Expr::Lambda { params, body } => {
            for d in &params.defaults {
                collect_names_expr(d, out);
            }
            for d in params.kwonly_defaults.iter().flatten() {
                collect_names_expr(d, out);
            }
            collect_names_expr(body, out);
        }
        Expr::ListComp(elt, comps) | Expr::SetComp(elt, comps) | Expr::GenExp(elt, comps) => {
            collect_names_expr(elt, out);
            collect_names_comps(comps, out);
        }
        Expr::DictComp(k, v, comps) => {
            collect_names_expr(k, out);
            collect_names_expr(v, out);
            collect_names_comps(comps, out);
        }
        Expr::FString(parts) => collect_names_fstr(parts, out),
        _ => {} // literals carry no names
    }
}

fn collect_names_comps(comps: &[Comprehension], out: &mut HashSet<String>) {
    for c in comps {
        collect_names_expr(&c.target, out);
        collect_names_expr(&c.iter, out);
        for cond in &c.ifs {
            collect_names_expr(cond, out);
        }
    }
}

fn collect_names_fstr(parts: &[FStrPart], out: &mut HashSet<String>) {
    for p in parts {
        if let FStrPart::Expr { expr, spec, .. } = p {
            collect_names_expr(expr, out);
            collect_names_fstr(spec, out);
        }
    }
}

/// Collect every `Name` referenced in one statement, recursing through suites AND
/// into nested `def`/`class` bodies (their free references may resolve outward).
fn collect_names_stmt(s: &Stmt, out: &mut HashSet<String>) {
    match &s.kind {
        StmtKind::Expr(e) => collect_names_expr(e, out),
        StmtKind::Assign { targets, value } => {
            for t in targets {
                collect_names_expr(t, out);
            }
            collect_names_expr(value, out);
        }
        StmtKind::AugAssign { target, value, .. } => {
            collect_names_expr(target, out);
            collect_names_expr(value, out);
        }
        StmtKind::AnnAssign { target, value, .. } => {
            collect_names_expr(target, out);
            if let Some(v) = value {
                collect_names_expr(v, out);
            }
        }
        StmtKind::If { test, body, orelse }
        | StmtKind::While { test, body, orelse } => {
            collect_names_expr(test, out);
            for st in body.iter().chain(orelse) {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::For { target, iter, body, orelse, .. } => {
            collect_names_expr(target, out);
            collect_names_expr(iter, out);
            for st in body.iter().chain(orelse) {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::With { items, body, .. } => {
            for it in items {
                collect_names_expr(&it.context, out);
                if let Some(a) = &it.vars {
                    collect_names_expr(a, out);
                }
            }
            for st in body {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::FuncDef { params, body, decorators, .. } => {
            for d in &params.defaults {
                collect_names_expr(d, out);
            }
            for d in params.kwonly_defaults.iter().flatten() {
                collect_names_expr(d, out);
            }
            for dec in decorators {
                collect_names_expr(dec, out);
            }
            for st in body {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::ClassDef { bases, keywords, body, decorators, .. } => {
            for b in bases {
                collect_names_expr(b, out);
            }
            for kw in keywords {
                collect_names_expr(&kw.value, out);
            }
            for dec in decorators {
                collect_names_expr(dec, out);
            }
            for st in body {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::Return(Some(e)) => collect_names_expr(e, out),
        StmtKind::Delete(es) => {
            for e in es {
                collect_names_expr(e, out);
            }
        }
        StmtKind::Raise { exc, cause } => {
            if let Some(e) = exc {
                collect_names_expr(e, out);
            }
            if let Some(c) = cause {
                collect_names_expr(c, out);
            }
        }
        StmtKind::Try { body, handlers, orelse, finalbody } => {
            for st in body {
                collect_names_stmt(st, out);
            }
            for h in handlers {
                if let Some(t) = &h.typ {
                    collect_names_expr(t, out);
                }
                for st in &h.body {
                    collect_names_stmt(st, out);
                }
            }
            for st in orelse.iter().chain(finalbody) {
                collect_names_stmt(st, out);
            }
        }
        StmtKind::Assert { test, msg } => {
            collect_names_expr(test, out);
            if let Some(m) = msg {
                collect_names_expr(m, out);
            }
        }
        StmtKind::Match { subject, cases } => {
            collect_names_expr(subject, out);
            for case in cases {
                if let Some(g) = &case.guard {
                    collect_names_expr(g, out);
                }
                for st in &case.body {
                    collect_names_stmt(st, out);
                }
            }
        }
        _ => {} // Pass/Break/Continue/Import/Global/Nonlocal/Return(None) bind no free refs
    }
}

/// Add every simple-name binding target in `e` (a `Name`, or the names nested in
/// tuple/list/starred unpacking). Attribute/subscript targets bind no local name.
fn bind_target(e: &Expr, out: &mut HashSet<String>) {
    let e = e.unspanned();
    match e {
        Expr::Name(n) => {
            out.insert(n.clone());
        }
        Expr::Tuple(items) | Expr::List(items) => {
            for it in items {
                bind_target(it, out);
            }
        }
        Expr::Starred(x) => bind_target(x, out),
        _ => {}
    }
}

/// Add every name bound by one statement to `out`, recursing through
/// control-flow suites at this scope level but NOT into nested `def`/`class`
/// bodies. See [`scope_locals`].
fn collect_bound_stmt(s: &Stmt, out: &mut HashSet<String>) {
    match &s.kind {
        StmtKind::Assign { targets, value } => {
            for t in targets {
                bind_target(t, out);
            }
            collect_leaked_walrus(value, out);
        }
        StmtKind::AugAssign { target, value, .. } => {
            bind_target(target, out);
            collect_leaked_walrus(value, out);
        }
        StmtKind::AnnAssign { target, value, .. } => {
            // A bare-name annotation (`x: int`) makes `x` local even with no value.
            bind_target(target, out);
            if let Some(v) = value {
                collect_leaked_walrus(v, out);
            }
        }
        StmtKind::For {
            target,
            iter,
            body,
            orelse,
            ..
        } => {
            bind_target(target, out);
            collect_leaked_walrus(iter, out);
            for s in body.iter().chain(orelse) {
                collect_bound_stmt(s, out);
            }
        }
        StmtKind::While { test, body, orelse } => {
            collect_leaked_walrus(test, out);
            for s in body.iter().chain(orelse) {
                collect_bound_stmt(s, out);
            }
        }
        StmtKind::If { test, body, orelse } => {
            collect_leaked_walrus(test, out);
            for s in body.iter().chain(orelse) {
                collect_bound_stmt(s, out);
            }
        }
        StmtKind::With { items, body, .. } => {
            for it in items {
                collect_leaked_walrus(&it.context, out);
                if let Some(v) = &it.vars {
                    bind_target(v, out);
                }
            }
            for s in body {
                collect_bound_stmt(s, out);
            }
        }
        StmtKind::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_bound_stmt(s, out);
            }
            for h in handlers {
                if let Some(n) = &h.name {
                    out.insert(n.clone());
                }
                for s in &h.body {
                    collect_bound_stmt(s, out);
                }
            }
        }
        StmtKind::FuncDef { name, .. } | StmtKind::ClassDef { name, .. } => {
            out.insert(name.clone());
        }
        StmtKind::Import(aliases) => {
            for a in aliases {
                // `import a.b.c` binds `a`; `import a.b as x` binds `x`.
                let n = a
                    .asname
                    .clone()
                    .unwrap_or_else(|| a.name.split('.').next().unwrap_or(&a.name).to_string());
                out.insert(n);
            }
        }
        StmtKind::ImportFrom { names, .. } => {
            for a in names {
                out.insert(a.asname.clone().unwrap_or_else(|| a.name.clone()));
            }
        }
        StmtKind::Match { subject, cases } => {
            collect_leaked_walrus(subject, out);
            for c in cases {
                let mut names = Vec::new();
                let _ = validate_pattern(&c.pattern, &mut names);
                for n in names {
                    out.insert(n);
                }
                if let Some(g) = &c.guard {
                    collect_leaked_walrus(g, out);
                }
                for s in &c.body {
                    collect_bound_stmt(s, out);
                }
            }
        }
        StmtKind::Expr(e) | StmtKind::Return(Some(e)) => collect_leaked_walrus(e, out),
        StmtKind::Delete(targets) => {
            // `del x` references a name that must already be local (CPython treats
            // it as a binding for scope purposes).
            for t in targets {
                bind_target(t, out);
            }
        }
        _ => {}
    }
}

/// Collect the names a suite declares `global` / `nonlocal` at this scope level
/// (recursing control flow, not nested `def`/`class` bodies).
fn collect_scope_decls_stmt(s: &Stmt, g: &mut HashSet<String>, nl: &mut HashSet<String>) {
    match &s.kind {
        StmtKind::Global(names) => {
            for n in names {
                g.insert(n.clone());
            }
        }
        StmtKind::Nonlocal(names) => {
            for n in names {
                nl.insert(n.clone());
            }
        }
        StmtKind::If { body, orelse, .. }
        | StmtKind::While { body, orelse, .. }
        | StmtKind::For { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                collect_scope_decls_stmt(s, g, nl);
            }
        }
        StmtKind::With { body, .. } => {
            for s in body {
                collect_scope_decls_stmt(s, g, nl);
            }
        }
        StmtKind::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_scope_decls_stmt(s, g, nl);
            }
            for h in handlers {
                for s in &h.body {
                    collect_scope_decls_stmt(s, g, nl);
                }
            }
        }
        StmtKind::Match { cases, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_scope_decls_stmt(s, g, nl);
                }
            }
        }
        _ => {}
    }
}

/// Like [`collect_walrus_targets`] but descends INTO nested comprehensions —
/// whose walrus (`:=`) targets leak to the enclosing function scope (PEP 572) —
/// while still stopping at a nested `def`/lambda, which owns its walrus targets.
fn collect_leaked_walrus(e: &Expr, out: &mut HashSet<String>) {
    let e = e.unspanned();
    match e {
        Expr::NamedExpr(target, value) => {
            if let Expr::Name(n) = target.unspanned() {
                out.insert(n.clone());
            }
            collect_leaked_walrus(value, out);
        }
        Expr::BoolOp(_, items) | Expr::List(items) | Expr::Tuple(items) | Expr::Set(items) => {
            for it in items {
                collect_leaked_walrus(it, out);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                if let Some(k) = k {
                    collect_leaked_walrus(k, out);
                }
                collect_leaked_walrus(v, out);
            }
        }
        Expr::UnaryOp(_, x)
        | Expr::Starred(x)
        | Expr::Await(x)
        | Expr::YieldFrom(x)
        | Expr::Yield(Some(x)) => collect_leaked_walrus(x, out),
        Expr::BinOp(_, a, b) => {
            collect_leaked_walrus(a, out);
            collect_leaked_walrus(b, out);
        }
        Expr::Compare(a, links) => {
            collect_leaked_walrus(a, out);
            for (_, rhs) in links {
                collect_leaked_walrus(rhs, out);
            }
        }
        Expr::IfExp { test, body, orelse } => {
            collect_leaked_walrus(test, out);
            collect_leaked_walrus(body, out);
            collect_leaked_walrus(orelse, out);
        }
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            collect_leaked_walrus(func, out);
            for a in args {
                collect_leaked_walrus(a, out);
            }
            for kw in keywords {
                collect_leaked_walrus(&kw.value, out);
            }
        }
        Expr::Attribute(x, _) => collect_leaked_walrus(x, out),
        Expr::Subscript(a, b) => {
            collect_leaked_walrus(a, out);
            collect_leaked_walrus(b, out);
        }
        Expr::Slice { lo, hi, step } => {
            for p in [lo, hi, step].into_iter().flatten() {
                collect_leaked_walrus(p, out);
            }
        }
        Expr::ListComp(elt, comps) | Expr::SetComp(elt, comps) | Expr::GenExp(elt, comps) => {
            collect_leaked_walrus(elt, out);
            for c in comps {
                collect_leaked_walrus(&c.iter, out);
                for cond in &c.ifs {
                    collect_leaked_walrus(cond, out);
                }
            }
        }
        Expr::DictComp(k, v, comps) => {
            collect_leaked_walrus(k, out);
            collect_leaked_walrus(v, out);
            for c in comps {
                collect_leaked_walrus(&c.iter, out);
                for cond in &c.ifs {
                    collect_leaked_walrus(cond, out);
                }
            }
        }
        // A lambda owns its own walrus targets — do not descend.
        Expr::Lambda { .. } => {}
        _ => {}
    }
}

/// Collect the names assigned by a walrus (`:=`) anywhere in `e`, without
/// descending into a nested scope (lambda / comprehension / genexpr), whose
/// walrus targets belong to that inner scope.
fn collect_walrus_targets(e: &Expr, out: &mut Vec<String>) {
    let e = e.unspanned();
    match e {
        Expr::NamedExpr(target, value) => {
            if let Expr::Name(n) = target.unspanned() {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
            collect_walrus_targets(value, out);
        }
        Expr::BoolOp(_, items) | Expr::List(items) | Expr::Tuple(items) | Expr::Set(items) => {
            for it in items {
                collect_walrus_targets(it, out);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                if let Some(k) = k {
                    collect_walrus_targets(k, out);
                }
                collect_walrus_targets(v, out);
            }
        }
        Expr::UnaryOp(_, x) | Expr::Starred(x) | Expr::Await(x) | Expr::YieldFrom(x) => {
            collect_walrus_targets(x, out)
        }
        Expr::Yield(Some(x)) => collect_walrus_targets(x, out),
        Expr::BinOp(_, a, b) => {
            collect_walrus_targets(a, out);
            collect_walrus_targets(b, out);
        }
        Expr::Compare(a, links) => {
            collect_walrus_targets(a, out);
            for (_, rhs) in links {
                collect_walrus_targets(rhs, out);
            }
        }
        Expr::IfExp { test, body, orelse } => {
            collect_walrus_targets(test, out);
            collect_walrus_targets(body, out);
            collect_walrus_targets(orelse, out);
        }
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            collect_walrus_targets(func, out);
            for a in args {
                collect_walrus_targets(a, out);
            }
            for kw in keywords {
                collect_walrus_targets(&kw.value, out);
            }
        }
        Expr::Attribute(x, _) => collect_walrus_targets(x, out),
        Expr::Subscript(a, b) => {
            collect_walrus_targets(a, out);
            collect_walrus_targets(b, out);
        }
        Expr::Slice { lo, hi, step } => {
            for p in [lo, hi, step].into_iter().flatten() {
                collect_walrus_targets(p, out);
            }
        }
        // Nested scopes own their own walrus targets — do not descend.
        Expr::Lambda { .. }
        | Expr::ListComp(..)
        | Expr::SetComp(..)
        | Expr::DictComp(..)
        | Expr::GenExp(..) => {}
        _ => {}
    }
}

#[derive(Clone, Copy)]
enum CompKind {
    List,
    Set,
    Dict,
}

/// Whether a suite contains a `yield` at the current function level (does not
/// descend into nested defs/lambdas).
fn body_has_yield(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_yield)
}

/// The docstring of a function/class/module body: its first statement when that
/// is a bare string-literal expression, else `None` (CPython's `__doc__` rule).
fn docstring(body: &[Stmt]) -> Option<String> {
    match body.first().map(|s| &s.kind) {
        Some(StmtKind::Expr(e)) => match e.unspanned() {
            Expr::Str(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn stmt_has_yield(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Expr(e) | StmtKind::Return(Some(e)) => expr_has_yield(e),
        StmtKind::Assign { value, .. } => expr_has_yield(value),
        StmtKind::AugAssign { value, .. } => expr_has_yield(value),
        StmtKind::If { body, orelse, .. } => body_has_yield(body) || body_has_yield(orelse),
        StmtKind::While { body, orelse, .. } => body_has_yield(body) || body_has_yield(orelse),
        StmtKind::For { body, orelse, .. } => body_has_yield(body) || body_has_yield(orelse),
        StmtKind::With { body, .. } => body_has_yield(body),
        StmtKind::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            body_has_yield(body)
                || body_has_yield(orelse)
                || body_has_yield(finalbody)
                || handlers.iter().any(|h| body_has_yield(&h.body))
        }
        StmtKind::Match { cases, .. } => cases.iter().any(|c| body_has_yield(&c.body)),
        _ => false,
    }
}

fn expr_has_yield(e: &Expr) -> bool {
    matches!(e.unspanned(), Expr::Yield(_) | Expr::YieldFrom(_))
}

/// Collect the `(line, keyword)` of every `return`/`break`/`continue` that would
/// jump out of a `finally` block (CPython's `SyntaxWarning`). Descends through
/// `if`/`with`/`match` and a nested `try`'s body/handlers/`else` — but NOT into a
/// nested `try`'s own `finally` (that `try` reports it) nor into `for`/`while`/
/// `def`/`class` (which capture their own control flow, so it never escapes).
fn collect_finally_escapes(stmts: &[Stmt], out: &mut Vec<(u32, String)>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(_) => out.push((s.line, "'return' in a 'finally' block".to_string())),
            StmtKind::Break => out.push((s.line, "'break' in a 'finally' block".to_string())),
            StmtKind::Continue => out.push((s.line, "'continue' in a 'finally' block".to_string())),
            StmtKind::If { body, orelse, .. } => {
                collect_finally_escapes(body, out);
                collect_finally_escapes(orelse, out);
            }
            StmtKind::With { body, .. } => collect_finally_escapes(body, out),
            StmtKind::Match { cases, .. } => {
                for c in cases {
                    collect_finally_escapes(&c.body, out);
                }
            }
            StmtKind::Try {
                body,
                handlers,
                orelse,
                ..
            } => {
                collect_finally_escapes(body, out);
                collect_finally_escapes(orelse, out);
                for h in handlers {
                    collect_finally_escapes(&h.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether a loop body has a `break`/`continue` (belonging to THIS loop) that
/// sits inside a `try` or `with` block. Such a jump must cross the sub-chunk
/// boundary those constructs introduce (a `finally`/`__exit__` has to run first),
/// The fusevm native ops for the Python binary operators whose native codegen is
/// overflow-safe under `int_overflow_deopt` — `+`, `-`, `*` only. Division,
/// modulo, power, bitwise, etc. are not lowered to native slot arithmetic.
fn native_arith_op(op: BinOp) -> Option<Op> {
    match op {
        BinOp::Add => Some(Op::Add),
        BinOp::Sub => Some(Op::Sub),
        BinOp::Mul => Some(Op::Mul),
        _ => None,
    }
}

/// A native-loop body value expression: only int literals, names, unary `+`/`-`,
/// and `+`/`-`/`*` binops are allowed (each name read is appended to `reads`).
/// Anything else — a call, comparison, subscript, attribute, float, `/`, `//`,
/// `%`, `**`, etc. — returns `false`, disqualifying the whole loop.
fn native_safe_value(e: &Expr, reads: &mut Vec<String>) -> bool {
    match e.unspanned() {
        Expr::Int(_) => true,
        Expr::Name(n) => {
            if !reads.iter().any(|r| r == n) {
                reads.push(n.clone());
            }
            true
        }
        Expr::UnaryOp(UnOp::Neg | UnOp::Pos, inner) => native_safe_value(inner, reads),
        Expr::BinOp(op, l, r) => {
            native_arith_op(*op).is_some()
                && native_safe_value(l, reads)
                && native_safe_value(r, reads)
        }
        _ => false,
    }
}

/// If `iter` is `range(a)` / `range(a, b)` / `range(a, b, 1)` with `range` named
/// directly, return `(start?, stop)`. Only step `1` is handled natively; any
/// other step (or a non-`range` call) yields `None`, so the loop falls back.
fn native_range_call(iter: &Expr) -> Option<(Option<&Expr>, &Expr)> {
    let (func, args) = match iter.unspanned() {
        Expr::Call {
            func,
            args,
            keywords,
        } if keywords.is_empty() => (func, args),
        _ => return None,
    };
    match func.unspanned() {
        Expr::Name(n) if n == "range" => {}
        _ => return None,
    }
    match args.len() {
        1 => Some((None, &args[0])),
        2 => Some((Some(&args[0]), &args[1])),
        3 => match args[2].unspanned() {
            Expr::Int(1) => Some((Some(&args[0]), &args[1])),
            _ => None,
        },
        _ => None,
    }
}

fn push_unique(v: &mut Vec<String>, name: &str) {
    if !v.iter().any(|x| x == name) {
        v.push(name.to_string());
    }
}

/// The fusevm comparison op for a Python comparator, for the arithmetic
/// comparisons whose native codegen matches Python integer semantics. `is`/`in`
/// and their negations are not lowered.
fn native_cmp_op(op: CmpOp) -> Option<Op> {
    match op {
        CmpOp::Eq => Some(Op::NumEq),
        CmpOp::Ne => Some(Op::NumNe),
        CmpOp::Lt => Some(Op::NumLt),
        CmpOp::Le => Some(Op::NumLe),
        CmpOp::Gt => Some(Op::NumGt),
        CmpOp::Ge => Some(Op::NumGe),
        _ => None,
    }
}

/// A native-safe loop/branch condition: a single (non-chained) arithmetic
/// comparison of native-safe integer operands, or a native-safe integer
/// expression used for truthiness. Collects the names it reads.
fn native_safe_cond(test: &Expr, reads: &mut Vec<String>) -> bool {
    match test.unspanned() {
        Expr::Compare(lhs, rest) if rest.len() == 1 && native_cmp_op(rest[0].0).is_some() => {
            native_safe_value(lhs, reads) && native_safe_value(&rest[0].1, reads)
        }
        other => native_safe_value(other, reads),
    }
}

/// Recursively validate a native for-range nest and collect its `loop_vars`
/// (names bound by a loop), `reads` (names read in bounds/values), and `writes`
/// (names assigned, incl. loop vars). Every statement must be `pass`, a single
/// slot-safe integer `Name = value`, a `+=`/`-=`/`*=` on a `Name`, or a nested
/// `for <name> in range(...)` whose body is itself native-safe — none of which
/// may rebind an enclosing loop variable, and no `break`/`continue`/`else`.
/// Returns `false` on anything else (the whole loop then falls back).
fn analyze_native_tree(
    body: &[Stmt],
    loop_vars: &mut Vec<String>,
    reads: &mut Vec<String>,
    writes: &mut Vec<String>,
) -> bool {
    analyze_native_tree_at(body, false, loop_vars, reads, writes)
}

/// `conditional` is true inside an `if`/`while` body: a `for` there is rejected
/// because its loop variable's slot might never be initialized on a path where
/// the enclosing loop still runs, which would make its namespace write-back leak
/// `Undef`. (A `while` binds no variable, so it may appear conditionally.)
fn analyze_native_tree_at(
    body: &[Stmt],
    conditional: bool,
    loop_vars: &mut Vec<String>,
    reads: &mut Vec<String>,
    writes: &mut Vec<String>,
) -> bool {
    for s in body {
        match &s.kind {
            StmtKind::Pass => {}
            StmtKind::Assign { targets, value } => {
                if targets.len() != 1 {
                    return false;
                }
                let name = match targets[0].unspanned() {
                    Expr::Name(n) => n,
                    _ => return false,
                };
                if loop_vars.iter().any(|v| v == name) {
                    return false; // must not rebind a loop variable
                }
                if !native_safe_value(value, reads) {
                    return false;
                }
                push_unique(writes, name);
            }
            StmtKind::AugAssign { target, op, value } => {
                let name = match target.unspanned() {
                    Expr::Name(n) => n,
                    _ => return false,
                };
                if loop_vars.iter().any(|v| v == name) || native_arith_op(*op).is_none() {
                    return false;
                }
                push_unique(reads, name); // `x op= v` reads x
                if !native_safe_value(value, reads) {
                    return false;
                }
                push_unique(writes, name);
            }
            StmtKind::If { test, body: tb, orelse } => {
                if !native_safe_cond(test, reads) {
                    return false;
                }
                if !analyze_native_tree_at(tb, true, loop_vars, reads, writes)
                    || !analyze_native_tree_at(orelse, true, loop_vars, reads, writes)
                {
                    return false;
                }
            }
            StmtKind::While { test, body: wb, orelse } => {
                if !orelse.is_empty() || loop_needs_signal(wb) {
                    return false;
                }
                if !native_safe_cond(test, reads) {
                    return false;
                }
                // A `while` re-tests its condition, so its body is conditional.
                if !analyze_native_tree_at(wb, true, loop_vars, reads, writes) {
                    return false;
                }
            }
            StmtKind::For {
                target,
                iter,
                body: inner,
                orelse,
                is_async,
            } => {
                if conditional || *is_async || !orelse.is_empty() || loop_needs_signal(inner) {
                    return false; // no conditional/async/for-else/break-continue for
                }
                let tname = match target.unspanned() {
                    Expr::Name(n) => n.clone(),
                    _ => return false,
                };
                let (start, stop) = match native_range_call(iter) {
                    Some(b) => b,
                    None => return false,
                };
                if let Some(e) = start {
                    if !native_safe_value(e, reads) {
                        return false;
                    }
                }
                if !native_safe_value(stop, reads) {
                    return false;
                }
                push_unique(loop_vars, &tname);
                push_unique(writes, &tname);
                if !analyze_native_tree_at(inner, false, loop_vars, reads, writes) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Collect names read in an expression that are not yet in `defined` — those
/// need their pre-loop namespace value loaded into a slot.
fn reads_needing_load(e: &Expr, defined: &[String], loads: &mut Vec<String>) {
    match e.unspanned() {
        Expr::Name(n) => {
            if !defined.iter().any(|d| d == n) {
                push_unique(loads, n);
            }
        }
        Expr::UnaryOp(_, inner) => reads_needing_load(inner, defined, loads),
        Expr::BinOp(_, l, r) => {
            reads_needing_load(l, defined, loads);
            reads_needing_load(r, defined, loads);
        }
        Expr::Compare(lhs, rest) => {
            reads_needing_load(lhs, defined, loads);
            for (_, r) in rest {
                reads_needing_load(r, defined, loads);
            }
        }
        _ => {} // int literal (validated native-safe upstream)
    }
}

/// Definite-assignment walk over a native loop body. Appends to `loads` every
/// name READ before it is definitely WRITTEN (so its slot must be seeded from the
/// namespace), and grows `defined` with the names UNCONDITIONALLY assigned by the
/// end of `body`. Writes inside `if`/`while`/`for` bodies are conditional and do
/// NOT escape into `defined` (each is analyzed against a clone), so a slot that a
/// conditional path leaves untouched keeps its seeded value.
fn collect_ns_loads(body: &[Stmt], defined: &mut Vec<String>, loads: &mut Vec<String>) {
    for s in body {
        match &s.kind {
            StmtKind::Pass => {}
            StmtKind::Assign { targets, value } => {
                reads_needing_load(value, defined, loads);
                if let Expr::Name(n) = targets[0].unspanned() {
                    push_unique(defined, n);
                }
            }
            StmtKind::AugAssign { target, value, .. } => {
                if let Expr::Name(n) = target.unspanned() {
                    if !defined.iter().any(|d| d == n) {
                        push_unique(loads, n); // read-modify-write reads first
                    }
                    reads_needing_load(value, defined, loads);
                    push_unique(defined, n);
                }
            }
            StmtKind::If { test, body: tb, orelse } => {
                reads_needing_load(test, defined, loads);
                let mut d1 = defined.clone();
                collect_ns_loads(tb, &mut d1, loads);
                let mut d2 = defined.clone();
                collect_ns_loads(orelse, &mut d2, loads);
            }
            StmtKind::While { test, body: wb, .. } => {
                reads_needing_load(test, defined, loads);
                let mut d = defined.clone();
                collect_ns_loads(wb, &mut d, loads);
            }
            StmtKind::For {
                target, iter, body: inner, ..
            } => {
                if let Some((start, stop)) = native_range_call(iter) {
                    if let Some(e) = start {
                        reads_needing_load(e, defined, loads);
                    }
                    reads_needing_load(stop, defined, loads);
                }
                // The loop var is initialized by the loop; its body is per-iteration
                // (conditional), so writes there don't escape into `defined`.
                let mut d = defined.clone();
                if let Expr::Name(n) = target.unspanned() {
                    push_unique(&mut d, n);
                }
                collect_ns_loads(inner, &mut d, loads);
            }
            _ => {}
        }
    }
}

/// Soundness gate for write-backs: every written name must have a valid slot when
/// its value is stored back — i.e. it is loaded (`loads`), a loop variable
/// (always initialized), or `guard_gated` (a for-range whose write-backs sit
/// behind the empty-range guard, so an unconditionally-`defined` name is also
/// safe). A conditionally-assigned write-first name fails and the loop falls back.
fn native_writebacks_valid(
    writes: &[String],
    loads: &[String],
    defined: &[String],
    loop_vars: &[String],
    guard_gated: bool,
) -> bool {
    writes.iter().all(|w| {
        loads.iter().any(|x| x == w)
            || loop_vars.iter().any(|v| v == w)
            || (guard_gated && defined.iter().any(|d| d == w))
    })
}

/// so it can't be a plain in-chunk jump — the loop uses the `LOOP_BODY`
/// signal path instead. The walk descends through `if`/`try`/`with`/`match`
/// (which share this loop) but NOT into nested `for`/`while`/`def`/`class`
/// (which own their own `break`/`continue`).
fn loop_needs_signal(body: &[Stmt]) -> bool {
    fn scan(stmts: &[Stmt], in_boundary: bool) -> bool {
        stmts.iter().any(|s| match &s.kind {
            StmtKind::Break | StmtKind::Continue => in_boundary,
            StmtKind::If { body, orelse, .. } => {
                scan(body, in_boundary) || scan(orelse, in_boundary)
            }
            StmtKind::Match { cases, .. } => cases.iter().any(|c| scan(&c.body, in_boundary)),
            StmtKind::With { body, .. } => scan(body, true),
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                scan(body, true)
                    || scan(orelse, true)
                    || scan(finalbody, true)
                    || handlers.iter().any(|h| scan(&h.body, true))
            }
            _ => false,
        })
    }
    scan(body, false)
}

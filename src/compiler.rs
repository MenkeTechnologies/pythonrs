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
use crate::host::{binop as bop, ops, unop, FuncDef, TryDef};
use fusevm::{Chunk, ChunkBuilder, Op, Value};

/// A compiled program: the top-level chunk plus the def/lambda/class-body
/// template table (indexed by def id) and the try-block table.
#[derive(Default)]
pub struct Program {
    pub main: Chunk,
    pub functions: Vec<(String, FuncDef)>,
    pub procs: Vec<FuncDef>,
    pub tries: Vec<TryDef>,
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
            Op::CallBuiltin(id, 1) if id == ops::TRY => try_off,
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

/// Break/continue jump fixups for a native loop.
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

#[derive(Default)]
pub struct Compiler {
    functions: Vec<(String, FuncDef)>,
    tries: Vec<TryDef>,
    loops: Vec<LoopCtx>,
    tmp: usize,
    debug: bool,
}

/// Compile a parsed program. `debug` enables per-statement DAP line markers.
pub fn compile(stmts: &[Stmt], debug: bool) -> Result<Program, String> {
    let mut c = Compiler {
        debug,
        ..Default::default()
    };
    let mut b = ChunkBuilder::new();
    c.compile_stmts(&mut b, stmts)?;
    Ok(Program {
        main: b.build(),
        functions: c.functions,
        procs: Vec::new(),
        tries: c.tries,
    })
}

fn argc(n: usize) -> Result<u8, String> {
    u8::try_from(n).map_err(|_| "too many arguments (>255) for one call".to_string())
}

impl Compiler {
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
        if self.debug && s.line != 0 {
            b.emit(Op::LoadInt(s.line as i64), s.line);
            b.emit(Op::CallBuiltin(ops::DBG_LINE, 1), s.line);
            b.emit(Op::Pop, s.line);
        }
        let line = s.line;
        match &s.kind {
            StmtKind::Expr(e) => {
                self.compile_expr(b, e)?;
                b.emit(Op::Pop, line);
            }
            StmtKind::Pass => {}
            StmtKind::Assign { targets, value } => {
                self.compile_expr(b, value)?;
                // Store to every target (dup for all but the last).
                for (i, t) in targets.iter().enumerate() {
                    if i + 1 < targets.len() {
                        b.emit(Op::Dup, line);
                    }
                    self.compile_assign(b, t)?;
                }
            }
            StmtKind::AnnAssign { target, value, .. } => {
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
                ..
            } => {
                self.compile_for(b, target, iter, body, orelse)?;
            }
            StmtKind::FuncDef {
                name,
                params,
                body,
                decorators,
                ..
            } => {
                self.compile_funcdef(b, name, params, body, decorators)?;
            }
            StmtKind::ClassDef {
                name,
                bases,
                body,
                decorators,
                ..
            } => {
                self.compile_classdef(b, name, bases, body, decorators)?;
            }
            StmtKind::Return(e) => {
                match e {
                    Some(e) => self.compile_expr(b, e)?,
                    None => {
                        b.emit(Op::LoadUndef, line);
                    }
                }
                b.emit(Op::CallBuiltin(ops::SIG_RETURN, 1), line);
            }
            StmtKind::Break => {
                let j = b.emit(Op::Jump(0), line);
                self.loops
                    .last_mut()
                    .ok_or("SyntaxError: 'break' outside loop")?
                    .breaks
                    .push(j);
            }
            StmtKind::Continue => {
                let j = b.emit(Op::Jump(0), line);
                self.loops
                    .last_mut()
                    .ok_or("SyntaxError: 'continue' outside loop")?
                    .continues
                    .push(j);
            }
            StmtKind::Delete(targets) => {
                for t in targets {
                    self.compile_delete(b, t)?;
                }
            }
            StmtKind::Global(names) | StmtKind::Nonlocal(names) => {
                // `nonlocal` is approximated by `global` in the current scope model.
                for n in names {
                    self.name_const(b, n);
                    b.emit(Op::CallBuiltin(ops::DECLARE_GLOBAL, 1), line);
                    b.emit(Op::Pop, line);
                }
            }
            StmtKind::Raise { exc, .. } => match exc {
                Some(e) => {
                    self.compile_expr(b, e)?;
                    b.emit(Op::CallBuiltin(ops::RAISE, 1), line);
                }
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
            StmtKind::With { items, body, .. } => {
                self.compile_with(b, items, body)?;
            }
            StmtKind::Assert { test, msg } => {
                self.compile_assert(b, test, msg)?;
            }
            StmtKind::Import(aliases) => {
                for a in aliases {
                    self.strlit(b, &a.name);
                    b.emit(Op::CallBuiltin(ops::IMPORT, 1), line);
                    let bind = a
                        .asname
                        .clone()
                        .unwrap_or_else(|| a.name.split('.').next().unwrap_or(&a.name).to_string());
                    self.store_name(b, &bind);
                }
            }
            StmtKind::ImportFrom { module, names, .. } => {
                let m = module.clone().unwrap_or_default();
                for a in names {
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
        self.name_const(b, name);
        b.emit(Op::Swap, 0);
        b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
        b.emit(Op::Pop, 0);
    }

    fn compile_assign(&mut self, b: &mut ChunkBuilder, target: &Expr) -> Result<(), String> {
        // Value is on top of stack.
        match target {
            Expr::Name(n) => {
                self.name_const(b, n);
                b.emit(Op::Swap, 0);
                b.emit(Op::CallBuiltin(ops::SETLOCAL, 2), 0);
                b.emit(Op::Pop, 0);
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

    fn compile_augassign(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        op: BinOp,
        value: &Expr,
    ) -> Result<(), String> {
        // Desugar `t op= v` -> `t = t op v`. Names & simple targets only (the
        // subscript/attribute path re-evaluates the receiver, matching output
        // for side-effect-free receivers).
        let combined = Expr::BinOp(op, Box::new(target.clone()), Box::new(value.clone()));
        self.compile_expr(b, &combined)?;
        self.compile_assign(b, target)?;
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
        let start = b.current_pos();
        self.compile_condition(b, test)?;
        let jfalse = b.emit(Op::JumpIfFalse(0), 0);
        self.loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
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

    fn compile_for(
        &mut self,
        b: &mut ChunkBuilder,
        target: &Expr,
        iter: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
        let def_id = self.build_function(name, params, body)?;
        // Push defaults, then the func id, then MKFUNC.
        for d in &params.defaults {
            self.compile_expr(b, d)?;
        }
        b.emit(Op::LoadInt(def_id as i64), 0);
        // MKFUNC reads [func_id, defaults...] with func_id pushed first; we push
        // defaults first, so reverse by emitting func id after — the handler
        // pops accordingly. To keep func_id first on the arg vector, we instead
        // rotate: simplest is to push func id BEFORE defaults. Re-emit clean:
        // (defaults already emitted above are below func id) -> handler treats
        // the top as func id.
        let n = params.defaults.len();
        b.emit(Op::CallBuiltin(ops::MKFUNC, argc(1 + n)?), 0);
        // Apply decorators (innermost first).
        for d in decorators.iter().rev() {
            self.compile_expr(b, d)?; // [func, dec]
            b.emit(Op::Swap, 0); // [dec, func]
            b.emit(Op::CallBuiltin(ops::CALL_VALUE, 2), 0);
        }
        self.store_name(b, name);
        Ok(())
    }

    fn build_function(
        &mut self,
        name: &str,
        params: &Params,
        body: &[Stmt],
    ) -> Result<usize, String> {
        let mut fb = ChunkBuilder::new();
        self.compile_stmts(&mut fb, body)?;
        let is_generator = body_has_yield(body);
        let def = FuncDef {
            name: name.to_string(),
            params: params.names.clone(),
            ndefaults: params.defaults.len(),
            star: params.star.clone(),
            kwonly: params.kwonly.clone(),
            kwonly_required: params.kwonly_defaults.iter().map(|d| d.is_none()).collect(),
            kwargs: params.kwargs.clone(),
            chunk: fb.build(),
            is_generator,
        };
        self.functions.push((name.to_string(), def));
        Ok(self.functions.len() - 1)
    }

    fn compile_classdef(
        &mut self,
        b: &mut ChunkBuilder,
        name: &str,
        bases: &[Expr],
        body: &[Stmt],
        decorators: &[Expr],
    ) -> Result<(), String> {
        // Class body compiles as a parameterless function that assigns members
        // into its local env; BUILD_CLASS captures that env as the namespace.
        let empty = Params::default();
        let def_id = self.build_function(&format!("<class {name}>"), &empty, body)?;
        // bases list
        for base in bases {
            self.compile_expr(b, base)?;
        }
        b.emit(Op::CallBuiltin(ops::MKLIST, argc(bases.len())?), 0); // [bases]
        self.name_const(b, name); // [bases, name]
        b.emit(Op::LoadInt(def_id as i64), 0);
        b.emit(Op::CallBuiltin(ops::MKFUNC, 1), 0); // [bases, name, bodyfunc]
        b.emit(Op::CallBuiltin(ops::BUILD_CLASS, 3), 0); // -> class value
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
        self.compile_stmts(&mut cb, stmts)?;
        Ok(cb.build())
    }

    /// Compile a single expression into a chunk leaving its value on the stack.
    fn compile_expr_chunk(&mut self, e: &Expr) -> Result<Chunk, String> {
        let mut cb = ChunkBuilder::new();
        self.compile_expr(&mut cb, e)?;
        Ok(cb.build())
    }

    fn compile_with(
        &mut self,
        b: &mut ChunkBuilder,
        items: &[WithItem],
        body: &[Stmt],
    ) -> Result<(), String> {
        // Desugar `with a as x: body` -> enter, then try/finally exit.
        // Build a synthetic body that binds the context vars and runs the suite.
        let mut inner: Vec<Stmt> = Vec::new();
        for it in items {
            // x = ctx.__enter__()
            let enter = Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(it.context.clone()),
                    "__enter__".into(),
                )),
                args: vec![],
                keywords: vec![],
            };
            match &it.vars {
                Some(v) => inner.push(
                    StmtKind::Assign {
                        targets: vec![v.clone()],
                        value: enter,
                    }
                    .into(),
                ),
                None => inner.push(StmtKind::Expr(enter).into()),
            }
        }
        inner.extend_from_slice(body);
        // finally: ctx.__exit__(None, None, None)
        let mut fin: Vec<Stmt> = Vec::new();
        for it in items {
            let exit = Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(it.context.clone()),
                    "__exit__".into(),
                )),
                args: vec![Expr::None, Expr::None, Expr::None],
                keywords: vec![],
            };
            fin.push(StmtKind::Expr(exit).into());
        }
        self.compile_try(b, &inner, &[], &[], &fin)
    }

    // ── expressions ──────────────────────────────────────────────────────
    fn compile_expr(&mut self, b: &mut ChunkBuilder, e: &Expr) -> Result<(), String> {
        match e {
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
                b.emit(Op::LoadUndef, 0);
            }
            Expr::Str(s) => self.strlit(b, s),
            Expr::Bytes(_) => {
                // Minimal: represent bytes literal as empty for now (see BUGS).
                self.strlit(b, "");
            }
            Expr::FString(parts) => self.compile_fstring(b, parts)?,
            Expr::Name(n) => {
                self.name_const(b, n);
                b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0);
            }
            Expr::List(items) => {
                self.compile_seq(b, items)?;
                b.emit(Op::CallBuiltin(ops::MKLIST, argc(items.len())?), 0);
            }
            Expr::Tuple(items) => {
                self.compile_seq(b, items)?;
                b.emit(Op::CallBuiltin(ops::MKTUPLE, argc(items.len())?), 0);
            }
            Expr::Set(items) => {
                self.compile_seq(b, items)?;
                b.emit(Op::CallBuiltin(ops::MKSET, argc(items.len())?), 0);
            }
            Expr::Dict(pairs) => {
                for (k, v) in pairs {
                    match k {
                        Some(k) => {
                            self.compile_expr(b, k)?;
                            self.compile_expr(b, v)?;
                        }
                        None => {
                            // **mapping spread — see BUGS; skipped for now.
                            return Err("dict ** unpacking not yet supported".into());
                        }
                    }
                }
                b.emit(Op::CallBuiltin(ops::MKDICT, argc(pairs.len() * 2)?), 0);
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
                b.emit(Op::CallBuiltin(ops::GETATTR, 2), 0);
            }
            Expr::Subscript(recv, idx) => {
                self.compile_expr(b, recv)?;
                self.compile_subscript_index(b, idx)?;
                b.emit(Op::CallBuiltin(ops::GETITEM, 2), 0);
            }
            Expr::Slice { lo, hi, step } => {
                self.compile_opt(b, lo)?;
                self.compile_opt(b, hi)?;
                self.compile_opt(b, step)?;
                b.emit(Op::CallBuiltin(ops::MKSLICE, 3), 0);
            }
            Expr::Lambda { params, body } => {
                let bodystmt = vec![Stmt::from(StmtKind::Return(Some((**body).clone())))];
                let def_id = self.build_function("<lambda>", params, &bodystmt)?;
                for d in &params.defaults {
                    self.compile_expr(b, d)?;
                }
                b.emit(Op::LoadInt(def_id as i64), 0);
                b.emit(
                    Op::CallBuiltin(ops::MKFUNC, argc(1 + params.defaults.len())?),
                    0,
                );
            }
            Expr::ListComp(elt, comps) => {
                self.compile_comprehension(b, CompKind::List, elt, None, comps)?
            }
            Expr::SetComp(elt, comps) => {
                self.compile_comprehension(b, CompKind::Set, elt, None, comps)?
            }
            Expr::GenExp(elt, comps) => {
                self.compile_comprehension(b, CompKind::List, elt, None, comps)?
            }
            Expr::DictComp(k, v, comps) => {
                self.compile_comprehension(b, CompKind::Dict, k, Some(v), comps)?
            }
            Expr::NamedExpr(target, value) => {
                self.compile_expr(b, value)?;
                b.emit(Op::Dup, 0);
                self.compile_assign(b, target)?;
            }
            Expr::Yield(_) | Expr::YieldFrom(_) => {
                return Err(
                    "yield is only valid inside a generator (unsupported; see BUGS)".into(),
                );
            }
            Expr::Await(inner) => self.compile_expr(b, inner)?,
        }
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

    fn compile_subscript_index(&mut self, b: &mut ChunkBuilder, idx: &Expr) -> Result<(), String> {
        self.compile_expr(b, idx)
    }

    fn compile_fstring(&mut self, b: &mut ChunkBuilder, parts: &[FStrPart]) -> Result<(), String> {
        let mut n = 0;
        for p in parts {
            match p {
                FStrPart::Lit(s) => {
                    let k = b.add_constant(Value::str(s));
                    b.emit(Op::LoadConst(k), 0);
                    n += 1;
                }
                FStrPart::Expr { expr, conv, spec } => {
                    self.compile_expr(b, expr)?;
                    let conv_i = match conv {
                        Some('s') => 1,
                        Some('r') => 2,
                        Some('a') => 3,
                        _ => 0,
                    };
                    b.emit(Op::LoadInt(conv_i), 0);
                    let spec = spec.clone().unwrap_or_default();
                    let k = b.add_constant(Value::str(spec));
                    b.emit(Op::LoadConst(k), 0);
                    b.emit(Op::CallBuiltin(ops::FORMAT, 3), 0);
                    n += 1;
                }
            }
        }
        b.emit(Op::CallBuiltin(ops::MKSTR, argc(n)?), 0);
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
                b.emit(Op::Negate, 0);
            }
            UnOp::Not => {
                self.compile_condition(b, e)?;
                b.emit(Op::LogNot, 0);
            }
            UnOp::Invert => {
                b.emit(Op::LoadInt(unop::INVERT), 0);
                self.compile_expr(b, e)?;
                b.emit(Op::CallBuiltin(ops::UNARY, 2), 0);
            }
            UnOp::Pos => {
                b.emit(Op::LoadInt(unop::POS), 0);
                self.compile_expr(b, e)?;
                b.emit(Op::CallBuiltin(ops::UNARY, 2), 0);
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
                b.emit(Op::Add, 0);
                return Ok(());
            }
            BinOp::Sub => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                b.emit(Op::Sub, 0);
                return Ok(());
            }
            BinOp::Mul => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                b.emit(Op::Mul, 0);
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
        b.emit(Op::CallBuiltin(ops::BINOP, 3), 0);
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
        // Chained: a<b<c -> (a<b) and (b<c). Re-evaluates interior operands
        // (side-effect-free operands only; see BUGS).
        let mut conj: Vec<Expr> = Vec::new();
        let mut prev = left.clone();
        for (op, rhs) in ops_list {
            conj.push(Expr::Compare(
                Box::new(prev.clone()),
                vec![(*op, rhs.clone())],
            ));
            prev = rhs.clone();
        }
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
                b.emit(Op::NumEq, 0);
            }
            CmpOp::Ne => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::NumNe, 0);
            }
            CmpOp::Lt => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::NumLt, 0);
            }
            CmpOp::Le => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::NumLe, 0);
            }
            CmpOp::Gt => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::NumGt, 0);
            }
            CmpOp::Ge => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::NumGe, 0);
            }
            CmpOp::Is => {
                self.compile_expr(b, left)?;
                self.compile_expr(b, rhs)?;
                b.emit(Op::CallBuiltin(ops::IS, 2), 0);
            }
            CmpOp::IsNot => {
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
            return Err("call-site * / ** unpacking not yet supported (see BUGS)".into());
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
        match func {
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(Op::CallBuiltin(ops::CALL_METHOD, argc(2 + args.len())?), 0);
                } else {
                    build_kw(self, b)?;
                    b.emit(
                        Op::CallBuiltin(ops::CALL_METHOD_KW, argc(3 + args.len())?),
                        0,
                    );
                }
            }
            Expr::Name(n) => {
                self.name_const(b, n);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(Op::CallBuiltin(ops::CALL, argc(1 + args.len())?), 0);
                } else {
                    build_kw(self, b)?;
                    b.emit(Op::CallBuiltin(ops::CALL_KW, argc(2 + args.len())?), 0);
                }
            }
            _ => {
                self.compile_expr(b, func)?;
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(Op::CallBuiltin(ops::CALL_VALUE, argc(1 + args.len())?), 0);
                } else {
                    build_kw(self, b)?;
                    b.emit(
                        Op::CallBuiltin(ops::CALL_VALUE_KW, argc(2 + args.len())?),
                        0,
                    );
                }
            }
        }
        Ok(())
    }

    // ── comprehensions ───────────────────────────────────────────────────
    fn compile_comprehension(
        &mut self,
        b: &mut ChunkBuilder,
        kind: CompKind,
        elt: &Expr,
        val: Option<&Expr>,
        comps: &[Comprehension],
    ) -> Result<(), String> {
        // Accumulate into a hidden local, then leave it on the stack.
        let acc = format!("__comp{}__", self.tmp);
        self.tmp += 1;
        match kind {
            CompKind::List => b.emit(Op::CallBuiltin(ops::MKLIST, 0), 0),
            CompKind::Set => b.emit(Op::CallBuiltin(ops::MKSET, 0), 0),
            CompKind::Dict => b.emit(Op::CallBuiltin(ops::MKDICT, 0), 0),
        };
        self.store_name(b, &acc);
        self.compile_comp_clauses(b, kind, &acc, elt, val, comps, 0)?;
        self.name_const(b, &acc);
        b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0);
        Ok(())
    }

    // A lowering helper that legitimately threads compiler + comprehension state.
    #[allow(clippy::too_many_arguments)]
    fn compile_comp_clauses(
        &mut self,
        b: &mut ChunkBuilder,
        kind: CompKind,
        acc: &str,
        elt: &Expr,
        val: Option<&Expr>,
        comps: &[Comprehension],
        depth: usize,
    ) -> Result<(), String> {
        if depth == comps.len() {
            // Innermost: append/add/insert the element.
            self.name_const(b, acc);
            b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), 0); // [acc]
            match kind {
                CompKind::List => {
                    self.name_const(b, "append");
                    self.compile_expr(b, elt)?;
                    b.emit(Op::CallBuiltin(ops::CALL_METHOD, 3), 0);
                    b.emit(Op::Pop, 0);
                }
                CompKind::Set => {
                    self.name_const(b, "add");
                    self.compile_expr(b, elt)?;
                    b.emit(Op::CallBuiltin(ops::CALL_METHOD, 3), 0);
                    b.emit(Op::Pop, 0);
                }
                CompKind::Dict => {
                    // acc[key] = value
                    self.compile_expr(b, elt)?; // key
                    self.compile_expr(b, val.unwrap())?; // value
                    b.emit(Op::CallBuiltin(ops::SETITEM, 3), 0);
                    b.emit(Op::Pop, 0);
                }
            }
            return Ok(());
        }
        let comp = &comps[depth];
        self.compile_expr(b, &comp.iter)?;
        b.emit(Op::CallBuiltin(ops::GETITER, 1), 0);
        let start = b.current_pos();
        b.emit(Op::CallBuiltin(ops::FORITER, 0), 0);
        let jdone = b.emit(Op::JumpIfFalse(0), 0);
        self.compile_assign(b, &comp.target)?;
        // Guards.
        let mut guard_jumps = Vec::new();
        for cond in &comp.ifs {
            self.compile_condition(b, cond)?;
            let jf = b.emit(Op::JumpIfFalse(0), 0);
            guard_jumps.push(jf);
        }
        self.compile_comp_clauses(b, kind, acc, elt, val, comps, depth + 1)?;
        // Guards jump here (skip element) -> continue loop.
        let cont = b.current_pos();
        for j in guard_jumps {
            b.patch_jump(j, cont);
        }
        b.emit(Op::Jump(start), 0);
        let done = b.current_pos();
        b.patch_jump(jdone, done);
        b.emit(Op::Pop, 0); // drop iterator
        Ok(())
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
        _ => false,
    }
}

fn expr_has_yield(e: &Expr) -> bool {
    matches!(e, Expr::Yield(_) | Expr::YieldFrom(_))
}

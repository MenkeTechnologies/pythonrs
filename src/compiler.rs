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
    /// The source line of the statement currently being lowered. Call ops carry
    /// it so an uncaught exception's traceback can name each call-site frame's
    /// line (expression ops otherwise emit line 0).
    cur_line: u32,
    /// Number of enclosing real scopes (`def`/`lambda`/class body) — comprehension
    /// hidden functions do NOT count. Decides whether a walrus (`:=`) inside a
    /// comprehension leaks to module scope (`global`, depth 0) or the enclosing
    /// function (`nonlocal`, depth > 0), per PEP 572.
    fn_depth: usize,
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
            StmtKind::Global(names) => {
                for n in names {
                    self.name_const(b, n);
                    b.emit(Op::CallBuiltin(ops::DECLARE_GLOBAL, 1), line);
                    b.emit(Op::Pop, line);
                }
            }
            StmtKind::Nonlocal(names) => {
                for n in names {
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
        is_async: bool,
    ) -> Result<(), String> {
        self.fn_depth += 1;
        let def_id = self.build_function_ex(name, params, body, is_async);
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

    /// Emit the `MKFUNC` sequence for `def_id`: push the evaluated positional
    /// defaults, then the keyword-only defaults, a count of them, and the func id
    /// (kept immediately below `MKFUNC` so id-rebasing still finds it). Assumes
    /// nothing this call needs is already on the stack.
    fn emit_make_func(
        &mut self,
        b: &mut ChunkBuilder,
        def_id: usize,
        params: &Params,
    ) -> Result<(), String> {
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
        let total = params.defaults.len() + nkw + 2; // + count + func id
        b.emit(Op::CallBuiltin(ops::MKFUNC, argc(total)?), 0);
        Ok(())
    }

    fn build_function(
        &mut self,
        name: &str,
        params: &Params,
        body: &[Stmt],
    ) -> Result<usize, String> {
        self.build_function_ex(name, params, body, false)
    }

    fn build_function_ex(
        &mut self,
        name: &str,
        params: &Params,
        body: &[Stmt],
        is_async: bool,
    ) -> Result<usize, String> {
        let mut fb = ChunkBuilder::new();
        self.compile_stmts(&mut fb, body)?;
        let is_generator = body_has_yield(body);
        let def = FuncDef {
            name: name.to_string(),
            params: params.names.clone(),
            posonly: params.posonly,
            ndefaults: params.defaults.len(),
            star: params.star.clone(),
            kwonly: params.kwonly.clone(),
            kwonly_required: params.kwonly_defaults.iter().map(|d| d.is_none()).collect(),
            kwargs: params.kwargs.clone(),
            chunk: fb.build(),
            is_generator,
            is_async,
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
        let def_id = self.build_function(&format!("<class {name}>"), &empty, body);
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
        // The context expression is evaluated EXACTLY ONCE into a synthetic temp;
        // `__enter__`/`__exit__` both dispatch on that temp. (Re-evaluating it —
        // as `with open(path,'w')` — would re-run the side effect, e.g. re-open and
        // truncate the file, discarding the writes.)
        let mut inner: Vec<Stmt> = Vec::new();
        let mut temps: Vec<String> = Vec::new();
        for it in items {
            let ctx_tmp = format!(".with{}", self.tmp);
            self.tmp += 1;
            temps.push(ctx_tmp.clone());
            // __with_ctx = <context expr>   (evaluated once)
            inner.push(
                StmtKind::Assign {
                    targets: vec![Expr::Name(ctx_tmp.clone())],
                    value: it.context.clone(),
                }
                .into(),
            );
            // x = __with_ctx.__enter__()
            let enter = Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(Expr::Name(ctx_tmp.clone())),
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
        // finally: __with_ctx.__exit__(None, None, None), in REVERSE (LIFO) order.
        let mut fin: Vec<Stmt> = Vec::new();
        for ctx_tmp in temps.iter().rev() {
            let exit = Expr::Call {
                func: Box::new(Expr::Attribute(
                    Box::new(Expr::Name(ctx_tmp.clone())),
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
                self.name_const(b, n);
                b.emit(Op::CallBuiltin(ops::GETLOCAL, 1), self.cur_line);
            }
            Expr::List(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    // BUILD_ARGS already yields a flat `list`.
                    self.compile_arg_spread(b, items)?;
                } else {
                    self.compile_seq(b, items)?;
                    b.emit(Op::CallBuiltin(ops::MKLIST, argc(items.len())?), 0);
                }
            }
            Expr::Tuple(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    // Flatten to a list, then convert to a tuple.
                    self.name_const(b, "tuple");
                    self.compile_arg_spread(b, items)?;
                    b.emit(Op::CallBuiltin(ops::CALL, 2), 0);
                } else {
                    self.compile_seq(b, items)?;
                    b.emit(Op::CallBuiltin(ops::MKTUPLE, argc(items.len())?), 0);
                }
            }
            Expr::Set(items) => {
                if items.iter().any(|e| matches!(e, Expr::Starred(_))) {
                    self.name_const(b, "set");
                    self.compile_arg_spread(b, items)?;
                    b.emit(Op::CallBuiltin(ops::CALL, 2), 0);
                } else {
                    self.compile_seq(b, items)?;
                    b.emit(Op::CallBuiltin(ops::MKSET, argc(items.len())?), 0);
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
                    for (k, v) in pairs {
                        if let Some(k) = k {
                            self.compile_expr(b, k)?;
                            self.compile_expr(b, v)?;
                        }
                    }
                    b.emit(Op::CallBuiltin(ops::MKDICT, argc(pairs.len() * 2)?), 0);
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
                b.emit(Op::CallBuiltin(ops::GETATTR, 2), self.cur_line);
            }
            Expr::Subscript(recv, idx) => {
                self.compile_expr(b, recv)?;
                self.compile_subscript_index(b, idx)?;
                b.emit(Op::CallBuiltin(ops::GETITEM, 2), self.cur_line);
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

    /// `yield from iterable` — drive the iterable, yielding each element.
    fn compile_yield_from(&mut self, b: &mut ChunkBuilder, iter: &Expr) -> Result<(), String> {
        self.compile_expr(b, iter)?;
        b.emit(Op::CallBuiltin(ops::GETITER, 1), 0); // [it]
        let start = b.current_pos();
        b.emit(Op::CallBuiltin(ops::FORITER, 0), 0); // [it, value, has_next]
        let jdone = b.emit(Op::JumpIfFalse(0), 0); // pops has_next -> [it, value]
        b.emit(Op::CallBuiltin(ops::YIELDV, 1), 0); // yield value -> [it, sent]
        b.emit(Op::Pop, 0); // drop sent -> [it]
        b.emit(Op::Jump(start), 0);
        let done = b.current_pos();
        b.patch_jump(jdone, done);
        // The `yield from` expression evaluates to the delegated iterator's
        // return value (its `StopIteration.value`) — `None` for a non-generator.
        b.emit(Op::CallBuiltin(ops::GENRET, 1), 0); // [it] -> retval
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
                    self.compile_fstring_spec(b, spec)?;
                    b.emit(Op::CallBuiltin(ops::FORMAT, 3), 0);
                    n += 1;
                }
            }
        }
        b.emit(Op::CallBuiltin(ops::MKSTR, argc(n)?), 0);
        Ok(())
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
                b.emit(Op::Add, self.cur_line);
                return Ok(());
            }
            BinOp::Sub => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                b.emit(Op::Sub, self.cur_line);
                return Ok(());
            }
            BinOp::Mul => {
                self.compile_expr(b, l)?;
                self.compile_expr(b, r)?;
                b.emit(Op::Mul, self.cur_line);
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
        b.emit(Op::CallBuiltin(ops::BINOP, 3), self.cur_line);
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
        match func {
            Expr::Attribute(recv, attr) => {
                self.compile_expr(b, recv)?;
                self.name_const(b, attr);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(
                        Op::CallBuiltin(ops::CALL_METHOD, argc(2 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    b.emit(
                        Op::CallBuiltin(ops::CALL_METHOD_KW, argc(3 + args.len())?),
                        self.cur_line,
                    );
                }
            }
            Expr::Name(n) => {
                self.name_const(b, n);
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(
                        Op::CallBuiltin(ops::CALL, argc(1 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    b.emit(
                        Op::CallBuiltin(ops::CALL_KW, argc(2 + args.len())?),
                        self.cur_line,
                    );
                }
            }
            _ => {
                self.compile_expr(b, func)?;
                self.compile_seq(b, args)?;
                if named.is_empty() {
                    b.emit(
                        Op::CallBuiltin(ops::CALL_VALUE, argc(1 + args.len())?),
                        self.cur_line,
                    );
                } else {
                    build_kw(self, b)?;
                    b.emit(
                        Op::CallBuiltin(ops::CALL_VALUE_KW, argc(2 + args.len())?),
                        self.cur_line,
                    );
                }
            }
        }
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
        match func {
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
        self.emit_comp_call(b, "<comp>", body, &comps[0].iter)
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
        self.emit_comp_call(b, "<genexpr>", body, &comps[0].iter)
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
    ) -> Result<(), String> {
        let params = Params {
            names: vec![".0".into()],
            ..Params::default()
        };
        let def_id = self.build_function(name, &params, &body)?;
        self.emit_make_func(b, def_id, &params)?; // [func]
        self.compile_expr(b, outer_iter)?; // [func, iterable]
        b.emit(Op::CallBuiltin(ops::CALL_VALUE, 2), 0);
        Ok(())
    }

    // ── match / case (PEP 634 structural pattern matching) ────────────────
    fn compile_match(
        &mut self,
        b: &mut ChunkBuilder,
        subject: &Expr,
        cases: &[MatchCase],
    ) -> Result<(), String> {
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
                b.emit(Op::NumEq, 0); // [bool]
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
            is_async: false,
        }
        .into()];
    }
    inner
}

/// Collect the names assigned by a walrus (`:=`) anywhere in `e`, without
/// descending into a nested scope (lambda / comprehension / genexpr), whose
/// walrus targets belong to that inner scope.
fn collect_walrus_targets(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::NamedExpr(target, value) => {
            if let Expr::Name(n) = &**target {
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
    matches!(e, Expr::Yield(_) | Expr::YieldFrom(_))
}

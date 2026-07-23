//! pythonrs — Python as a fusevm frontend.
//!
//! Pipeline: `lexer` → `parser` builds a Python AST → `compiler` lowers it to a
//! `fusevm::Chunk` (plus a table of function/lambda/class-body sub-chunks and
//! try-block chunks) → fusevm executes it, calling back into the `host` (through
//! registered builtins and the strict numeric hook) for every Python-specific
//! operation. There is no bespoke VM or JIT here — execution and codegen live in
//! fusevm.

pub mod aot;
pub mod aot_native;
pub mod ast;
pub mod async_rt;
pub mod banner;
pub mod builtins;
pub mod cache;
pub mod casefold;
pub mod cli;
pub mod compiler;
pub mod dap;
pub mod extensions;
#[cfg(feature = "stdlib-ffi")]
pub mod ffi;
pub mod host;
pub mod intercepts;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod repl;
pub mod rust_ffi;
pub mod stdlib;

pub use fusevm::Value;

/// Compile a source string to a runnable program.
pub fn compile(src: &str) -> Result<compiler::Program, String> {
    let stmts = parser::parse(src)?;
    compiler::compile(&stmts, false)
}

/// Compile with per-statement DAP line markers enabled (`python --dap`).
pub fn compile_debug(src: &str) -> Result<compiler::Program, String> {
    let stmts = parser::parse(src)?;
    compiler::compile(&stmts, true)
}

/// Compile one interactive REPL line in CPython "single" mode: a top-level
/// expression statement echoes its value through `sys.displayhook` (prints
/// `repr(value)` for non-`None` results and binds `_`). Not used for scripts.
pub fn compile_interactive(src: &str) -> Result<compiler::Program, String> {
    let stmts = parser::parse(src)?;
    compiler::compile_interactive(&stmts)
}

/// Rebase a freshly compiled program's func/try ids above those already loaded
/// on the host, then install its functions/tries and return the (rebased) main
/// chunk to run. Shared by the initial script run, each REPL line, and imports.
pub fn load_merged(mut prog: compiler::Program) -> fusevm::Chunk {
    let (func_off, try_off) = host::with_host(|h| h.program_offsets());
    // Register traceback-caret position tables before rebasing. Keys are the
    // pre-rebase `op_hash`, which `rebase_program` leaves untouched (it mutates
    // ops but not the stored hash), so runtime lookups by `vm.chunk.op_hash`
    // still match. Covers both fresh compiles and cache hits.
    for (op_hash, table) in &prog.positions {
        host::register_positions(*op_hash, table.clone());
    }
    compiler::rebase_program(&mut prog, func_off, try_off);
    let compiler::Program {
        main,
        functions,
        procs: _,
        tries,
        warnings: _,
        positions: _,
    } = prog;
    let funcs: Vec<host::FuncDef> = functions.into_iter().map(|(_, f)| f).collect();
    host::with_host(|h| h.load_program(funcs, tries));
    main
}

/// Run an already-compiled program on the current host.
pub fn run_compiled(prog: compiler::Program) -> Result<Value, String> {
    host::run_main(load_merged(prog))
}

/// Transparent bytecode cache: return the cached compiled `Program` for `src`
/// (skipping lex/parse/lower entirely), else compile it, store it in the
/// `~/.pythonrs/scripts.rkyv` shard, and return it. This runs on EVERY ordinary
/// `python foo.py` / `python -c` invocation, so scripts are rkyv-cached
/// automatically — not only under `--build`. Set `PYTHONRS_TRACE=1` to log
/// hit/miss to stderr (silent otherwise; normal runs print nothing).
pub fn compile_or_load(src: &str) -> Result<compiler::Program, String> {
    // `PYTHONRS_CACHE=0|false|no` (see `cache::cache_enabled`) turns the shard off
    // entirely — every run recompiles and nothing is stored. `--doctor` reports
    // this state, so the gate must be honored here or that report would lie.
    if !cache::cache_enabled() {
        return compile(src);
    }
    if let Some(prog) = cache::load(src) {
        if std::env::var_os("PYTHONRS_TRACE").is_some() {
            eprintln!(
                "pythonrs: cache HIT ({} ops, {} functions) — skipped lex/parse/lower",
                prog.main.ops.len(),
                prog.functions.len()
            );
        }
        return Ok(prog);
    }
    let prog = compile(src)?;
    let _ = cache::store(src, &prog);
    if std::env::var_os("PYTHONRS_TRACE").is_some() {
        eprintln!(
            "pythonrs: cache MISS — compiled + stored ({} ops, {} functions)",
            prog.main.ops.len(),
            prog.functions.len()
        );
    }
    Ok(prog)
}

/// Parse/load, compile, and run a Python source string on a fresh host. Runs as
/// the top-level `__main__` with a default `sys.argv` of `['']`.
pub fn eval_str(src: &str) -> Result<Value, String> {
    host::reset_host();
    host::init_runtime(vec![String::new()], None, src, "<string>", true);
    run_compiled(compile_or_load(src)?)
}

/// Read and run a `.py` file (transparently rkyv-cached — see `compile_or_load`).
pub fn eval_file(path: &str) -> Result<Value, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    host::reset_host();
    host::init_runtime(vec![path.to_string()], None, &src, path, true);
    run_compiled(compile_or_load(&src)?)
}

/// Run `python -m <module> [args…]`. Delegates to the embedded CPython's
/// `runpy` (the same code path as CPython's `-m`), so `-m pip`/`-m venv`/… run on
/// the real interpreter. Returns the process exit code. Only available with the
/// `stdlib-ffi` bridge; a native-only build has no interpreter to host `runpy`.
#[cfg(feature = "stdlib-ffi")]
pub fn run_module(module: &str, args: &[String]) -> i32 {
    ffi::run_module(module, args)
}

/// `-m` with no embedded interpreter (native-only `--no-default-features` build):
/// there is no `runpy` to run the module through, so report and exit non-zero.
#[cfg(not(feature = "stdlib-ffi"))]
pub fn run_module(module: &str, _args: &[String]) -> i32 {
    eprintln!("python: -m requires the stdlib-ffi bridge (not in this build): {module}");
    1
}

/// How a program run ended: the process exit code plus any text the runtime must
/// emit to stderr (a traceback block or a `SystemExit` message).
pub struct RunReport {
    pub exit_code: i32,
    pub stderr: Option<String>,
}

/// Run a top-level program with a fully specified CLI/runtime context and reduce
/// the outcome to a process exit code + stderr text (uncaught-exception traceback
/// or `SystemExit` handling). This is the entry the `python` binary uses so that
/// `sys.argv`, `__name__`/`__file__`, `sys.exit`, and tracebacks all behave like
/// CPython.
pub fn run_program(
    src: &str,
    argv: Vec<String>,
    main_file: Option<String>,
    tb_filename: &str,
    show_source: bool,
) -> RunReport {
    host::reset_host();
    host::init_runtime(argv, main_file, src, tb_filename, show_source);
    let prog = match compile_or_load(src) {
        Ok(p) => p,
        Err(e) => {
            return RunReport {
                exit_code: 1,
                stderr: Some(format!("{e}\n")),
            }
        }
    };
    // Compile-time `SyntaxWarning`s (e.g. `'return' in a 'finally' block`) print
    // before execution, matching CPython. Carried through the bytecode cache so a
    // cache hit warns identically to a fresh compile.
    for (line, msg) in &prog.warnings {
        eprintln!("{tb_filename}:{line}: SyntaxWarning: {msg}");
        // For a real file, CPython echoes the offending source line (via
        // linecache) indented two spaces; `-c`/`<stdin>` have no file to read.
        if !tb_filename.starts_with('<') {
            if let Some(text) = src.lines().nth((*line as usize).saturating_sub(1)) {
                eprintln!("  {}", text.trim_start());
            }
        }
    }
    let result = run_compiled(prog);
    // CPython emits `RuntimeWarning: coroutine '…' was never awaited` for any
    // coroutine that was created but never driven; do the same at teardown.
    host::warn_unawaited_coroutines();
    match result {
        Ok(_) => RunReport {
            exit_code: 0,
            stderr: None,
        },
        Err(e) => match host::classify_top_error(&e) {
            host::TopExit::SystemExit { code, message } => RunReport {
                exit_code: code,
                stderr: message,
            },
            host::TopExit::Uncaught { traceback } => RunReport {
                exit_code: 1,
                stderr: Some(traceback),
            },
        },
    }
}

/// Read and run a `.py` file under the DAP debugger.
pub fn eval_file_debug(path: &str) -> Result<Value, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let prog = compile_debug(&src)?;
    host::reset_host();
    host::set_debug_mode(true);
    let r = run_compiled(prog);
    host::set_debug_mode(false);
    r
}

/// Evaluate `src` and return the `repr` of the last expression's value.
pub fn eval_to_string(src: &str) -> Result<String, String> {
    let v = eval_str(src)?;
    Ok(host::with_host(|h| h.repr_of(&v)))
}

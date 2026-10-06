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
pub mod excgroup;
pub mod excunicode;
pub mod extensions;
#[cfg(feature = "stdlib-ffi")]
pub mod ffi;
pub mod host;
pub mod intercepts;
pub mod lexer;
pub mod lsp;
pub mod lsp_nav;
pub mod mangle;
pub mod oracle;
pub mod parser;
pub mod pyhash;
pub mod regexpr;
pub mod repl;
pub mod rust_ffi;
pub mod sre_parse;
pub mod stack;
pub mod stdio;
pub mod stdlib;
pub mod suggest;
pub mod symtable;
pub mod tiers;

pub use fusevm::Value;

/// Compile a source string to a runnable program.
pub fn compile(src: &str) -> Result<compiler::Program, String> {
    let (stmts, warnings) = parse_with_warnings(src)?;
    compiler::compile(&stmts, false)
        .map(|p| with_parse_warnings(p, warnings))
        .map_err(|e| parser::with_byte_columns(e, src))
}

/// `SyntaxWarning`s as `(line, message)`.
type Warnings = Vec<(u32, String)>;

/// Parse `src`, collecting the `SyntaxWarning`s the tokenizer and parser
/// raise along the way (invalid escapes), which precede the compiler's own.
fn parse_with_warnings(src: &str) -> Result<(Vec<ast::Stmt>, Warnings), String> {
    let _ = lexer::take_escape_warnings();
    let stmts = parser::parse(src);
    let warnings = lexer::take_escape_warnings();
    Ok((stmts?, warnings))
}

fn with_parse_warnings(mut prog: compiler::Program, mut warnings: Warnings) -> compiler::Program {
    warnings.append(&mut prog.warnings);
    prog.warnings = warnings;
    prog
}

/// Compile with per-statement DAP line markers enabled (`python --dap`).
pub fn compile_debug(src: &str) -> Result<compiler::Program, String> {
    let (stmts, warnings) = parse_with_warnings(src)?;
    compiler::compile(&stmts, true)
        .map(|p| with_parse_warnings(p, warnings))
        .map_err(|e| parser::with_byte_columns(e, src))
}

/// Compile one interactive REPL line in CPython "single" mode: a top-level
/// expression statement echoes its value through `sys.displayhook` (prints
/// `repr(value)` for non-`None` results and binds `_`). Not used for scripts.
pub fn compile_interactive(src: &str) -> Result<compiler::Program, String> {
    let stmts = parser::parse(src)?;
    compiler::compile_interactive(&stmts).map_err(|e| parser::with_byte_columns(e, src))
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
    compile_or_load_cacheable(src, true)
}

/// [`compile_or_load`] that labels a stored entry with `source` (its module
/// path) instead of the runtime's traceback filename. Imported modules use this
/// so `--cacheview` attributes each cached blob to the module that produced it,
/// not the `<string>`/script name of whatever run triggered the import.
pub fn compile_or_load_labeled(src: &str, source: &str) -> Result<compiler::Program, String> {
    // A syntax error in the module names the module's file.
    let compile = |src: &str| compile(src).map_err(|e| parser::with_filename(e, source));
    if !cache::cache_enabled() {
        return compile(src);
    }
    if let Some(prog) = cache::load(src) {
        return Ok(prog);
    }
    let prog = compile(src)?;
    let _ = cache::store_labeled(src, &prog, source);
    Ok(prog)
}

/// [`compile_or_load`] with an explicit cacheability gate. A `-c`/stdin program
/// (`cacheable == false`) is compiled fresh and NEVER touches the shard: such
/// one-off snippets are never re-run byte-for-byte, so caching them only bloats
/// the append-only shard (and forces an O(shard) rewrite per miss) for no hit —
/// CPython likewise never writes a `.pyc` for `-c`/stdin. Real script files and
/// imported modules stay cacheable.
pub fn compile_or_load_cacheable(src: &str, cacheable: bool) -> Result<compiler::Program, String> {
    // `PYTHONRS_CACHE=0|false|no` (see `cache::cache_enabled`) turns the shard off
    // entirely — every run recompiles and nothing is stored. `--doctor` reports
    // this state, so the gate must be honored here or that report would lie.
    if !cacheable || !cache::cache_enabled() {
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
    // A bare source string (`<string>`) is a throwaway program — don't cache it.
    let result = compile_or_load_cacheable(src, false).and_then(run_compiled);
    // The run is a whole interpreter lifetime: what it buffered on stdout
    // reaches the descriptor now, as `Py_FinalizeEx` would put it there.
    stdio::flush_std_files();
    result.map_err(plain_error)
}

/// An error as an embedder reads it: a syntax error's position trailer (see
/// [`parser::split_syntax_error`]) is for the runtime's own exception builder
/// and traceback, and comes off at the library boundary.
pub fn plain_error(e: String) -> String {
    match parser::split_syntax_error(&e) {
        (head, Some(_)) => head.to_string(),
        (_, None) => e,
    }
}

/// Run a Python source string on a fresh host with `globals` bound and the
/// program's output captured in-process, returning the program's outcome
/// alongside everything it wrote.
///
/// This is the entry point for an embedder rather than for the `python` binary,
/// and it exists because [`eval_str`] cannot serve one: it resets the host
/// first, which wipes any global installed beforehand, and it lets `print`
/// reach the real stdout, which corrupts a host that owns the terminal. Both
/// are fixed here — the globals are seeded *after* the reset, and every write
/// the program makes lands in the returned string.
///
/// The outcome and the output are returned separately (rather than the output
/// only on success) because a program that prints and *then* raises produced
/// both, and an embedder generally wants to show both.
///
/// Globals are given as text and interned as real Python `str` objects here.
/// They are deliberately *not* `Value`: a `Value::Str` built by a caller is not
/// a Python string — strings live on this host's heap as `PyObj::Str`, so a
/// bare `Value::Str` reprs correctly but has no methods (`stdin.upper()` finds
/// nothing). Handing the host text and letting it intern removes that trap.
///
/// ```no_run
/// let (result, out) = pythonrs::eval_str_captured("print(stdin.upper())", &[("stdin", "hi")]);
/// assert!(result.is_ok());
/// assert_eq!(out, "HI\n");
/// ```
pub fn eval_str_captured(src: &str, globals: &[(&str, &str)]) -> (Result<Value, String>, String) {
    host::reset_host();
    host::init_runtime(vec![String::new()], None, src, "<string>", true);
    host::with_host(|h| {
        for (name, text) in globals {
            let value = h.new_str(*text);
            h.set_global(name, value);
        }
        h.begin_capture();
    });
    let result = compile_or_load_cacheable(src, false)
        .and_then(run_compiled)
        .map_err(plain_error);
    let output = host::with_host(|h| h.end_capture());
    (result, output)
}

/// Read and run a `.py` file (transparently rkyv-cached — see `compile_or_load`).
pub fn eval_file(path: &str) -> Result<Value, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    host::reset_host();
    host::init_runtime(vec![path.to_string()], None, &src, path, true);
    let result = compile_or_load(&src).and_then(run_compiled);
    stdio::flush_std_files();
    result.map_err(plain_error)
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

/// A program that does not compile, reported the way CPython reports one: the
/// `File "…", line N` header, the offending line, a caret run under the
/// position, and `Class: message`. A message the compiler positioned only by
/// a ` (line N)` suffix gets the header and the line without carets; one with
/// no position at all is printed as it is.
pub fn render_compile_error(err: &str, src: &str, filename: &str) -> String {
    let (head, pos) = parser::split_syntax_error(err);
    let mut pos = match pos {
        Some(p) if p.lineno.is_some() => p,
        _ => {
            let Some((text, line)) = head
                .strip_suffix(')')
                .and_then(|h| h.rsplit_once(" (line "))
                .and_then(|(t, n)| n.parse::<i64>().ok().map(|n| (t, n)))
            else {
                return format!("{head}\n");
            };
            let pos = parser::SyntaxPos {
                lineno: Some(line),
                ..Default::default()
            };
            return render_positioned(text, pos, src, filename);
        }
    };
    if pos.filename.is_none() {
        pos.filename = Some(filename.to_string());
    }
    render_positioned(head, pos, src, filename)
}

/// [`render_compile_error`] once the position is known: the source line comes
/// from `src` when the error did not carry one.
fn render_positioned(head: &str, mut pos: parser::SyntaxPos, src: &str, filename: &str) -> String {
    // CPython's traceback reads a missing line from the FILE (`linecache`), so
    // `-c` code and `<stdin>` show none.
    if pos.text.is_none() && !filename.starts_with('<') {
        pos.text = pos.lineno.and_then(|l| parser::source_line(src, l, true));
    }
    format!("{}\n", parser::render_syntax_head(&pos, head, filename))
}

/// How a program run ended: the process exit code plus any text the runtime must
/// emit to stderr (a traceback block or a `SystemExit` message).
pub struct RunReport {
    pub exit_code: i32,
    pub stderr: Option<String>,
}

/// When the binary flushes the standard streams relative to reporting the
/// program's uncaught exception — the one place CPython's entry points differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportOrder {
    /// A script file or stdin (`_PyRun_SimpleFile`): `flush_io()` runs before
    /// `PyErr_Print`, so the program's buffered stdout precedes the traceback.
    FlushThenReport,
    /// `-c` (`pymain_run_command` → `_PyRun_SimpleString`): the traceback is
    /// printed first and buffered stdout only reaches the descriptor at
    /// shutdown, after it.
    ReportThenFlush,
}

/// Run a top-level program with a fully specified CLI/runtime context and reduce
/// the outcome to a process exit code + stderr text (uncaught-exception traceback
/// or `SystemExit` handling), running interpreter shutdown (`atexit`, stream
/// flushes) before returning. The text is returned rather than printed, so an
/// embedder or a test reads it; [`run_main_program`] is the binary's entry,
/// which prints it at the point CPython does.
pub fn run_program(
    src: &str,
    argv: Vec<String>,
    main_file: Option<String>,
    tb_filename: &str,
    show_source: bool,
) -> RunReport {
    let report = execute_program(src, argv, main_file, tb_filename, show_source);
    shut_down();
    report
}

/// The `python` binary's run: [`run_program`] with the report written to
/// stderr where CPython writes it — after the program's own buffered stdout for
/// a script, before it for `-c` (see [`ReportOrder`]) — and before `atexit`
/// callbacks run, since CPython prints the exception in `PyErr_Print` and runs
/// `atexit` later, in `Py_FinalizeEx`. Returns the exit code.
pub fn run_main_program(
    src: &str,
    argv: Vec<String>,
    main_file: Option<String>,
    tb_filename: &str,
    show_source: bool,
    order: ReportOrder,
) -> i32 {
    let report = execute_program(src, argv, main_file, tb_filename, show_source);
    if order == ReportOrder::FlushThenReport {
        stdio::flush_io();
    }
    if let Some(text) = &report.stderr {
        stdio::write(stdio::Stream::Stderr, text);
    }
    shut_down();
    report.exit_code
}

/// Compile and run `src` as `__main__`, classifying how it ended. Shutdown is
/// the caller's.
fn execute_program(
    src: &str,
    argv: Vec<String>,
    main_file: Option<String>,
    tb_filename: &str,
    show_source: bool,
) -> RunReport {
    host::reset_host();
    // A real script file (`main_file` set) is cacheable; a `-c`/stdin program is
    // not — it never re-runs byte-for-byte, so caching it only bloats the shard.
    let cacheable = main_file.is_some();
    host::init_runtime(argv, main_file, src, tb_filename, show_source);
    let prog = match compile_or_load_cacheable(src, cacheable) {
        Ok(p) => p,
        Err(e) => {
            return RunReport {
                exit_code: 1,
                stderr: Some(render_compile_error(&e, src, tb_filename)),
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
    match run_compiled(prog) {
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

/// Interpreter shutdown, in `Py_FinalizeEx`'s order: `atexit` callbacks, then
/// the remaining teardown output, then the standard streams are flushed.
fn shut_down() {
    // `atexit` callbacks run at interpreter shutdown, after the top-level program
    // finishes (whether it returned or raised), before teardown warnings.
    host::run_atexit_callbacks();
    // A CPython-side file the program left open is flushed here: the embedded
    // interpreter is never finalized, so nothing else would.
    #[cfg(feature = "stdlib-ffi")]
    ffi::flush_open_files();
    // CPython emits `RuntimeWarning: coroutine '…' was never awaited` for any
    // coroutine that was created but never driven; do the same at teardown.
    host::warn_unawaited_coroutines();
    stdio::flush_std_files();
}

/// Read and run a `.py` file under the DAP debugger.
pub fn eval_file_debug(path: &str) -> Result<Value, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let prog = compile_debug(&src).map_err(plain_error)?;
    host::reset_host();
    host::set_debug_mode(true);
    let r = run_compiled(prog);
    host::set_debug_mode(false);
    stdio::flush_std_files();
    r.map_err(plain_error)
}

/// Evaluate `src` and return the `repr` of the last expression's value.
pub fn eval_to_string(src: &str) -> Result<String, String> {
    let v = eval_str(src)?;
    Ok(host::with_host(|h| h.repr_of(&v)))
}

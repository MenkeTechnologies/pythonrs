//! The `python` binary entry point.
//!
//! Dispatch: `--lsp`/`--dap` speak their protocols over stdio; `--repl` (or no
//! file on a TTY) starts the interactive loop; `--build` AOT-compiles to a
//! standalone native executable; `--dump-bytecode` prints the lowered fusevm
//! chunk; otherwise a file or `-c` one-liner is run. Errors go to stderr in
//! terse `python: <reason>` form; nothing else is printed.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Run on a worker thread with a large stack. Each Python call frame recurses
    // through a deep chain of Rust functions (dispatch → call → run_user_func →
    // run_chunk_on → …), so the default 8 MiB main-thread stack overflows at only
    // ~85 Python frames. A 512 MiB stack reaches well past CPython's default
    // recursion limit (1000); a `RecursionError` guard (host::enter_call) stops
    // runaway recursion before even that is exhausted.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run_main)
        .expect("spawn interpreter thread")
        .join()
        .unwrap_or(ExitCode::FAILURE)
}

fn run_main() -> ExitCode {
    // CPython (`Modules/main.c::pymain_parse_cmdline`) stops interpreter-option
    // parsing at the FIRST of `-c` / `-m` / a script file / `--`; everything from
    // there is the program plus its own `sys.argv`, passed through verbatim (so
    // `script.py -u`, `-c '…' --flag`, `pip install --upgrade` all reach the
    // program, never python). clap parses options anywhere and can't model that,
    // so we split the raw args at the program boundary first, hand ONLY the
    // leading interpreter-flag region to clap, and own the program + its argv.
    let raw: Vec<String> = std::env::args().collect();
    let boundary = program_boundary(&raw);
    let cli = pythonrs::cli::parse_from(&raw[..boundary]);

    // Interpreter flags that affect the embedded runtime, set as env vars before
    // it starts (`ffi::init` reads them at `Py_Initialize`).
    if cli.unbuffered {
        std::env::set_var("PYTHONUNBUFFERED", "1");
    }
    if !cli.warnings.is_empty() {
        std::env::set_var("PYTHONWARNINGS", cli.warnings.join(","));
    }

    if cli.lsp {
        return match pythonrs::lsp::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    if cli.dap {
        return match pythonrs::dap::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }

    // Diagnostic/cache extensions (no program): report and exit before any script
    // parsing. Each returns a process exit code.
    if cli.doctor {
        return ExitCode::from((pythonrs::extensions::doctor::run() & 0xFF) as u8);
    }
    if cli.cacheview {
        return ExitCode::from((pythonrs::extensions::cacheview::run() & 0xFF) as u8);
    }
    if cli.cache_clear {
        return match pythonrs::cache::clear() {
            Ok(()) => {
                // Explicit, user-requested confirmation on stdout.
                println!("python: bytecode cache cleared ({})", pythonrs::cache::default_cache_path().display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("--cache-clear: {e}")),
        };
    }

    let (prog, args) = parse_program(&raw[boundary..]);
    match prog {
        // `python -m module …` → run the library module on the embedded CPython.
        Prog::Module(module) => {
            let code = pythonrs::run_module(&module, &args);
            ExitCode::from((code & 0xFF) as u8)
        }
        // `python -c '…' a b` → sys.argv == ['-c', 'a', 'b'].
        Prog::Command(src) => {
            let mut argv = vec!["-c".to_string()];
            argv.extend(args);
            emit(pythonrs::run_program(&src, argv, None, "<string>", true))
        }
        // `python - …` reads the script from stdin (argv[0] == '-').
        Prog::Stdin => {
            let src = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
            let mut argv = vec!["-".to_string()];
            argv.extend(args);
            emit(pythonrs::run_program(&src, argv, None, "<stdin>", false))
        }
        Prog::Script(file) => run_script(&cli, file, args),
        Prog::MissingArg(opt) => fail_usage(&format!("Argument expected for the {opt} option")),
        // No program: LSP/DAP handled above; a TTY starts the REPL, `--repl` over a
        // pipe runs the interactive loop on the piped source, else stdin is a script.
        Prog::None => {
            if atty_stdin() {
                pythonrs::repl::run();
                return ExitCode::SUCCESS;
            }
            if cli.repl {
                pythonrs::repl::run_piped();
                return ExitCode::SUCCESS;
            }
            // stdin-as-script: argv == [''].
            let src = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
            let mut argv = vec![String::new()];
            argv.extend(args);
            emit(pythonrs::run_program(&src, argv, None, "<stdin>", false))
        }
    }
}

/// Run a script file: honor `--dump-*`/`--disasm`/`--build` if requested, else
/// execute it with `sys.argv == [file, *args]`.
fn run_script(cli: &pythonrs::cli::Cli, file: String, args: Vec<String>) -> ExitCode {
    if cli.dump_bytecode {
        return match dump(&file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    if cli.dump_tokens {
        return match dump_tokens(&file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    if cli.dump_ast {
        return match dump_ast(&file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    if cli.disasm {
        return match disasm(&file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    if cli.build {
        return match pythonrs::aot::build(&file) {
            Ok(msg) => {
                // A build report is explicit user-requested output.
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        };
    }
    let src = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => return fail(&format!("cannot read {file}: {e}")),
    };
    // `__file__`/traceback use the absolute path; `sys.argv[0]` keeps the path as
    // typed. `python script.py a b` → sys.argv == ['script.py', 'a', 'b'].
    let abs = abs_path(&file);
    let mut argv = vec![file];
    argv.extend(args);
    emit(pythonrs::run_program(&src, argv, Some(abs.clone()), &abs, true))
}

/// What the command line asks python to run, after CPython's interpreter-option
/// parsing terminates at the program boundary.
enum Prog {
    /// `-c SRC` — a one-liner.
    Command(String),
    /// `-m MODULE` — a library module (runs on the embedded CPython).
    Module(String),
    /// A script file path.
    Script(String),
    /// `-` — read the script from stdin (`sys.argv[0] == '-'`).
    Stdin,
    /// `-c`/`-m` given with no following argument (`opt` is `-c`/`-m`).
    MissingArg(&'static str),
    /// No program token: REPL / LSP / DAP / stdin-as-script.
    None,
}

/// The index into `raw` where the program begins — the first of `-c` / `-m` / a
/// script file / `-` / `--`. Everything before is the interpreter-flag region;
/// everything from here is the program and its verbatim `sys.argv`. Mirrors
/// CPython's `pymain_parse_cmdline`: only interpreter options precede the program,
/// and the first bare token (or `-c`/`-m`/`-`/`--`) ends option parsing.
fn program_boundary(raw: &[String]) -> usize {
    let mut i = 1;
    while i < raw.len() {
        let a = &raw[i];
        // Program starters (separate or glued `-cSRC`/`-mMOD`), stdin `-`, and the
        // `--` end-of-options marker all begin the program region.
        if a == "-c"
            || a == "-m"
            || a == "-"
            || a == "--"
            || ((a.starts_with("-c") || a.starts_with("-m")) && a.len() > 2)
        {
            return i;
        }
        if a == "-W" {
            i += 2; // `-W <action>` consumes its value.
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            i += 1; // an interpreter flag (short `-u/-E/-I/-O/-S/-B`, long `--repl`…).
            continue;
        }
        return i; // a bare token — the script path.
    }
    raw.len()
}

/// Classify the program region (`raw[boundary..]`) into a [`Prog`] plus its
/// verbatim argv (everything after the program token). CPython's `--` consumes
/// itself, then the next token is the script.
fn parse_program(region: &[String]) -> (Prog, Vec<String>) {
    let Some(first) = region.first() else {
        return (Prog::None, Vec::new());
    };
    let rest = |n: usize| region.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    match first.as_str() {
        "-c" => match region.get(1) {
            Some(src) => (Prog::Command(src.clone()), rest(2)),
            None => (Prog::MissingArg("-c"), Vec::new()),
        },
        "-m" => match region.get(1) {
            Some(m) => (Prog::Module(m.clone()), rest(2)),
            None => (Prog::MissingArg("-m"), Vec::new()),
        },
        "-" => (Prog::Stdin, rest(1)),
        "--" => match region.get(1) {
            Some(f) => (Prog::Script(f.clone()), rest(2)),
            None => (Prog::None, Vec::new()),
        },
        s if s.starts_with("-c") => (Prog::Command(s[2..].to_string()), rest(1)),
        s if s.starts_with("-m") => (Prog::Module(s[2..].to_string()), rest(1)),
        s => (Prog::Script(s.to_string()), rest(1)),
    }
}

/// A command-line usage error: terse message on stderr, exit 2 (CPython's
/// usage-error code, e.g. `-c`/`-m` with no argument).
fn fail_usage(msg: &str) -> ExitCode {
    eprintln!("python: {msg}");
    ExitCode::from(2)
}

/// Emit a run's stderr text (traceback / `SystemExit` message) and reduce its
/// exit code to a process `ExitCode` (masked to 8 bits like the OS does).
fn emit(report: pythonrs::RunReport) -> ExitCode {
    if let Some(s) = &report.stderr {
        eprint!("{s}");
    }
    ExitCode::from((report.exit_code & 0xFF) as u8)
}

/// CPython's `__file__` rule: an absolute path is kept; a relative one is joined
/// onto the cwd without normalizing (so `./x.py` stays `<cwd>/./x.py`).
fn abs_path(file: &str) -> String {
    let p = std::path::Path::new(file);
    if p.is_absolute() {
        return file.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(file).to_string_lossy().into_owned(),
        Err(_) => file.to_string(),
    }
}

fn dump(file: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let prog = pythonrs::compile(&src)?;
    println!("== main ==\n{:#?}", prog.main.ops);
    for (name, m) in &prog.functions {
        println!(
            "== def {name} ({}) ==\n{:#?}",
            m.params.join(", "),
            m.chunk.ops
        );
    }
    for (i, p) in prog.procs.iter().enumerate() {
        println!("== block #{i} ==\n{:#?}", p.chunk.ops);
    }
    Ok(())
}

/// `--dump-tokens`: print the lexer token stream, one `line\tTok` per line.
/// Python is indentation-sensitive, so INDENT/DEDENT/NEWLINE tokens are printed
/// as emitted.
fn dump_tokens(file: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    for t in pythonrs::lexer::lex(&src)? {
        println!("{}\t{:?}", t.line, t.tok);
    }
    Ok(())
}

/// `--dump-ast`: print the parsed Python AST.
fn dump_ast(file: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let stmts = pythonrs::parser::parse(&src)?;
    println!("{stmts:#?}");
    Ok(())
}

/// `--disasm`: print a fusevm bytecode disassembly of the main chunk and every
/// compiled function/block, via the shared `fusevm::Chunk::disassemble`.
fn disasm(file: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let prog = pythonrs::compile(&src)?;
    println!("; python fusevm — main\n{}", prog.main.disassemble());
    for (name, m) in &prog.functions {
        println!(
            "; python fusevm — def {name}({})\n{}",
            m.params.join(", "),
            m.chunk.disassemble()
        );
    }
    for (i, p) in prog.procs.iter().enumerate() {
        println!("; python fusevm — block #{i}\n{}", p.chunk.disassemble());
    }
    Ok(())
}

fn atty_stdin() -> bool {
    // SAFETY: isatty is a pure query on the stdin fd.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("python: {msg}");
    ExitCode::FAILURE
}

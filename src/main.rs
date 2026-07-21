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
    let cli = pythonrs::cli::parse();

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

    if let Some(src) = cli.eval {
        // `python -c '…' a b` → sys.argv == ['-c', 'a', 'b']. With `-c` present the
        // first trailing token is a program arg, not a script path, so clap's
        // `file` slot (if filled) belongs at the front of the passthrough args.
        let mut argv = vec!["-c".to_string()];
        argv.extend(cli.file);
        argv.extend(cli.argv);
        return emit(pythonrs::run_program(&src, argv, None, "<string>", true));
    }

    if let Some(file) = cli.file {
        // `python - …` reads the script from stdin (argv[0] == '-').
        if file == "-" {
            let src = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
            let mut argv = vec!["-".to_string()];
            argv.extend(cli.argv);
            return emit(pythonrs::run_program(&src, argv, None, "<stdin>", false));
        }
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
        // `__file__`/traceback use the absolute path; `sys.argv[0]` keeps the path
        // as typed. `python script.py a b` → sys.argv == ['script.py', 'a', 'b'].
        let abs = abs_path(&file);
        let mut argv = vec![file.clone()];
        argv.extend(cli.argv);
        return emit(pythonrs::run_program(
            &src,
            argv,
            Some(abs.clone()),
            &abs,
            true,
        ));
    }

    if atty_stdin() {
        pythonrs::repl::run();
        return ExitCode::SUCCESS;
    }
    // `--repl` with a piped (non-TTY) stdin: run the interactive loop over the
    // piped source (CPython's `python3 -i < file`), so REPL echo is observable
    // and testable without a terminal.
    if cli.repl {
        pythonrs::repl::run_piped();
        return ExitCode::SUCCESS;
    }

    // No file and non-interactive stdin: run stdin as a script (argv == ['']).
    let src = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
    let mut argv = vec![String::new()];
    argv.extend(cli.argv);
    emit(pythonrs::run_program(&src, argv, None, "<stdin>", false))
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

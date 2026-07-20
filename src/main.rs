//! The `python` binary entry point.
//!
//! Dispatch: `--lsp`/`--dap` speak their protocols over stdio; `--repl` (or no
//! file on a TTY) starts the interactive loop; `--build` AOT-compiles to a
//! standalone native executable; `--dump-bytecode` prints the lowered fusevm
//! chunk; otherwise a file or `-c` one-liner is run. Errors go to stderr in
//! terse `python: <reason>` form; nothing else is printed.

use std::process::ExitCode;

fn main() -> ExitCode {
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
        return run_source(&src, "-c");
    }

    if let Some(file) = cli.file {
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
        return match pythonrs::eval_file(&file) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }

    if cli.repl || atty_stdin() {
        pythonrs::repl::run();
        return ExitCode::SUCCESS;
    }

    // No file and non-interactive stdin: run stdin as a script.
    let src = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
    run_source(&src, "stdin")
}

fn run_source(src: &str, _label: &str) -> ExitCode {
    match pythonrs::eval_str(src) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
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

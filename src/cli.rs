//! Command-line interface for the `python` binary.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "python",
    version,
    about = "Python on fusevm — a compiled Python runtime (bytecode VM + Cranelift JIT)",
    long_about = None,
)]
pub struct Cli {
    /// Execute a one-liner instead of a file (`python -c 'print(1+1)'`).
    #[arg(short = 'c', long = "command", value_name = "SRC")]
    pub eval: Option<String>,

    /// Run a library module as a script (`python -m pip …`); delegates to the
    /// embedded CPython (`runpy`), so `-m pip`/`-m venv`/`-m http.server` behave
    /// exactly like `python3 -m`. Trailing tokens become the module's `sys.argv`.
    #[arg(short = 'm', value_name = "MODULE")]
    pub module: Option<String>,

    /// Force stdout/stderr unbuffered (CPython `-u` / `PYTHONUNBUFFERED`).
    #[arg(short = 'u')]
    pub unbuffered: bool,

    /// Ignore `PYTHON*` environment variables (CPython `-E`). Accepted for
    /// drop-in compatibility.
    #[arg(short = 'E')]
    pub ignore_env: bool,

    /// Run in isolated mode (CPython `-I`). Accepted for drop-in compatibility.
    #[arg(short = 'I')]
    pub isolated: bool,

    /// No `site` import (CPython `-S`). Accepted for drop-in compatibility.
    #[arg(short = 'S')]
    pub no_site: bool,

    /// Don't write bytecode caches (CPython `-B`). Accepted for drop-in
    /// compatibility.
    #[arg(short = 'B')]
    pub no_bytecode: bool,

    /// Optimization level (CPython `-O` / `-OO`). Accepted for drop-in
    /// compatibility.
    #[arg(short = 'O', action = clap::ArgAction::Count)]
    pub optimize: u8,

    /// Warning filter(s) (CPython `-W <action>`). Passed to the embedded
    /// interpreter via `PYTHONWARNINGS`.
    #[arg(short = 'W', value_name = "ARG")]
    pub warnings: Vec<String>,

    /// Start the interactive REPL.
    #[arg(long = "repl")]
    pub repl: bool,

    /// Speak the Language Server Protocol over stdio.
    #[arg(long = "lsp")]
    pub lsp: bool,

    /// Speak the Debug Adapter Protocol over stdio.
    #[arg(long = "dap")]
    pub dap: bool,

    /// Ahead-of-time compile the script to a standalone native executable.
    #[arg(long = "build")]
    pub build: bool,

    /// Print the compiled fusevm bytecode for the script and exit.
    #[arg(long = "dump-bytecode")]
    pub dump_bytecode: bool,

    /// Print the lexer token stream for the script and exit.
    #[arg(long = "dump-tokens")]
    pub dump_tokens: bool,

    /// Print the parsed AST for the script and exit.
    #[arg(long = "dump-ast")]
    pub dump_ast: bool,

    /// Print a fusevm bytecode disassembly listing for the script and exit.
    #[arg(long = "disasm")]
    pub disasm: bool,

    /// The `.py` script to run (omit with --repl / --lsp / --dap / -c).
    #[arg(value_name = "FILE")]
    pub file: Option<String>,

    /// Arguments passed through to the Python program as `sys.argv`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

/// Parse the process arguments.
pub fn parse() -> Cli {
    Cli::parse()
}

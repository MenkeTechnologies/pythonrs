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

    // NOTE: `-m` is intentionally NOT a clap field. It's intercepted from the raw
    // args in `main.rs::find_dash_m` before clap runs, because CPython's `-m`
    // terminates interpreter-option parsing (every token after the module is the
    // module's verbatim `sys.argv`) — a contract clap can't model. A clap `-m`
    // field would also wrongly capture a `-m` that is a *program* argument
    // (`python foo.py -m bar`, `python -c '…' -m x`), dropping it from `sys.argv`.

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

/// Parse a specific argument slice (the leading interpreter-flag region, with
/// `args[0]` the program name). Used after the raw args are split at the program
/// boundary so clap only ever sees interpreter options — never the program's own
/// args. `--help`/`--version`/a bad flag are handled by clap here (print + exit).
pub fn parse_from(args: &[String]) -> Cli {
    Cli::parse_from(args)
}

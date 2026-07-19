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

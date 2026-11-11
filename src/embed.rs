//! Embedding entry point: run a Python program from a host and capture its output.

use crate::host::{self, StdinSource, TopExit};

/// Run `src` as a complete program with `input` as its stdin and return
/// everything it wrote to stdout.
///
/// `input` is what `input()`, `sys.stdin.read()`/`readline()` and iterating
/// `sys.stdin` see. Each call runs on its own thread (with the interpreter's
/// 512 MiB stack reservation) and a fresh host; stdout goes to that host's
/// capture buffer ([`crate::eval_str_captured`]), so concurrent calls never share
/// output and nothing reaches the process stdout. Writes to `sys.stderr` go to the
/// process stderr, not the returned string (unlike [`crate::eval_str_captured`],
/// which returns both streams merged).
///
/// A syntax error, an uncaught exception (its traceback is the message), a
/// nonzero `sys.exit`, and a panic in the interpreter return `Err`.
/// `sys.exit()` / `sys.exit(0)` returns `Ok` with the output printed so far.
/// `atexit` callbacks are not run.
pub fn run_program(src: &str, input: &str) -> Result<String, String> {
    let src = src.to_owned();
    let stdin = StdinSource::Fed(std::io::Cursor::new(input.as_bytes().to_vec()));
    std::thread::Builder::new()
        .name("pythonrs-embed".into())
        .stack_size(crate::stack::FRONTEND_STACK)
        .spawn(move || run_captured(&src, stdin))
        .map_err(|e| format!("spawn interpreter thread: {e}"))?
        .join()
        .unwrap_or_else(|panic| Err(panic_message(panic.as_ref())))
}

fn run_captured(src: &str, stdin: StdinSource) -> Result<String, String> {
    let (result, output) = crate::eval_captured(src, &[], stdin, false);
    let Err(e) = result else {
        return Ok(output);
    };
    match crate::tbcolor::with(false, || host::classify_top_error(&e)) {
        TopExit::SystemExit { code: 0, .. } => Ok(output),
        TopExit::SystemExit { code, message } => Err(message.map_or_else(
            || format!("SystemExit: {code}"),
            |m| m.trim_end().to_string(),
        )),
        TopExit::Uncaught { traceback } => Err(traceback.trim_end().to_string()),
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    let msg = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic");
    format!("interpreter panicked: {msg}")
}

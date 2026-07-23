//! `python -m <module>` delegation to the embedded CPython (`runpy`). Drives the
//! built `python` binary as a subprocess (`CARGO_BIN_EXE_python`) so the full CLI
//! path — raw `-m` interception, `runpy._run_module_as_main`, piped-stdout flush,
//! and exit-code propagation — is exercised end to end. Requires the `stdlib-ffi`
//! bridge (the embedded interpreter that hosts `runpy`); compiled out otherwise.
#![cfg(feature = "stdlib-ffi")]

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `python <args...>` with optional stdin, returning `(stdout, stderr, code)`.
fn run(args: &[&str], stdin: Option<&str>) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_python"));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn python binary");
    if let Some(text) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait python binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn dash_m_json_tool_passes_module_flags_and_flushes_piped_stdout() {
    // `--sort-keys` must reach json.tool (not be parsed as a python option), and
    // the module's block-buffered stdout must be flushed before exit even though
    // the interpreter is never finalized. Both are the point of the -m path.
    let (out, err, code) = run(
        &["-m", "json.tool", "--sort-keys"],
        Some(r#"{"b": 2, "a": 1}"#),
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "{\n    \"a\": 1,\n    \"b\": 2\n}\n");
}

#[test]
fn dash_m_positional_args_reach_the_module() {
    // `calendar 2025 1` — the year/month positionals must arrive as the module's
    // sys.argv[1:].
    let (out, err, code) = run(&["-m", "calendar", "2025", "1"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("January"), "got: {out:?}");
    assert!(out.contains("2025"), "got: {out:?}");
}

#[test]
fn dash_m_runnable_stdlib_module_executes() {
    // `-m this` runs the module's top-level code (prints the Zen), proving runpy
    // executes the module body, not just imports it.
    let (out, _err, code) = run(&["-m", "this"], None);
    assert_eq!(code, 0);
    assert!(out.contains("The Zen of Python"), "got: {out:?}");
}

#[test]
fn dash_m_missing_module_exits_nonzero_like_cpython() {
    let (_out, err, code) = run(&["-m", "no_such_module_xyz"], None);
    assert_eq!(code, 1);
    assert!(
        err.contains("No module named"),
        "expected a 'No module named' error, got: {err:?}"
    );
}

#[test]
fn dash_m_leading_interpreter_flags_are_accepted() {
    // `-E -I -u` before `-m` must not error (drop-in tolerance) and must not
    // swallow the module or its args.
    let (out, err, code) = run(&["-E", "-I", "-u", "-m", "this"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("The Zen of Python"), "got: {out:?}");
}

#[test]
fn passthrough_flags_do_not_break_c_eval() {
    // The accepted CPython flags must coexist with `-c`.
    let (out, err, code) = run(&["-OO", "-S", "-B", "-c", "print(6 * 7)"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), "42");
}

// ── Program-boundary / argv-termination parity with CPython ──────────────────
// CPython stops interpreter-option parsing at the first of `-c`/`-m`/script/`--`;
// every token after belongs to the program's sys.argv, even tokens that look like
// python's own flags. These guard against the regression where clap kept parsing
// `-u`/`-m`/etc. past the program boundary and dropped them from sys.argv.

/// Write a `print(sys.argv)` script to a temp file and return its path (kept
/// alive by the returned handle).
fn argv_script() -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".py")
        .tempfile()
        .expect("temp file");
    f.write_all(b"import sys\nprint(sys.argv)\n").expect("write");
    f
}

#[test]
fn script_args_that_look_like_python_flags_reach_sys_argv() {
    let f = argv_script();
    let path = f.path().to_str().unwrap().to_string();
    // `-u -E extra` are the SCRIPT's argv, not python's — python stopped parsing
    // options at the script path.
    let (out, err, code) = run(&[&path, "-u", "-E", "extra"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), format!("['{path}', '-u', '-E', 'extra']"));
}

#[test]
fn dash_m_as_a_program_arg_is_not_module_mode() {
    let f = argv_script();
    let path = f.path().to_str().unwrap().to_string();
    // `-m bar` AFTER a script path is the script's argv, not `python -m`.
    let (out, err, code) = run(&[&path, "-m", "bar"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), format!("['{path}', '-m', 'bar']"));
}

#[test]
fn c_eval_argv_keeps_flaglike_tokens() {
    // `python -c CODE -m x` → sys.argv == ['-c', '-m', 'x'].
    let (out, err, code) = run(&["-c", "import sys; print(sys.argv)", "-m", "x"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), "['-c', '-m', 'x']");
}

#[test]
fn double_dash_ends_option_parsing() {
    let f = argv_script();
    let path = f.path().to_str().unwrap().to_string();
    // `python -- script.py -u` → script with argv [script, '-u'].
    let (out, err, code) = run(&["--", &path, "-u"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), format!("['{path}', '-u']"));
}

#[test]
fn missing_c_and_m_args_exit_2() {
    let (_o, _e, c1) = run(&["-c"], None);
    assert_eq!(c1, 2, "`-c` with no argument should exit 2");
    let (_o2, _e2, c2) = run(&["-m"], None);
    assert_eq!(c2, 2, "`-m` with no argument should exit 2");
}

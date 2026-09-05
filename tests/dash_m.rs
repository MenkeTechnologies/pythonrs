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

/// Write `src` to a temp `.py` file and return the handle (which owns the path
/// and keeps the file alive for as long as the caller holds it).
fn script_file(src: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".py")
        .tempfile()
        .expect("temp file");
    f.write_all(src).expect("write");
    f
}

/// Write a `print(sys.argv)` script to a temp file and return its path (kept
/// alive by the returned handle).
fn argv_script() -> tempfile::NamedTempFile {
    script_file(b"import sys\nprint(sys.argv)\n")
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

#[test]
fn cpython_side_stdout_interleaves_with_pythonrs_print() {
    // Two writers share fd 1: pythonrs's own `print` (straight to the fd) and the
    // embedded interpreter's `sys.stdout`. A pythonrs builtin handed to CPython
    // crosses as the GENUINE CPython builtin, so `functools.partial(print, …)` and
    // `ExitStack.callback(print, …)` write through CPython's stream. That stream
    // is block-buffered on a pipe and the interpreter is never finalized, so its
    // output used to come out reordered — or, with nothing after it to force a
    // flush, be dropped at exit entirely. Piped stdout is the failing case, which
    // is exactly what this harness gives it.
    let prog = concat!(
        "import contextlib, functools\n",
        "print('a')\n",
        "functools.partial(print, 'b')()\n",
        "print('c')\n",
        "with contextlib.ExitStack() as st:\n",
        "    st.callback(print, 'cb1')\n",
        "    st.callback(print, 'cb2')\n",
        "print('d')\n",
    );
    let (out, err, code) = run(&["-c", prog], None);
    assert_eq!(code, 0, "stderr: {err}");
    // CPython 3.14 for the same program, in program order (callbacks are LIFO).
    assert_eq!(out, "a\nb\nc\ncb2\ncb1\nd\n");
}

#[test]
fn cpython_side_stdout_is_not_dropped_at_exit() {
    // The write CPython owns is the LAST thing the program does: nothing after it
    // can force a flush, so only per-line flushing keeps it.
    let (out, err, code) = run(
        &[
            "-c",
            "import functools\nfunctools.partial(print, 'last')()\n",
        ],
        None,
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "last\n");
}

#[test]
fn bridged_argparse_parses_the_program_arguments() {
    // argparse runs on the embedded CPython and reads ITS `sys.argv` — a list
    // libpython builds at startup as `['']`, because nothing hands the program's
    // arguments to `Py_Initialize`. Unmirrored, this failed SILENTLY: no error,
    // no traceback, just every option at its default and every positional gone.
    let f = script_file(
        b"import argparse\n\
          p = argparse.ArgumentParser()\n\
          p.add_argument('--input', default='DEFAULT')\n\
          p.add_argument('rest', nargs='*')\n\
          a = p.parse_args()\n\
          print(a.input, a.rest)\n",
    );
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path, "--input", "GIVEN", "x", "y"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), "GIVEN ['x', 'y']");
}

#[test]
fn argparse_sees_an_argv_the_program_rewrote() {
    // The mirror is refreshed on every bridged import, so the argv argparse reads
    // is the one pythonrs holds when argparse is imported — not a startup
    // snapshot. A program that builds its own argv (a test runner, a REPL driver)
    // depends on this.
    let f = script_file(
        b"import sys\n\
          sys.argv = ['prog', '--input', 'REWRITTEN']\n\
          import argparse\n\
          p = argparse.ArgumentParser()\n\
          p.add_argument('--input', default='DEFAULT')\n\
          print(p.parse_args().input)\n",
    );
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path, "--input", "ORIGINAL"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), "REWRITTEN");
}

#[test]
fn an_unclosed_bridged_file_is_flushed_at_exit() {
    // `io.open` hands back a CPython stream, which is block-buffered. The embedded
    // interpreter is never finalized, so without an explicit flush at teardown a
    // handle the program never closed lost every byte written to it — the file was
    // left on disk at the zero length `open` truncated it to, silently.
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("unclosed.txt");
    let f = script_file(
        format!(
            "import io\nh = io.open('{}', 'w')\nh.write('payload')\n",
            target.display()
        )
        .as_bytes(),
    );
    let path = f.path().to_str().unwrap().to_string();
    let (_, err, code) = run(&[&path], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(
        std::fs::read_to_string(&target).expect("the file the program wrote"),
        "payload"
    );
}

#[test]
fn a_dropped_bridged_file_still_reaches_disk() {
    // The handle is unreachable the instant the statement ends. CPython would
    // flush it at refcount zero; pythonrs holds every `Foreign` in the side-table
    // for the process lifetime, so the exit flush is what saves this one.
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("dropped.txt");
    let f = script_file(
        format!(
            "import io\nio.open('{}', 'w').write('payload')\n",
            target.display()
        )
        .as_bytes(),
    );
    let path = f.path().to_str().unwrap().to_string();
    let (_, err, code) = run(&[&path], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(
        std::fs::read_to_string(&target).expect("the file the program wrote"),
        "payload"
    );
}

/// A script whose `argparse` parser has one option, so `--help` and an unknown
/// flag both end the program from inside bridged CPython code.
fn argparse_exit_script() -> tempfile::NamedTempFile {
    script_file(
        b"import argparse\n\
          p = argparse.ArgumentParser(prog='tool')\n\
          p.add_argument('--input', default='D')\n\
          p.parse_args()\n\
          print('reached body')\n",
    )
}

#[test]
fn bridged_help_exits_zero_like_cpython() {
    // `--help` ends the program with `SystemExit(0)` raised by argparse — on the
    // CPython side, where it crosses the bridge as an error string rather than a
    // `PyObj::Exception`. Reported as an ordinary uncaught exception it exited 1
    // and printed a traceback after the help text.
    let f = argparse_exit_script();
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path, "--help"], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.starts_with("usage: tool [-h] [--input INPUT]"),
        "help text on stdout, got: {out:?}"
    );
    assert!(
        !out.contains("reached body"),
        "the body must not run: {out:?}"
    );
    assert!(!err.contains("Traceback"), "no traceback, got: {err:?}");
}

#[test]
fn bridged_usage_error_exits_two_like_cpython() {
    // An unrecognized argument is `SystemExit(2)` from argparse. CPython prints
    // usage plus one error line and exits 2; the traceback that used to follow was
    // pythonrs's, and the exit code was 1.
    let f = argparse_exit_script();
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path, "--bogus"], None);
    assert_eq!(code, 2, "stdout: {out:?} stderr: {err:?}");
    assert!(
        err.trim_end()
            .ends_with("tool: error: unrecognized arguments: --bogus"),
        "the error line is the last thing on stderr, got: {err:?}"
    );
    assert!(!err.contains("Traceback"), "no traceback, got: {err:?}");
}

#[test]
fn a_bridged_system_exit_carries_its_code() {
    // The general shape behind both tests above: any `SystemExit` raised by
    // bridged CPython code sets the process status, exactly as pythonrs's own
    // `sys.exit` does.
    let f = script_file(
        b"import argparse\n\
          p = argparse.ArgumentParser(prog='tool')\n\
          p.exit(7)\n",
    );
    let path = f.path().to_str().unwrap().to_string();
    let (_, err, code) = run(&[&path], None);
    assert_eq!(code, 7, "stderr: {err:?}");
    assert!(!err.contains("Traceback"), "no traceback, got: {err:?}");
}

#[test]
fn an_exception_from_pythonrs_code_keeps_its_class_for_a_cpython_caller() {
    // A pythonrs function invoked BY CPython (a `key=` callback, a `json.dumps`
    // default, a unittest test method) used to have every exception rewritten to
    // `RuntimeError: KeyError: 'k'`, so a caller's `except KeyError` never fired.
    let f = script_file(b"import functools\ndef raises_key():\n    raise KeyError('k')\ntry:\n    functools.partial(raises_key)()\nexcept KeyError as e:\n    print('KeyError', e.args)\nexcept Exception as e:\n    print('WRONG', type(e).__name__)\n");
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.trim(), "KeyError ('k',)");
}

#[test]
fn unittest_reports_a_failed_assertion_as_a_failure_not_an_error() {
    // `unittest` branches on the exception class: an `AssertionError` is a FAIL,
    // anything else is an ERROR. Arriving as `RuntimeError` put every failed
    // assertion in the wrong column, and the count a CI gate reads with it.
    let f = script_file(b"import unittest, sys\nclass T(unittest.TestCase):\n    def test_fail(self):\n        self.assertEqual(1, 2)\nres = unittest.TextTestRunner(stream=sys.stdout, verbosity=0).run(\n    unittest.TestLoader().loadTestsFromTestCase(T))\nprint('failures', len(res.failures), 'errors', len(res.errors))\n");
    let path = f.path().to_str().unwrap().to_string();
    let (out, err, code) = run(&[&path], None);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("FAIL: test_fail"),
        "expected a FAIL line, got: {out}"
    );
    assert!(
        out.contains("AssertionError: 1 != 2"),
        "the assertion message survives: {out}"
    );
    assert!(
        out.trim_end().ends_with("failures 1 errors 0"),
        "counted as a failure, not an error: {out}"
    );
}

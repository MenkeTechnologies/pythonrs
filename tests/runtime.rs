//! Headless tests for the Tier-0 runtime surface: `sys.argv`, `__name__`/
//! `__file__`, `sys.exit`/`SystemExit` exit codes, the `sys` module completeness,
//! and uncaught-exception tracebacks. Expected values are what CPython 3.14
//! produces for the same program/invocation.

use pythonrs::{eval_str, host, run_program};

/// Run `src` as a top-level program with the given argv, then return the `repr`
/// of global `name`.
fn g_with_argv(src: &str, argv: &[&str], name: &str) -> String {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let r = run_program(src, argv, None, "<string>", true);
    assert_eq!(
        r.exit_code, 0,
        "program should exit 0, got stderr {:?}",
        r.stderr
    );
    host::with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

/// Run `src`, then return the `repr` of global `name`.
fn g(src: &str, name: &str) -> String {
    eval_str(src).expect("program should run without error");
    host::with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

#[test]
fn sys_argv_command_form() {
    // `python -c '…' a b` → sys.argv == ['-c', 'a', 'b'].
    assert_eq!(
        g_with_argv("import sys\nx = sys.argv", &["-c", "a", "b"], "x"),
        "['-c', 'a', 'b']"
    );
}

#[test]
fn sys_argv_script_form() {
    // `python script.py x y` → sys.argv[0] is the path as typed.
    assert_eq!(
        g_with_argv("import sys\nx = sys.argv", &["script.py", "x", "y"], "x"),
        "['script.py', 'x', 'y']"
    );
}

#[test]
fn name_is_main_at_top_level() {
    assert_eq!(g("n = __name__", "n"), "'__main__'");
}

#[test]
fn name_main_guard_runs() {
    // The canonical entry idiom must fire at the top level.
    assert_eq!(
        g(
            "ran = False\nif __name__ == '__main__':\n    ran = True",
            "ran"
        ),
        "True"
    );
}

#[test]
fn file_global_set_for_file_run() {
    let r = run_program(
        "x = __file__",
        vec!["s.py".into()],
        Some("/tmp/s.py".into()),
        "/tmp/s.py",
        true,
    );
    assert_eq!(r.exit_code, 0);
    let repr = host::with_host(|h| h.repr_of(&h.read_global("x").unwrap()));
    assert_eq!(repr, "'/tmp/s.py'");
}

#[test]
fn sys_exit_int_sets_exit_code() {
    let r = run_program(
        "import sys\nsys.exit(3)",
        vec![String::new()],
        None,
        "<string>",
        true,
    );
    assert_eq!(r.exit_code, 3);
    assert_eq!(r.stderr, None);
}

#[test]
fn sys_exit_string_writes_message_exit_1() {
    let r = run_program(
        "import sys\nsys.exit('bad')",
        vec![String::new()],
        None,
        "<string>",
        true,
    );
    assert_eq!(r.exit_code, 1);
    assert_eq!(r.stderr.as_deref(), Some("bad\n"));
}

#[test]
fn sys_exit_none_is_zero() {
    let r = run_program(
        "import sys\nsys.exit()",
        vec![String::new()],
        None,
        "<string>",
        true,
    );
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stderr, None);
}

#[test]
fn raise_system_exit_with_code() {
    let r = run_program(
        "raise SystemExit(5)",
        vec![String::new()],
        None,
        "<string>",
        true,
    );
    assert_eq!(r.exit_code, 5);
    assert_eq!(r.stderr, None);
}

#[test]
fn system_exit_is_catchable() {
    // `except SystemExit as e: e.code` sees the exit argument.
    assert_eq!(
        g(
            "import sys\ntry:\n    sys.exit(2)\nexcept SystemExit as e:\n    c = e.code",
            "c"
        ),
        "2"
    );
}

#[test]
fn sys_version_info_namedtuple() {
    assert_eq!(
        g("import sys\nv = sys.version_info", "v"),
        "sys.version_info(major=3, minor=14, micro=6, releaselevel='final', serial=0)"
    );
    assert_eq!(
        g("import sys\nv = tuple(sys.version_info)", "v"),
        "(3, 14, 6, 'final', 0)"
    );
    assert_eq!(g("import sys\nv = sys.version_info[0]", "v"), "3");
}

#[test]
fn sys_scalars_present() {
    assert_eq!(g("import sys\nx = sys.maxsize", "x"), "9223372036854775807");
    assert_eq!(g("import sys\nx = type(sys.path).__name__", "x"), "'list'");
    assert_eq!(g("import sys\nx = sys.getrecursionlimit()", "x"), "1000");
    // sys.version reports the emulated CPython, not pythonrs's crate version.
    assert_eq!(g("import sys\nx = sys.version[:6]", "x"), "'3.14.6'");
}

#[test]
fn uncaught_traceback_shape() {
    // A nested-call uncaught exception prints the CPython traceback block: header,
    // one `File "…", line N, in <scope>` + source line per frame (outermost first),
    // then `ErrorType: message`. Caret markers are intentionally omitted.
    let src = "def a():\n    b()\n\ndef b():\n    raise ValueError(\"boom\")\n\na()\n";
    let r = run_program(
        src,
        vec!["t.py".into()],
        Some("/t.py".into()),
        "/t.py",
        true,
    );
    assert_eq!(r.exit_code, 1);
    let tb = r.stderr.expect("expected a traceback on stderr");
    let expected = concat!(
        "Traceback (most recent call last):\n",
        "  File \"/t.py\", line 7, in <module>\n",
        "    a()\n",
        "  File \"/t.py\", line 2, in a\n",
        "    b()\n",
        "  File \"/t.py\", line 5, in b\n",
        "    raise ValueError(\"boom\")\n",
        "ValueError: boom\n",
    );
    assert_eq!(tb, expected);
}

#[test]
fn stdin_traceback_omits_source_lines() {
    // Source text is unavailable for stdin, so only the frame headers show.
    let r = run_program(
        "raise KeyError(7)\n",
        vec![String::new()],
        None,
        "<stdin>",
        false,
    );
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.stderr.as_deref(),
        Some("Traceback (most recent call last):\n  File \"<stdin>\", line 1, in <module>\nKeyError: 7\n")
    );
}

#[test]
fn print_to_stderr_does_not_error() {
    // `print(..., file=sys.stderr)` routes off stdout without raising (the bytes
    // land on the process's stderr, which this in-process test can't capture).
    let r = run_program(
        "import sys\nprint('x', file=sys.stderr)\nok = True",
        vec![String::new()],
        None,
        "<string>",
        true,
    );
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stderr, None);
}

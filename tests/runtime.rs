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
fn builtin_exit_int_sets_exit_code() {
    // `exit(2)` (site.Quitter) is equivalent to `sys.exit(2)`.
    let r = run_program("exit(2)", vec![String::new()], None, "<string>", true);
    assert_eq!(r.exit_code, 2);
    assert_eq!(r.stderr, None);
}

#[test]
fn builtin_quit_none_is_zero() {
    let r = run_program("quit()", vec![String::new()], None, "<string>", true);
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stderr, None);
}

#[test]
fn builtin_exit_string_writes_message_exit_1() {
    let r = run_program("exit('bad')", vec![String::new()], None, "<string>", true);
    assert_eq!(r.exit_code, 1);
    assert_eq!(r.stderr.as_deref(), Some("bad\n"));
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

/// One helper: run `src` as `/t.py`, expect exit 1, return the traceback.
fn traceback_of(src: &str) -> String {
    let r = run_program(
        src,
        vec!["t.py".into()],
        Some("/t.py".into()),
        "/t.py",
        true,
    );
    assert_eq!(r.exit_code, 1);
    r.stderr.expect("expected a traceback on stderr")
}

#[test]
fn caret_anchor_shapes() {
    // CPython 3.11+ fine-grained caret anchors, byte-for-byte:
    // a subscript underlines its brackets (`~^^^`), a binary op its operator
    // (`~~^~~`), and an attribute error whose span covers the whole displayed
    // line draws no caret at all.
    assert_eq!(
        traceback_of("x = [1, 2]\nprint(x[9])\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    print(x[9])\n",
            "          ~^^^\n",
            "IndexError: list index out of range\n",
        )
    );
    assert_eq!(
        traceback_of("a = 1\nb = \"z\"\nc = a + b\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 3, in <module>\n",
            "    c = a + b\n",
            "        ~~^~~\n",
            "TypeError: unsupported operand type(s) for +: 'int' and 'str'\n",
        )
    );
    // Whole-line attribute span → caret omitted (matches CPython).
    assert_eq!(
        traceback_of("v = None\nv.attr\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    v.attr\n",
            "AttributeError: 'NoneType' object has no attribute 'attr'\n",
        )
    );
}

#[test]
fn caret_call_value_suppressed() {
    // CPython omits the caret when an `x = f(...)` / `return f(...)` call raises;
    // the same `int("z")` inside a bare expression IS anchored.
    assert_eq!(
        traceback_of("y = int(\"z\")\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 1, in <module>\n",
            "    y = int(\"z\")\n",
            "ValueError: invalid literal for int() with base 10: 'z'\n",
        )
    );
    assert_eq!(
        traceback_of("int(\"z\")\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 1, in <module>\n",
            "    int(\"z\")\n",
            "    ~~~^^^^^\n",
            "ValueError: invalid literal for int() with base 10: 'z'\n",
        )
    );
}

#[test]
fn uncaught_traceback_shape() {
    // A nested-call uncaught exception prints the CPython traceback block: header,
    // one `File "…", line N, in <scope>` + source line + caret per frame
    // (outermost first), then `ErrorType: message`. The `a()`/`b()` call sites get
    // the `~^^` call anchor; the bare `raise` line has no caret.
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
        "    ~^^\n",
        "  File \"/t.py\", line 2, in a\n",
        "    b()\n",
        "    ~^^\n",
        "  File \"/t.py\", line 5, in b\n",
        "    raise ValueError(\"boom\")\n",
        "ValueError: boom\n",
    );
    assert_eq!(tb, expected);
}

#[test]
fn uncaught_traceback_explicit_cause_chain() {
    // `raise X from Y` renders both blocks: the cause first (its own frames), the
    // CPython connector line, then the raising exception. The `int("bad")` call
    // carries the `~~~^^^^^^^` caret; the `raise … from e` line has none.
    let src = "try:\n    int(\"bad\")\nexcept ValueError as e:\n    raise RuntimeError(\"wrapped\") from e\n";
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
        "  File \"/t.py\", line 2, in <module>\n",
        "    int(\"bad\")\n",
        "    ~~~^^^^^^^\n",
        "ValueError: invalid literal for int() with base 10: 'bad'\n",
        "\n",
        "The above exception was the direct cause of the following exception:\n",
        "\n",
        "Traceback (most recent call last):\n",
        "  File \"/t.py\", line 4, in <module>\n",
        "    raise RuntimeError(\"wrapped\") from e\n",
        "RuntimeError: wrapped\n",
    );
    assert_eq!(tb, expected);
}

#[test]
fn uncaught_traceback_implicit_context_chain() {
    // An exception raised while handling another (no `from`) chains via
    // `__context__` with the "During handling …" connector, including the frames
    // of the function where the original exception was raised. The `parse("xyz")`
    // call gets the `~~~~~^^^^^^^` caret; `return int(s)` is caret-suppressed (a
    // `return call()` value), matching CPython.
    let src = "def parse(s):\n    return int(s)\n\ntry:\n    parse(\"xyz\")\nexcept ValueError:\n    raise RuntimeError(\"handler failed\")\n";
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
        "  File \"/t.py\", line 5, in <module>\n",
        "    parse(\"xyz\")\n",
        "    ~~~~~^^^^^^^\n",
        "  File \"/t.py\", line 2, in parse\n",
        "    return int(s)\n",
        "ValueError: invalid literal for int() with base 10: 'xyz'\n",
        "\n",
        "During handling of the above exception, another exception occurred:\n",
        "\n",
        "Traceback (most recent call last):\n",
        "  File \"/t.py\", line 7, in <module>\n",
        "    raise RuntimeError(\"handler failed\")\n",
        "RuntimeError: handler failed\n",
    );
    assert_eq!(tb, expected);
}

#[test]
fn uncaught_traceback_from_none_suppresses_context() {
    // `raise X from None` sets `__suppress_context__`, so the implicit context is
    // hidden — only the raising exception's block prints.
    let src =
        "try:\n    int(\"bad\")\nexcept ValueError:\n    raise RuntimeError(\"clean\") from None\n";
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
        "  File \"/t.py\", line 4, in <module>\n",
        "    raise RuntimeError(\"clean\") from None\n",
        "RuntimeError: clean\n",
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

#[test]
fn main_module_dunders_are_bound() {
    // CPython binds `__doc__`/`__package__`/`__spec__` in `__main__` before the
    // body runs; reading one must not raise `NameError`. `__cached__` names the
    // bytecode file, so it exists only when there IS a script file.
    assert_eq!(
        g("x = (__name__, __doc__, __package__, __spec__)", "x"),
        "('__main__', None, None, None)"
    );
    assert_eq!(
        g("x = '__cached__' in globals()", "x"),
        "False",
        "`python -c` has no script file, so no __cached__"
    );
    let r = run_program(
        "x = ('__cached__' in globals(), __cached__)",
        vec!["prog.py".to_string()],
        Some("prog.py".to_string()),
        "prog.py",
        true,
    );
    assert_eq!(r.exit_code, 0, "stderr {:?}", r.stderr);
    assert_eq!(
        host::with_host(|h| {
            let v = h.read_global("x").expect("x unbound");
            h.repr_of(&v)
        }),
        "(True, None)"
    );
}

#[test]
fn module_docstring_binds_dunder_doc() {
    // A body opening with a string literal stores it as `__doc__` — and a body
    // that does NOT open with one leaves `__doc__` alone (CPython emits the
    // store only when a docstring is present).
    assert_eq!(g("'''Mod doc.'''\nx = __doc__", "x"), "'Mod doc.'");
    assert_eq!(g("y = 1\nx = __doc__", "x"), "None");
}

/// `__debug__` is a builtin CONSTANT, not a module global: it resolves in every
/// scope of every module without being bound anywhere, and it is False exactly
/// when the interpreter is optimized. `if __debug__:` is the ordinary spelling
/// of a debug-only block, so leaving it unbound made that a `NameError`.
#[test]
fn dunder_debug_is_bound_in_every_scope() {
    assert_eq!(g("x = __debug__", "x"), "True");
    assert_eq!(g("def f(): return __debug__\nx = f()", "x"), "True");
    assert_eq!(g("class C:\n    v = __debug__\nx = C.v", "x"), "True");
    assert_eq!(
        g("x = 'off'\nif __debug__:\n    x = 'on'", "x"),
        "'on'",
        "the guard must fire in an unoptimized interpreter"
    );
    // The value tracks `sys.flags.optimize`, whose CLI spelling (`-O`/`-OO`) the
    // binary folds into `PYTHONOPTIMIZE`. CPython's parse is lax: empty is level
    // 0, an integer is that integer, and any other non-empty value is 1.
    for (env, want) in [
        (None, 0),
        (Some(""), 0),
        (Some("0"), 0),
        (Some("1"), 1),
        (Some("2"), 2),
        (Some("x"), 1),
    ] {
        // SAFETY: single-threaded test, and the read below is in this process.
        unsafe {
            match env {
                Some(v) => std::env::set_var("PYTHONOPTIMIZE", v),
                None => std::env::remove_var("PYTHONOPTIMIZE"),
            }
        }
        assert_eq!(
            pythonrs::host::optimize_level(),
            want,
            "for PYTHONOPTIMIZE={env:?}"
        );
    }
    unsafe { std::env::remove_var("PYTHONOPTIMIZE") };
}

/// CPython's `Did you mean: 'x'?` hint on an uncaught `NameError`/
/// `AttributeError`. Every expectation is CPython 3.14.6's, byte-checked
/// against `python3` on the same program.
#[test]
fn did_you_mean_suggestions() {
    // A module global, a frame-slotted function local, and a builtin.
    assert_eq!(
        traceback_of("value = 1\nvalu\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    valu\n",
            "NameError: name 'valu' is not defined. Did you mean: 'value'?\n",
        )
    );
    assert!(
        traceback_of("def f():\n    counter = 1\n    return countr\nf()\n")
            .ends_with("NameError: name 'countr' is not defined. Did you mean: 'counter'?\n")
    );
    assert!(traceback_of("leng([1])\n")
        .ends_with("NameError: name 'leng' is not defined. Did you mean: 'len'?\n"));
    // `st` is equally close to `set` and `str`; CPython answers `set`, which is
    // only reproducible with `suggestions.c`'s tie-breaking (its Python fallback
    // in `traceback.py` finds neither).
    assert!(traceback_of("st\n")
        .ends_with("NameError: name 'st' is not defined. Did you mean: 'set'?\n"));
    // Attributes: an instance attribute, a method reached through the fused
    // CALL_METHOD op (which never goes through `get_attr`), and a builtin type.
    assert!(
        traceback_of("class C:\n    def __init__(s):\n        s.value = 1\nC().valu\n").ends_with(
            "AttributeError: 'C' object has no attribute 'valu'. Did you mean: 'value'?\n"
        )
    );
    assert!(
        traceback_of("class C:\n    def total(s):\n        return 1\nC().totl()\n").ends_with(
            "AttributeError: 'C' object has no attribute 'totl'. Did you mean: 'total'?\n"
        )
    );
    assert!(traceback_of("x = [1]\nx.appendd(2)\n").ends_with(
        "AttributeError: 'list' object has no attribute 'appendd'. Did you mean: 'append'?\n"
    ));
    // Nothing close enough gets no hint at all.
    assert!(traceback_of("value = 1\nqqqqqqqqqq\n")
        .ends_with("NameError: name 'qqqqqqqqqq' is not defined\n"));
    assert!(traceback_of("x = [1]\nx.zzzzzzzz\n")
        .ends_with("AttributeError: 'list' object has no attribute 'zzzzzzzz'\n"));
    // The hint belongs to the rendered traceback, never to the exception: CPython's
    // `str(e)` and `e.args` carry the bare message.
    assert_eq!(
        g(
            "value = 1\n\
             try:\n\
             \x20   valu\n\
             except NameError as e:\n\
             \x20   x = (str(e), e.args)",
            "x"
        ),
        "(\"name 'valu' is not defined\", (\"name 'valu' is not defined\",))"
    );
    assert_eq!(
        g(
            "xs = [1]\n\
             try:\n\
             \x20   xs.appendd\n\
             except AttributeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"'list' object has no attribute 'appendd'\""
    );
}

#[test]
fn container_display_and_subscript_store_carry_a_line_and_caret() {
    // A set/dict display and a subscript STORE emitted line 0, so an unhashable
    // key rendered `File "…", line 0, in <module>` with no source line and no
    // carets — the traceback named nothing at all. They now carry the
    // statement's line and the expression's span, matching CPython's frame.
    assert_eq!(
        traceback_of("d = {[1]: 5}\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 1, in <module>\n",
            "    d = {[1]: 5}\n",
            "        ^^^^^^^^\n",
            "TypeError: unhashable type: 'list'\n",
        )
    );
    assert_eq!(
        traceback_of("s = {[1]}\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 1, in <module>\n",
            "    s = {[1]}\n",
            "        ^^^^^\n",
            "TypeError: unhashable type: 'list'\n",
        )
    );
    // The store anchors its bracket region, so the receiver renders `~`.
    assert_eq!(
        traceback_of("d = {}\nd[[1]] = 2\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    d[[1]] = 2\n",
            "    ~^^^^^\n",
            "TypeError: unhashable type: 'list'\n",
        )
    );
    // A `{**a, …}` merge takes a different lowering path with the same gap.
    assert_eq!(
        traceback_of("a = {}\nd = {**a, [1]: 2}\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    d = {**a, [1]: 2}\n",
            "        ^^^^^^^^^^^^^\n",
            "TypeError: unhashable type: 'list'\n",
        )
    );
}

#[test]
fn attribute_store_and_delete_carry_a_line_and_caret() {
    // The attribute STORE and both DELETE forms emitted line 0, so every
    // traceback from a rejected `obj.attr = v` / `del obj.attr` / `del obj[k]`
    // said `line 0` with no source line and no carets. `__slots__` rejection is
    // the everyday way to reach the store path.
    assert_eq!(
        traceback_of("class S:\n    __slots__ = ()\ns = S()\ns.x = 1\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 4, in <module>\n",
            "    s.x = 1\n",
            "    ^^^\n",
            "AttributeError: 'S' object has no attribute 'x' and no __dict__ \
             for setting new attributes\n",
        )
    );
    // `del obj.attr` underlines the whole target, `del obj[k]` anchors its
    // bracket region — the same shapes CPython draws for the stores.
    assert_eq!(
        traceback_of("class A: pass\na = A()\ndel a.__class__\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 3, in <module>\n",
            "    del a.__class__\n",
            "        ^^^^^^^^^^^\n",
            "TypeError: can't delete __class__ attribute\n",
        )
    );
    assert_eq!(
        traceback_of("d = {}\ndel d[1]\n"),
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"/t.py\", line 2, in <module>\n",
            "    del d[1]\n",
            "        ~^^^\n",
            "KeyError: 1\n",
        )
    );
}

// ── PYTHONHASHSEED (subprocess: the secret is a process-wide OnceLock) ────────

/// Run `python -c <code>` with `PYTHONHASHSEED` set to `seed`, returning
/// `(stdout, stderr, exit code)`. A subprocess is required: the hash secret
/// latches once per process, so an in-process test could only ever observe the
/// seed the test runner itself was started with.
fn run_with_hash_seed(seed: &str, code: &str) -> (String, String, i32) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_python"))
        .env("PYTHONHASHSEED", seed)
        .args(["-c", code])
        .output()
        .expect("spawn python binary");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A pinned `PYTHONHASHSEED` reproduces CPython's `str`/`bytes` hashes for that
/// seed, not just for seed 0.
///
/// The expected numbers are CPython 3.14.6's, measured as
/// `PYTHONHASHSEED=<seed> python3 -c 'print(hash("abc"), hash(b"xyz"), hash("héllo"))'`.
/// Before `_Py_HashRandomization_Init` was ported, every seed produced the
/// seed-0 row, so only the first case here would have passed.
#[test]
fn pinned_hash_seed_matches_cpython_for_every_seed() {
    let cases = [
        (
            "0",
            "-4594863902769663758 9013747392277146282 6395329678795984700",
        ),
        (
            "1",
            "-4667308735975688587 -5634034666027049350 4627783765987660535",
        ),
        (
            "42",
            "3869580338025362921 -4491887112347156947 -4058713343194919257",
        ),
        (
            "4294967295",
            "-6122489556238538401 2562532176708456890 3491461287047953188",
        ),
    ];
    for (seed, expect) in cases {
        let (out, err, code) =
            run_with_hash_seed(seed, r#"print(hash("abc"), hash(b"xyz"), hash("héllo"))"#);
        assert_eq!(code, 0, "seed {seed}: exited {code}, stderr {err}");
        assert_eq!(out, expect, "seed {seed}");
    }
}

/// An unset or `random` seed draws per-process entropy, as CPython does, so two
/// runs disagree. This is the behavior that makes the dict-collision mitigation
/// real; a fixed default would be a security regression against CPython, not
/// just a cosmetic one.
///
/// The empty string is CPython's "unset" (`_Py_GetEnv` maps it to `NULL`), and
/// `hash("")` is 0 under every key — so the run that varies and the value that
/// cannot vary are asserted together, which is what distinguishes "randomized"
/// from "crashed".
#[test]
fn unpinned_hash_seed_varies_per_process() {
    for seed in ["random", ""] {
        let code = r#"print(hash("abc"), hash(""))"#;
        let (a, _, ca) = run_with_hash_seed(seed, code);
        let (b, _, cb) = run_with_hash_seed(seed, code);
        assert_eq!((ca, cb), (0, 0), "seed {seed:?} should run");
        assert_ne!(a, b, "seed {seed:?} should randomize across processes");
        assert!(
            a.ends_with(" 0") && b.ends_with(" 0"),
            "hash('') must stay 0"
        );
    }
}

/// A `PYTHONHASHSEED` CPython refuses is refused here, with CPython's own
/// pre-initialization message and exit code — accepting it would let a program
/// run under pythonrs that aborts under `python3`.
#[test]
fn invalid_hash_seed_is_the_cpython_fatal_error() {
    // strtoul semantics: trailing text (including a space), a non-decimal base,
    // and anything outside [0, 4294967295].
    for seed in ["abc", "-1", "4294967296", "0x10", "42 ", "1e3", " "] {
        let (out, err, code) = run_with_hash_seed(seed, "print(1)");
        assert_eq!(code, 1, "seed {seed:?} should be fatal, stdout {out:?}");
        assert_eq!(out, "", "seed {seed:?} must not run the program");
        assert_eq!(
            err,
            "Fatal Python error: config_init_hash_seed: PYTHONHASHSEED must be \"random\" \
             or an integer in range [0; 4294967295]\nPython runtime state: preinitialized",
            "seed {seed:?}"
        );
    }
    // …and the forms strtoul DOES accept still run: leading space, leading
    // zeros, an explicit sign.
    for seed in [" 42", "007", "+7", "-0"] {
        let (_, err, code) = run_with_hash_seed(seed, "print(1)");
        assert_eq!(code, 0, "seed {seed:?} should be accepted, stderr {err}");
    }
}

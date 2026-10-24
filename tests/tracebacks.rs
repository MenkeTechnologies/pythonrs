//! Tracebacks of programs that span several files and the CPython bridge,
//! run through the built `python` binary. Expected text is CPython 3.14.8's
//! for the same files, with their directory written as `DIR`.

use std::process::Command;

/// Write `files` into a fresh directory, run `main` from it, and return
/// `(stdout, stderr)` with the directory's path replaced by `DIR`.
fn run_files(files: &[(&str, &str)], main: &str) -> (String, String) {
    let tmp = tempfile::tempdir().expect("temp dir");
    // The canonical path, so the script path and the module paths found
    // through `sys.path[0]` spell the directory the same way.
    let dir = tmp.path().canonicalize().expect("canonical temp dir");
    for (name, src) in files {
        std::fs::write(dir.join(name), src).expect("write source");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg(dir.join(main))
        .env("PYTHONRS_CACHE", "0")
        .output()
        .expect("spawn python");
    let d = dir.to_string_lossy().into_owned();
    let clean = |b: &[u8]| String::from_utf8_lossy(b).replace(&d, "DIR");
    (clean(&out.stdout), clean(&out.stderr))
}

/// A frame names the file of the module it runs in, with that file's line and
/// caret: a function of an imported module, and a module body that fails
/// while being imported (listed below the `import` line). They named the
/// main script and showed its line instead.
#[test]
fn a_frame_names_its_own_modules_file() {
    let helper = "def boom(x):\n    return 1 / x\n";
    let bad = "def h():\n    return [][0]\nh()\n";
    let main = "import helper\ndef f():\n    helper.boom(0)\ntry:\n    f()\nexcept ZeroDivisionError as e:\n    \
                print(e.__traceback__.tb_next.tb_next.tb_lineno)\ntry:\n    import bad\nexcept IndexError:\n    \
                print('import failed')\nf()\n";
    let (out, err) = run_files(
        &[("helper.py", helper), ("bad.py", bad), ("main.py", main)],
        "main.py",
    );
    assert_eq!(out, "2\nimport failed\n");
    assert_eq!(
        err,
        "Traceback (most recent call last):\n  File \"DIR/main.py\", line 12, in <module>\n    f()\n    ~^^\n  \
         File \"DIR/main.py\", line 3, in f\n    helper.boom(0)\n    ~~~~~~~~~~~^^^\n  \
         File \"DIR/helper.py\", line 2, in boom\n    return 1 / x\n           ~~^~~\nZeroDivisionError: division by zero\n"
    );
    let (_, err) = run_files(&[("bad.py", bad), ("main.py", "import bad\n")], "main.py");
    assert_eq!(
        err,
        "Traceback (most recent call last):\n  File \"DIR/main.py\", line 1, in <module>\n    import bad\n  \
         File \"DIR/bad.py\", line 3, in <module>\n    h()\n    ~^^\n  \
         File \"DIR/bad.py\", line 2, in h\n    return [][0]\n           ~~^^^\nIndexError: list index out of range\n"
    );
}

/// An exception's `__notes__` print under its final line, one line per line of
/// each note (`add_note` is 3.11+, so this is not in the 3.9-spanning parity
/// corpus); a re-raise keeps the entries the exception already had, and a
/// `raise` that does not fill its line is underlined.
#[test]
fn notes_print_under_the_final_line_of_a_reraised_exception() {
    let main =
        "def f():\n    e = KeyError('k'); e.add_note('first\\nsecond'); raise e\ntry:\n    f()\n\
                except KeyError as e:\n    e.add_note('more')\n    raise\n";
    let (out, err) = run_files(&[("main.py", main)], "main.py");
    assert_eq!(out, "");
    assert_eq!(
        err,
        "Traceback (most recent call last):\n  File \"DIR/main.py\", line 4, in <module>\n    f()\n    ~^^\n  \
         File \"DIR/main.py\", line 2, in f\n    e = KeyError('k'); e.add_note('first\\nsecond'); raise e\n    \
         \x20                                               ^^^^^^^\nKeyError: 'k'\nfirst\nsecond\nmore\n"
    );
}

/// Run `main` from a fresh directory holding `files` with `env` set, and
/// return its stderr with the directory written as `DIR`.
fn stderr_with_env(files: &[(&str, &str)], main: &str, env: &[(&str, &str)]) -> String {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().canonicalize().expect("canonical temp dir");
    for (name, src) in files {
        std::fs::write(dir.join(name), src).expect("write source");
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_python"));
    cmd.arg(dir.join(main)).env("PYTHONRS_CACHE", "0");
    for var in ["PYTHON_COLORS", "NO_COLOR", "FORCE_COLOR", "TERM"] {
        cmd.env_remove(var);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn python");
    String::from_utf8_lossy(&out.stderr).replace(&*dir.to_string_lossy(), "DIR")
}

/// The traceback of an uncaught exception is coloured as `sys.excepthook`
/// colours it when `_colorize.can_colorize(file=sys.stderr)` holds: the
/// `File` line's parts in magenta, the caret range in red with its anchor in
/// bold red, the type in bold magenta and the message in magenta; a note and
/// the chain connector stay plain. A syntax error colours its range the same
/// way. `PYTHON_COLORS=0` wins over `FORCE_COLOR`, and a pipe is not a
/// terminal. Expected output is python3.14's.
#[test]
fn an_uncaught_traceback_is_coloured_when_stderr_may_be() {
    let main = "def f(d):\n    return d[\"a\"] + 1\ntry:\n    f({\"a\": \"x\"})\nexcept TypeError as e:\n    \
                e.add_note(\"a note\")\n    raise ValueError(\"outer\") from e\n";
    let err = stderr_with_env(&[("main.py", main)], "main.py", &[("PYTHON_COLORS", "1")]);
    assert_eq!(
        err,
        "Traceback (most recent call last):\n  \
         File \x1b[35m\"DIR/main.py\"\x1b[0m, line \x1b[35m4\x1b[0m, in \x1b[35m<module>\x1b[0m\n    \
         \x1b[31mf\x1b[0m\x1b[1;31m({\"a\": \"x\"})\x1b[0m\n    \
         \x1b[31m~\x1b[0m\x1b[1;31m^^^^^^^^^^^^\x1b[0m\n  \
         File \x1b[35m\"DIR/main.py\"\x1b[0m, line \x1b[35m2\x1b[0m, in \x1b[35mf\x1b[0m\n    \
         return \x1b[31md[\"a\"] \x1b[0m\x1b[1;31m+\x1b[0m\x1b[31m 1\x1b[0m\n           \
         \x1b[31m~~~~~~~\x1b[0m\x1b[1;31m^\x1b[0m\x1b[31m~~\x1b[0m\n\
         \x1b[1;35mTypeError\x1b[0m: \x1b[35mcan only concatenate str (not \"int\") to str\x1b[0m\n\
         a note\n\n\
         The above exception was the direct cause of the following exception:\n\n\
         Traceback (most recent call last):\n  \
         File \x1b[35m\"DIR/main.py\"\x1b[0m, line \x1b[35m7\x1b[0m, in \x1b[35m<module>\x1b[0m\n    \
         raise ValueError(\"outer\") from e\n\
         \x1b[1;35mValueError\x1b[0m: \x1b[35mouter\x1b[0m\n"
    );
    let err = stderr_with_env(
        &[("syn.py", "x = (1,\n2 3)\n")],
        "syn.py",
        &[("FORCE_COLOR", "1")],
    );
    assert_eq!(
        err,
        "  File \x1b[35m\"DIR/syn.py\"\x1b[0m, line \x1b[35m2\x1b[0m\n    \
         \x1b[1;31m2 3\x1b[0m)\n    \x1b[1;31m^^^\x1b[0m\n\
         \x1b[1;35mSyntaxError\x1b[0m: \x1b[35minvalid syntax. Perhaps you forgot a comma?\x1b[0m\n"
    );
    let plain = "  File \"DIR/syn.py\", line 2\n    2 3)\n    ^^^\n\
                 SyntaxError: invalid syntax. Perhaps you forgot a comma?\n";
    for env in [
        &[("PYTHON_COLORS", "0"), ("FORCE_COLOR", "1")][..],
        &[("NO_COLOR", "1"), ("FORCE_COLOR", "1")][..],
        &[][..],
    ] {
        assert_eq!(
            stderr_with_env(&[("syn.py", "x = (1,\n2 3)\n")], "syn.py", env),
            plain
        );
    }
}

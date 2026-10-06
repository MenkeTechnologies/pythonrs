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

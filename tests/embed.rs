//! Tests for the embedder entry point (`eval_str_captured`), which an in-process
//! host uses instead of `eval_str`: globals are seeded after the host reset, and
//! program output is captured rather than written to the process streams.

/// A seeded global is a real Python `str`, not just something that reprs like
/// one: it answers `.upper()`. Strings live on the host heap, so this is the
/// property that a `Value::Str`-shaped API would silently fail.
#[test]
fn seeded_globals_are_real_strings() {
    let (result, out) =
        pythonrs::eval_str_captured("print(type(stdin).__name__)", &[("stdin", "hi")]);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, "str\n");
}

/// A seeded global is visible to the program. `eval_str` cannot do this — it
/// resets the host, so anything installed beforehand is gone by the time the
/// program runs.
#[test]
fn seeded_globals_survive_the_host_reset() {
    let (result, out) = pythonrs::eval_str_captured("print(stdin.upper())", &[("stdin", "hello")]);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, "HELLO\n");
}

/// Both native write paths are captured: `print` (which goes through
/// `write_stdout`) and a direct `sys.stdout.write` (which goes through the file
/// handle layer). Neither can reach a terminal the embedder owns.
#[test]
fn every_write_path_is_captured() {
    let (result, out) = pythonrs::eval_str_captured(
        "import sys\nsys.stdout.write('raw ')\nprint('printed')\n",
        &[],
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, "raw printed\n");
}

/// A program that prints and then raises produced both: the error and the
/// output it managed to write are returned side by side.
#[test]
fn output_before_a_raise_is_kept() {
    let (result, out) =
        pythonrs::eval_str_captured("print('before')\nraise ValueError('boom')", &[]);
    assert!(result.is_err(), "expected the raise to surface");
    assert_eq!(out, "before\n");
}

/// Capture is per-run: a second run does not see the first run's output.
#[test]
fn capture_resets_between_runs() {
    let (_, first) = pythonrs::eval_str_captured("print('one')", &[]);
    let (_, second) = pythonrs::eval_str_captured("print('two')", &[]);
    assert_eq!(first, "one\n");
    assert_eq!(second, "two\n");
}

// `run_program`: the uniform `(src, input) -> Result<String, String>` entry point.

#[test]
fn print_is_captured() {
    assert_eq!(
        pythonrs::run_program("print('hello')", "").unwrap(),
        "hello\n"
    );
}

#[test]
fn input_and_sys_stdin_read_the_given_input() {
    let src = "import sys\nfirst = input()\nrest = sys.stdin.read()\nprint(first.upper(), len(rest.splitlines()))";
    assert_eq!(
        pythonrs::run_program(src, "abc\nx\ny\n").unwrap(),
        "ABC 2\n"
    );
}

#[test]
fn errors_and_exit_codes() {
    assert!(pythonrs::run_program("def (", "").is_err());
    let err = pythonrs::run_program("raise ValueError('boom')", "").unwrap_err();
    assert!(err.ends_with("ValueError: boom"), "{err}");
    assert_eq!(
        pythonrs::run_program("import sys\nsys.exit(3)", "").unwrap_err(),
        "SystemExit: 3"
    );
    assert_eq!(
        pythonrs::run_program("import sys\nprint('a')\nsys.exit(0)\nprint('b')", "").unwrap(),
        "a\n"
    );
}

#[test]
fn concurrent_calls_keep_their_own_output() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                let input: String = (0..500).map(|i| format!("{t} {i}\n")).collect();
                let src = "import sys\nfor line in sys.stdin:\n    print(line.split()[0])";
                (t, pythonrs::run_program(src, &input).unwrap())
            })
        })
        .collect();
    for h in handles {
        let (t, out) = h.join().unwrap();
        assert!(
            out.lines().all(|l| l == t.to_string()),
            "thread {t} saw foreign output"
        );
        assert_eq!(out.lines().count(), 500);
    }
}

#[test]
fn stderr_is_not_in_run_program_output_but_stays_in_eval_str_captured() {
    let src =
        "import sys\nprint('out')\nsys.stderr.write('err\\n')\nprint('more', file=sys.stderr)";
    assert_eq!(pythonrs::run_program(src, "").unwrap(), "out\n");
    let (result, merged) = pythonrs::eval_str_captured(src, &[]);
    assert!(result.is_ok());
    assert_eq!(merged, "out\nerr\nmore\n");
}

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

//! `python --build`: a script compiled to a standalone executable behaves as
//! the interpreter does. Only the libpython-free build can AOT (a `stdlib-ffi`
//! build refuses up front), so these run under `cargo test
//! --no-default-features` — after a `cargo build --no-default-features`, since
//! `--build` links the `libpythonrs.a` beside the binary and a test build does
//! not refresh it. Expectations are CPython 3.14.8's output for the same
//! script.
#![cfg(not(feature = "stdlib-ffi"))]

use std::process::Command;

/// Build `src` (saved as `<dir>/<name>.py`) and run the executable:
/// `(stdout, stderr, exit status)`.
fn build_and_run(name: &str, src: &str) -> (String, String, i32) {
    let dir = std::env::temp_dir().join(format!("pythonrs_aot_{}_{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let script = dir.join(format!("{name}.py"));
    std::fs::write(&script, src).expect("write script");
    let built = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg("--build")
        .arg(&script)
        .env("PYTHONRS_CACHE", "0")
        .output()
        .expect("spawn python --build");
    assert!(
        built.status.success(),
        "--build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let out = Command::new(dir.join(name))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the built executable");
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// The embedded chunk is fusevm's format tag followed by the bincode chunk;
/// the runner deserialized the tag as data and every built binary failed with
/// `corrupt embedded chunk` (or hung allocating). And an error the VM raises
/// itself — `int + str` on the native fast path — sits in the run's result,
/// not on the host: the binary exited 0 with no traceback.
#[test]
fn a_built_binary_runs_and_reports_a_native_op_error() {
    let (stdout, stderr, code) = build_and_run(
        "addstr",
        "x = 1\nprint(\"before\")\ny = x + \"a\"\nprint(\"after\")\n",
    );
    assert_eq!(stdout, "before\n");
    assert!(
        stderr.ends_with(
            "addstr.py\", line 3, in <module>\n    y = x + \"a\"\n        ~~^~~~~\n\
             TypeError: unsupported operand type(s) for +: 'int' and 'str'\n"
        ),
        "stderr: {stderr}"
    );
    assert!(
        stderr.starts_with("Traceback (most recent call last):\n"),
        "stderr: {stderr}"
    );
    assert_eq!(code, 1);
}

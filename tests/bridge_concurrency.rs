//! Several pythonrs hosts on several threads share ONE embedded CPython, so one
//! program's first import is visible, half-built, to every other program
//! running at the same moment.
//!
//! The case that bit: `dataclasses._process_class` reads
//! `sys.modules.get('typing')` without the import lock and then
//! `typing.ClassVar`. While another thread is still executing `typing`'s module
//! body, that read finds the partially initialized module and the decorator
//! raises `AttributeError: partially initialized module 'typing' has no
//! attribute 'ClassVar'`. Plain CPython has the same race between its own
//! threads; under pythonrs it is reachable from programs that never share
//! anything, so the bridge imports `typing` while it starts the interpreter,
//! before any program can observe the window.
//!
//! The race needs a process where `typing` has not been imported yet, so the
//! test re-runs this test binary as a child process, many times.

use std::process::Command;
use std::sync::{Arc, Barrier};

const CHILD_ENV: &str = "PYTHONRS_BRIDGE_RACE_CHILD";

/// Run in a child process only: one thread imports `typing` for the first time
/// while eight others decorate dataclasses in a loop, all starting together.
#[test]
fn race_child_dataclass_against_first_typing_import() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    const DECORATORS: usize = 8;
    let barrier = Arc::new(Barrier::new(DECORATORS + 1));
    let mut handles = Vec::new();
    {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            pythonrs::eval_str("import dataclasses").map_err(|e| e.to_string())?;
            barrier.wait();
            pythonrs::eval_str("import typing\nx = typing.ClassVar").map_err(|e| e.to_string())
        }));
    }
    for _ in 0..DECORATORS {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            pythonrs::eval_str("import dataclasses").map_err(|e| e.to_string())?;
            barrier.wait();
            pythonrs::eval_str(
                "import dataclasses\n\
                 for _ in range(100):\n\
                 \x20   @dataclasses.dataclass\n\
                 \x20   class P:\n\
                 \x20       x: int = 1\n",
            )
            .map_err(|e| e.to_string())
        }));
    }
    let errors: Vec<String> = handles
        .into_iter()
        .filter_map(|h| h.join().expect("thread panicked").err())
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
#[cfg(feature = "stdlib-ffi")]
fn dataclasses_never_see_a_half_imported_typing() {
    let exe = std::env::current_exe().expect("test binary path");
    for run in 0..25 {
        let out = Command::new(&exe)
            .env(CHILD_ENV, "1")
            .args([
                "race_child_dataclass_against_first_typing_import",
                "--exact",
            ])
            .output()
            .expect("spawn child test");
        assert!(
            out.status.success(),
            "run {run}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

//! The interactive REPL must survive a failed `import`.
//!
//! This is a PTY test and it has to be. The REPL only takes its interactive path
//! when stdin is a terminal, and the failure it guards against — CPython's
//! `Py_FatalError` calling `abort()` when the embedded interpreter cannot find a
//! stdlib — kills the whole process. Through a pipe the REPL never enters that
//! path at all, so a piped test passes no matter how broken the interactive one
//! is; that is why this class of bug reached a user's daily shell.
//!
//! Reported as: `import os` at the prompt printing
//! `Fatal Python error: Failed to import encodings module` and returning the
//! user to their shell, where the next line they typed was swallowed.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A directory that exists and is definitely not a CPython stdlib.
///
/// `PYTHONRS_STDLIB` is the one input that reaches the interpreter's home
/// resolution from outside, so it is the hermetic way to reproduce "the stdlib
/// is not where pythonrs thinks it is" without depending on what the machine has
/// installed. The real report came from the same condition arrived at
/// differently: Homebrew's `bin/python` is a symlink, `current_exe()` on macOS
/// does not resolve it, and the `/opt/homebrew/lib/python3.14` that the
/// unresolved path pointed at holds nothing but `site-packages`.
fn empty_prefix() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pythonrs-pty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create probe dir");
    dir
}

/// Open a pty pair, or `None` when the sandbox has no ptys to give — the signal
/// to skip rather than to fail.
fn open_pty() -> Option<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: `openpty` writes two fds through the out-pointers and is given
    // null for every optional argument, which it documents as "use defaults".
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: both fds were just produced by `openpty` and are owned by us.
    unsafe { Some((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))) }
}

/// Run the built `python` as an interactive REPL on a pty, feed `lines`, and
/// return everything it wrote plus whether it exited normally.
///
/// Terminal queries are answered as they arrive: the line editor asks for the
/// cursor position with DSR (`ESC [ 6 n`) and blocks until something replies, so
/// a driver that only writes would deadlock and report a timeout as a crash.
fn drive_repl(lines: &[&str], stdlib: &std::path::Path) -> Option<(String, bool)> {
    let (master, slave) = open_pty()?;
    let slave_in = slave.try_clone().ok()?;
    let slave_out = slave.try_clone().ok()?;

    let mut child = unsafe {
        Command::new(env!("CARGO_BIN_EXE_python"))
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave))
            .env("PYTHONRS_STDLIB", stdlib)
            .env("TERM", "dumb")
            .env_remove("PYTHONHOME")
            // Its own session, so the pty is the child's controlling terminal
            // and not shared with the test runner's.
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()
            .ok()?
    };

    let mut pty = std::fs::File::from(master);
    let mut reader = pty.try_clone().ok()?;
    // SAFETY-free: a reader thread keeps the pty drained so the child can never
    // block writing while we are blocked writing to it.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut out: Vec<u8> = Vec::new();
    let mut sent = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60);

    // Wait for the prompt before typing anything. Sending on a timer instead
    // made this test FLAKY in exactly the direction that hides the bug: on a
    // cold binary the child needs longer to reach its prompt than any idle
    // threshold worth using, so the driver typed every line plus EOF into a
    // process that had not started reading, then reported the missing reply as
    // a failure. Readiness is a fact to observe, not an interval to guess.
    while Instant::now() < deadline {
        let ready = {
            let seen = String::from_utf8_lossy(&out);
            seen.contains(">>>") || seen.contains("type an expression")
        };
        if ready {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = pty.write_all(b"\x1b[1;1R");
                }
                out.extend_from_slice(&chunk);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
    }

    let mut quiet = 0;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(chunk) => {
                if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = pty.write_all(b"\x1b[1;1R");
                }
                out.extend_from_slice(&chunk);
                quiet = 0;
            }
            Err(_) => {
                // Idle twice in a row means the child is waiting on us.
                quiet += 1;
                if quiet < 2 {
                    continue;
                }
                quiet = 0;
                if sent < lines.len() {
                    let _ = pty.write_all(format!("{}\n", lines[sent]).as_bytes());
                    sent += 1;
                } else {
                    let _ = pty.write_all(&[0x04]); // Ctrl-D
                    break;
                }
            }
        }
    }
    // Drain whatever followed the final input before reaping.
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(400)) {
        out.extend_from_slice(&chunk);
    }
    let status = match child.wait_timeout_secs(10) {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return None;
        }
    };
    Some((String::from_utf8_lossy(&out).into_owned(), status))
}

/// `wait` with an upper bound, so a hung child fails the test instead of hanging
/// the suite. Returns `Some(exited_normally)`.
trait WaitTimeout {
    fn wait_timeout_secs(&mut self, secs: u64) -> Option<bool>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout_secs(&mut self, secs: u64) -> Option<bool> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match self.try_wait() {
                Ok(Some(status)) => {
                    // A `Py_FatalError` abort leaves no exit code, only SIGABRT.
                    return Some(status.code().is_some());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        None
    }
}

/// A failed import at the prompt must raise, and the session must still be there
/// to run the next line.
///
/// Both halves matter. Reporting the error is not enough if the process is gone
/// afterwards, and surviving is not enough if the error never surfaced — so the
/// marker is printed by a line typed AFTER the failing import.
#[test]
fn failed_import_does_not_kill_the_repl() {
    let stdlib = empty_prefix();
    let Some((out, exited_normally)) =
        drive_repl(&["import os", "print('STILL_ALIVE', 6 * 7)"], &stdlib)
    else {
        eprintln!("repl_pty: SKIP — no pty available in this sandbox");
        return;
    };

    assert!(
        !out.contains("Fatal Python error"),
        "the interpreter aborted instead of raising. A failed import must never \
         end the process — it takes the user's interactive session with it.\n\
         --- pty transcript ---\n{out}"
    );
    assert!(
        out.contains("ModuleNotFoundError"),
        "a failed import must report a catchable ModuleNotFoundError.\n\
         --- pty transcript ---\n{out}"
    );
    assert!(
        out.contains("STILL_ALIVE 42"),
        "the line typed AFTER the failed import never ran, so the session did \
         not survive it.\n--- pty transcript ---\n{out}"
    );
    assert!(
        exited_normally,
        "the REPL was killed by a signal rather than exiting on EOF.\n\
         --- pty transcript ---\n{out}"
    );
}

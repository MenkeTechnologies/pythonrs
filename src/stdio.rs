//! The process's `stdout`/`stderr`, buffered exactly as CPython buffers them.
//!
//! CPython's `sys.stdout`/`sys.stderr` are a `TextIOWrapper` over a
//! `BufferedWriter` over a `FileIO` (`Python/pylifecycle.c::create_stdio`), and
//! WHEN bytes reach the file descriptor is a property of that stack, not of the
//! program: on a pipe or a file `stdout` holds everything until 128 KiB pile up
//! or the interpreter exits, while `stderr` goes out a line at a time. Merge the
//! two (`python prog.py > log 2>&1`) and the order of the log depends on it, as
//! does whether `os._exit` loses output. Writing straight to the fd — what
//! pythonrs did — interleaves the streams in program order, which CPython never
//! does on a pipe.
//!
//! Both layers are ported, because each one decides a flush point:
//!
//! * the text layer (`Modules/_io/textio.c::_io_TextIOWrapper_write_impl`)
//!   accumulates encoded writes in `pending` and hands them down once they
//!   reach `_CHUNK_SIZE` (8192), on a newline when line-buffered, or on every
//!   write when write-through;
//! * the buffer layer (`Modules/_io/bufferedio.c::_io_BufferedWriter_write_impl`)
//!   holds up to `buffer_size` bytes and, when a chunk does not fit, writes out
//!   what it holds before buffering (or directly writing) the chunk.
//!
//! The configuration is `create_stdio`'s: `-u`/`PYTHONUNBUFFERED` drops the
//! buffer layer and makes the text layer write-through; otherwise a stream is
//! line-buffered when it is a TTY or is `stderr`, and the buffer is sized by
//! `io.open`'s rule, `max(min(st_blksize, 8 MiB), DEFAULT_BUFFER_SIZE)`.
//!
//! The state is process-global rather than a `PyHost` field because the embedded
//! CPython's `sys.stdout` writes into the same stream (see `ffi.rs`), from code
//! that runs while the host is already borrowed.

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

/// `io.DEFAULT_BUFFER_SIZE` (128 KiB since CPython 3.14).
const DEFAULT_BUFFER_SIZE: usize = 128 * 1024;
/// The ceiling `io.open` puts on a device's `st_blksize`.
const MAX_BLKSIZE: usize = 8 * 1024 * 1024;
/// `TextIOWrapper._CHUNK_SIZE`.
const CHUNK_SIZE: usize = 8192;

/// Which standard stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One `TextIOWrapper` + `BufferedWriter` pair over a file descriptor.
struct TextStream {
    fd: i32,
    /// Set on first use, from the environment and the descriptor as they are
    /// then. CPython fixes the configuration at startup; the binary sets
    /// `PYTHONUNBUFFERED` for `-u` before any output.
    configured: bool,
    line_buffering: bool,
    write_through: bool,
    /// The text layer's encoded-but-not-yet-handed-down bytes.
    pending: Vec<u8>,
    /// The buffer layer's contents.
    buffer: Vec<u8>,
    /// `0` means no buffer layer at all (unbuffered: the text layer writes the
    /// raw descriptor).
    buffer_size: usize,
}

static STDOUT: Mutex<TextStream> = Mutex::new(TextStream::new(1));
static STDERR: Mutex<TextStream> = Mutex::new(TextStream::new(2));

impl TextStream {
    const fn new(fd: i32) -> Self {
        TextStream {
            fd,
            configured: false,
            line_buffering: false,
            write_through: false,
            pending: Vec::new(),
            buffer: Vec::new(),
            buffer_size: 0,
        }
    }

    /// `create_stdio`'s choice of buffering for this descriptor.
    fn configure(&mut self) {
        if self.configured {
            return;
        }
        self.configured = true;
        // `config_get_env` treats an empty variable as unset.
        let buffered = std::env::var_os("PYTHONUNBUFFERED").map_or(true, |v| v.is_empty());
        // SAFETY: `isatty`/`fstat` only query the descriptor.
        let isatty = unsafe { libc::isatty(self.fd) == 1 };
        self.write_through = !buffered;
        self.line_buffering = buffered && (isatty || self.fd == 2);
        self.buffer_size = if buffered {
            buffer_size_for(self.fd)
        } else {
            0
        };
    }

    /// `TextIOWrapper.write` for already-encoded text.
    fn write(&mut self, bytes: &[u8]) {
        self.configure();
        // Only consulted when line-buffered, as in `textio.c` (POSIX writes
        // `\n` untranslated, so `writetranslate` never asks).
        let needflush = self.line_buffering && bytes.iter().any(|&b| b == b'\n' || b == b'\r');
        // A large write first pushes out what is pending, so the two are not
        // concatenated (CPython gh-87426).
        if bytes.len() >= CHUNK_SIZE {
            self.write_pending();
        }
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= CHUNK_SIZE || needflush || self.write_through {
            self.write_pending();
        }
        if needflush {
            self.flush_buffer();
        }
    }

    /// `TextIOWrapper.flush`: hand pending text down, then flush the buffer.
    fn flush(&mut self) {
        self.configure();
        self.write_pending();
        self.flush_buffer();
    }

    /// `_textiowrapper_writeflush`.
    fn write_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.pending);
        self.buffered_write(&data);
    }

    /// `BufferedWriter.write` (or the raw `FileIO.write` when unbuffered).
    fn buffered_write(&mut self, data: &[u8]) {
        if self.buffer_size == 0 {
            raw_write(self.fd, data);
            return;
        }
        let avail = self.buffer_size - self.buffer.len();
        if data.len() <= avail && data.len() < self.buffer_size {
            self.buffer.extend_from_slice(data);
            return;
        }
        // It does not fit: write out what is held, then write the chunk
        // directly if it alone fills a buffer, else start a new buffer with it.
        self.flush_buffer();
        if data.len() >= self.buffer_size {
            raw_write(self.fd, data);
        } else {
            self.buffer.extend_from_slice(data);
        }
    }

    /// `_bufferedwriter_flush_unlocked`.
    fn flush_buffer(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.buffer);
        raw_write(self.fd, &data);
    }
}

/// `io.open`'s buffer size for a descriptor: its `st_blksize` clamped to
/// `[DEFAULT_BUFFER_SIZE, 8 MiB]`, `DEFAULT_BUFFER_SIZE` when `fstat` fails or
/// reports a block size of 1 or less (`FileIO._blksize`'s rule).
fn buffer_size_for(fd: i32) -> usize {
    // SAFETY: `st` is plain data `fstat` fills; it is read only on success.
    let blksize = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 && st.st_blksize > 1 {
            st.st_blksize as usize
        } else {
            DEFAULT_BUFFER_SIZE
        }
    };
    blksize.clamp(DEFAULT_BUFFER_SIZE, MAX_BLKSIZE)
}

/// Put bytes on the descriptor. Going through Rust's handles (and flushing
/// them) keeps any direct `println!`/`eprintln!` the runtime makes in order with
/// program output.
fn raw_write(fd: i32, data: &[u8]) {
    if fd == 2 {
        let mut e = std::io::stderr().lock();
        let _ = e.write_all(data);
        let _ = e.flush();
    } else {
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(data);
        let _ = o.flush();
    }
}

fn lock(stream: Stream) -> MutexGuard<'static, TextStream> {
    let m = match stream {
        Stream::Stdout => &STDOUT,
        Stream::Stderr => &STDERR,
    };
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `sys.stdout.write(s)` / `sys.stderr.write(s)` on the native stream.
pub fn write(stream: Stream, s: &str) {
    lock(stream).write(s.as_bytes());
}

/// The same write for text that arrives already encoded (UTF-8) — the
/// embedded CPython's write-through `TextIOWrapper` hands its bytes here.
pub fn write_bytes(stream: Stream, bytes: &[u8]) {
    lock(stream).write(bytes);
}

/// `sys.stdout.flush()` / `sys.stderr.flush()`.
pub fn flush(stream: Stream) {
    lock(stream).flush();
}

/// `pythonrun.c::flush_io`: `stderr`, then `stdout`. CPython runs it after a
/// script file (before reporting its exception) and after each interactive
/// statement.
pub fn flush_io() {
    flush(Stream::Stderr);
    flush(Stream::Stdout);
}

/// `pylifecycle.c::flush_std_files`: `stdout`, then `stderr` — what interpreter
/// shutdown does, so it is the last thing every run does.
pub fn flush_std_files() {
    flush(Stream::Stdout);
    flush(Stream::Stderr);
}

/// Make both streams write-through, as `-u` does, for a host that relays the
/// program's output as it is written (the DAP adapter's `output` events).
pub fn set_write_through() {
    for stream in [Stream::Stdout, Stream::Stderr] {
        let mut s = lock(stream);
        s.flush();
        s.write_through = true;
        s.line_buffering = false;
        s.buffer_size = 0;
    }
}

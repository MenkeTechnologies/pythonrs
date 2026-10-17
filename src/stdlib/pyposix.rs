//! `posix` — the syscall layer `os` is built on, ported from the semantics of
//! CPython's `Modules/posixmodule.c`.
//!
//! Native here: everything that is one libc call (file descriptors, links,
//! permissions, times, process control, `stat`). Python-shaped here: the struct
//! sequences, `DirEntry` and `scandir`, which are classes with behavior; they
//! live in [`module_source`] and are bound into the `posix` namespace by the
//! importer.
//!
//! Errors follow `path_error` / `posix_error`: `OSError` narrowed to the errno's
//! subclass, `strerror` from libc, and the filename (and second filename)
//! carried in the message so `parse_oserror` can rebuild the attributes.

use crate::builtins::{bind_named, KwStyle};
use crate::host::{self, with_host, IndexFit, NtMeta, PyHost, PyObj};
use fusevm::Value;
use std::ffi::{CStr, CString};

type R = Result<Value, String>;

/// The current thread's `errno`.
pub(crate) fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// libc's `strerror(eno)`, the text CPython puts in `OSError.strerror`.
pub fn strerror(eno: i32) -> String {
    // SAFETY: strerror returns a NUL-terminated string valid until the next call.
    unsafe { CStr::from_ptr(libc::strerror(eno)) }
        .to_string_lossy()
        .into_owned()
}

/// The error string for a failed syscall: `Class: [Errno N] strerror[: 'file'[ -> 'file2']]`.
pub fn os_error(eno: i32, file: Option<&str>, file2: Option<&str>) -> String {
    let mut s = format!(
        "{}: [Errno {eno}] {}",
        host::errno_exc_class(eno),
        strerror(eno)
    );
    if let Some(f) = file {
        s.push_str(&format!(": {}", host::quote_str(f)));
        if let Some(f2) = file2 {
            s.push_str(&format!(" -> {}", host::quote_str(f2)));
        }
    }
    s
}

/// Run a syscall, retrying on `EINTR` as CPython does, and return its result or
/// the errno.
fn syscall(mut f: impl FnMut() -> isize) -> Result<isize, i32> {
    loop {
        let r = f();
        if r >= 0 {
            return Ok(r);
        }
        let e = errno();
        if e != libc::EINTR {
            return Err(e);
        }
    }
}

/// A converted path argument: the C string, the text to show in errors, and
/// whether the caller passed `bytes` (so results keep the caller's type).
pub(crate) struct PathArg {
    pub(crate) c: CString,
    pub(crate) shown: String,
    pub(crate) bytes: bool,
}

fn str_of(h: &PyHost, v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        _ => h.as_str(v),
    }
}

/// CPython's `path_converter`: `str`, `bytes`, or an `os.PathLike` returning one.
pub(crate) fn path_of(fname: &str, argname: &str, v: &Value) -> Result<PathArg, String> {
    let direct = with_host(|h| match str_of(h, v) {
        Some(s) => Some((s.into_bytes(), false)),
        None => match h.get(v) {
            Some(PyObj::Bytes(b)) => Some((b.clone(), true)),
            _ => None,
        },
    });
    let (raw, bytes) = match direct {
        Some(x) => x,
        None => {
            let fsp = with_host(|h| h.get_attr(v, "__fspath__")).ok();
            let ty = with_host(|h| h.type_name(v));
            let not_path = || {
                host::type_error(&format!(
                    "{fname}: {argname} should be string, bytes or os.PathLike, not {ty}"
                ))
            };
            let Some(f) = fsp else {
                return Err(not_path());
            };
            let r = host::invoke(&f, vec![], vec![])?;
            match with_host(|h| match str_of(h, &r) {
                Some(s) => Some((s.into_bytes(), false)),
                None => match h.get(&r) {
                    Some(PyObj::Bytes(b)) => Some((b.clone(), true)),
                    _ => None,
                },
            }) {
                Some(x) => x,
                None => {
                    let rt = with_host(|h| h.type_name(&r));
                    return Err(host::type_error(&format!(
                        "expected {ty}.__fspath__() to return str or bytes, not {rt}"
                    )));
                }
            }
        }
    };
    let shown = String::from_utf8_lossy(&raw).into_owned();
    let c = CString::new(raw).map_err(|_| "ValueError: embedded null byte".to_string())?;
    Ok(PathArg { c, shown, bytes })
}

pub(crate) fn int_of(fname: &str, argname: &str, v: &Value) -> Result<i64, String> {
    with_host(|h| match h.index_fit(v) {
        IndexFit::Fits(n) => Ok(n),
        IndexFit::TooLarge(_) => {
            Err("OverflowError: Python int too large to convert to C long".to_string())
        }
        IndexFit::NotInt => Err(host::type_error(&format!(
            "{fname}() argument '{argname}' must be int, not {}",
            h.type_name(v)
        ))),
    })
}

/// A C `int` argument (file descriptors, flags, signals).
pub(crate) fn cint_of(fname: &str, argname: &str, v: &Value) -> Result<i32, String> {
    i32::try_from(int_of(fname, argname, v)?)
        .map_err(|_| "OverflowError: Python int too large to convert to C int".to_string())
}

fn bytes_val(b: Vec<u8>) -> Value {
    with_host(|h| h.alloc(PyObj::Bytes(b)))
}

fn str_val(s: String) -> Value {
    with_host(|h| h.new_str(s))
}

/// A path-typed result: `bytes` when the caller passed `bytes`, else `str`.
fn path_val(raw: Vec<u8>, bytes: bool) -> Value {
    if bytes {
        bytes_val(raw)
    } else {
        str_val(String::from_utf8_lossy(&raw).into_owned())
    }
}

fn buffer_of(v: &Value) -> Result<Vec<u8>, String> {
    with_host(|h| match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(b.clone()),
        Some(PyObj::Memoryview { .. }) => h.mv_bytes(v),
        _ => Err(host::type_error(&format!(
            "a bytes-like object is required, not '{}'",
            h.type_name(v)
        ))),
    })
}

fn is_none(v: &Option<Value>) -> bool {
    matches!(v, None | Some(Value::Undef))
}

/// `dir_fd` is not offered on this build (`os._have_functions` is empty), which
/// is what CPython reports for an unsupported keyword.
fn no_dir_fd(fname: &str, dir_fd: &Option<Value>) -> Result<(), String> {
    if is_none(dir_fd) {
        Ok(())
    } else {
        Err(format!(
            "NotImplementedError: {fname}: dir_fd unavailable on this platform"
        ))
    }
}

fn truthy(v: &Option<Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(v) => with_host(|h| h.truthy(v)),
    }
}

fn unit() -> R {
    Ok(Value::Undef)
}

/// A path or an open file descriptor, the `path_t(allow_fd=True)` shape.
enum PathOrFd {
    Path(PathArg),
    Fd(i32),
}

fn path_or_fd(fname: &str, v: &Value) -> Result<PathOrFd, String> {
    let is_int =
        with_host(|h| !matches!(v, Value::Bool(_)) && matches!(h.index_fit(v), IndexFit::Fits(_)))
            && !matches!(v, Value::Str(_));
    if is_int {
        Ok(PathOrFd::Fd(cint_of(fname, "path", v)?))
    } else {
        Ok(PathOrFd::Path(path_of(fname, "path", v)?))
    }
}

// ── stat ─────────────────────────────────────────────────────────────────────

/// `os.stat_result`: the ten-field sequence plus the float and nanosecond times
/// and the platform fields as attributes, exactly as `_pystat_fromstructstat`.
// The `stat` field widths differ by platform (`time_t`, `blkcnt_t`, `nsec`), so
// the casts are redundant on some targets and required on others.
#[allow(clippy::unnecessary_cast)]
fn stat_result(st: &libc::stat) -> Value {
    let ns = |sec: i64, nsec: i64| sec as i128 * 1_000_000_000 + nsec as i128;
    let secs = |sec: i64, nsec: i64| sec as f64 + nsec as f64 * 1e-9;
    let (a, m, c) = (
        (st.st_atime as i64, st.st_atime_nsec as i64),
        (st.st_mtime as i64, st.st_mtime_nsec as i64),
        (st.st_ctime as i64, st.st_ctime_nsec as i64),
    );
    let seq = [
        ("st_mode", st.st_mode as i64),
        ("st_ino", st.st_ino as i64),
        ("st_dev", st.st_dev as i64),
        ("st_nlink", st.st_nlink as i64),
        ("st_uid", st.st_uid as i64),
        ("st_gid", st.st_gid as i64),
        ("st_size", st.st_size),
        ("st_atime", a.0),
        ("st_mtime", m.0),
        ("st_ctime", c.0),
    ];
    with_host(|h| {
        let big = |h: &mut PyHost, n: i128| match i64::try_from(n) {
            Ok(n) => Value::Int(n),
            Err(_) => h.norm_big(num_bigint::BigInt::from(n)),
        };
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut extra: Vec<(String, Value)> = vec![
            ("st_atime".into(), Value::Float(secs(a.0, a.1))),
            ("st_mtime".into(), Value::Float(secs(m.0, m.1))),
            ("st_ctime".into(), Value::Float(secs(c.0, c.1))),
            ("st_atime_ns".into(), big(h, ns(a.0, a.1))),
            ("st_mtime_ns".into(), big(h, ns(m.0, m.1))),
            ("st_ctime_ns".into(), big(h, ns(c.0, c.1))),
            ("st_blksize".into(), Value::Int(st.st_blksize as i64)),
            ("st_blocks".into(), Value::Int(st.st_blocks as i64)),
            ("st_rdev".into(), Value::Int(st.st_rdev as i64)),
        ];
        #[cfg(target_os = "macos")]
        {
            extra.push(("st_flags".into(), Value::Int(st.st_flags as i64)));
            extra.push(("st_gen".into(), Value::Int(st.st_gen as i64)));
            extra.push((
                "st_birthtime".into(),
                Value::Float(secs(st.st_birthtime as i64, st.st_birthtime_nsec as i64)),
            ));
        }
        let vals: Vec<Value> = seq.iter().map(|(_, v)| Value::Int(*v)).collect();
        let tup = h.new_tuple(vals);
        if let Value::Obj(i) = tup {
            h.nt_meta.insert(
                i,
                NtMeta {
                    type_name: "os.stat_result".to_string(),
                    fields: seq.iter().map(|(k, _)| (*k).to_string()).collect(),
                    extra,
                },
            );
        }
        tup
    })
}

fn stat_impl(fname: &str, args: &[Value], kw: &[(String, Value)], lstat: bool) -> R {
    let [p, dir_fd, follow] = bind_named(
        fname,
        ["path", "dir_fd", "follow_symlinks"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd(fname, &dir_fd)?;
    let follow = !lstat && truthy(&follow, true);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    match path_or_fd(fname, &p.unwrap())? {
        PathOrFd::Fd(fd) => {
            syscall(|| unsafe { libc::fstat(fd, &mut st) } as isize)
                .map_err(|e| os_error(e, None, None))?;
        }
        PathOrFd::Path(p) => {
            syscall(|| unsafe {
                if follow {
                    libc::stat(p.c.as_ptr(), &mut st)
                } else {
                    libc::lstat(p.c.as_ptr(), &mut st)
                }
            } as isize)
            .map_err(|e| os_error(e, Some(&p.shown), None))?;
        }
    }
    Ok(stat_result(&st))
}

fn fstat(args: &[Value], kw: &[(String, Value)]) -> R {
    let [fd] = bind_named("fstat", ["fd"], 0, 1, KwStyle::Unexpected, args, kw)?;
    let fd = cint_of("fstat", "fd", &fd.unwrap())?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    syscall(|| unsafe { libc::fstat(fd, &mut st) } as isize)
        .map_err(|e| os_error(e, None, None))?;
    Ok(stat_result(&st))
}

// ── file descriptors ─────────────────────────────────────────────────────────

fn open(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, flags, mode, dir_fd] = bind_named(
        "open",
        ["path", "flags", "mode", "dir_fd"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("open", &dir_fd)?;
    let path = path_of("open", "path", &p.unwrap())?;
    let flags = cint_of("open", "flags", &flags.unwrap())?;
    let mode = match mode {
        Some(m) => cint_of("open", "mode", &m)?,
        None => 0o777,
    };
    // Every descriptor Python opens is non-inheritable (PEP 446).
    let fd = syscall(|| unsafe {
        libc::open(
            path.c.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        ) as isize
    })
    .map_err(|e| os_error(e, Some(&path.shown), None))?;
    Ok(Value::Int(fd as i64))
}

fn close(args: &[Value]) -> R {
    let fd = cint_of("close", "fd", args.first().ok_or_else(missing)?)?;
    if unsafe { libc::close(fd) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn missing() -> String {
    host::type_error("missing required argument")
}

fn read(args: &[Value]) -> R {
    let [fd, n] = two("read", args)?;
    let fd = cint_of("read", "fd", fd)?;
    let n = int_of("read", "length", n)?;
    if n < 0 {
        return Err(os_error(libc::EINVAL, None, None));
    }
    let mut buf = vec![0u8; n as usize];
    let got = syscall(|| unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) })
        .map_err(|e| os_error(e, None, None))?;
    buf.truncate(got as usize);
    Ok(bytes_val(buf))
}

fn write(args: &[Value]) -> R {
    let [fd, data] = two("write", args)?;
    let fd = cint_of("write", "fd", fd)?;
    let data = buffer_of(data)?;
    let n = syscall(|| unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) })
        .map_err(|e| os_error(e, None, None))?;
    Ok(Value::Int(n as i64))
}

fn two<'a>(fname: &str, args: &'a [Value]) -> Result<[&'a Value; 2], String> {
    match args {
        [a, b] => Ok([a, b]),
        _ => Err(host::type_error(&format!(
            "{fname}() takes exactly 2 arguments ({} given)",
            args.len()
        ))),
    }
}

fn lseek(args: &[Value]) -> R {
    let [fd, pos, how] = match args {
        [a, b, c] => [a, b, c],
        _ => {
            return Err(host::type_error(&format!(
                "lseek() takes exactly 3 arguments ({} given)",
                args.len()
            )))
        }
    };
    let fd = cint_of("lseek", "fd", fd)?;
    let pos = int_of("lseek", "position", pos)?;
    let how = cint_of("lseek", "whence", how)?;
    let r = unsafe { libc::lseek(fd, pos as libc::off_t, how) };
    if r < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(r as i64))
}

fn dup(args: &[Value]) -> R {
    let fd = cint_of("dup", "fd", args.first().ok_or_else(missing)?)?;
    let r = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if r < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(r as i64))
}

fn dup2(args: &[Value], kw: &[(String, Value)]) -> R {
    let [fd, fd2, inheritable] = bind_named(
        "dup2",
        ["fd", "fd2", "inheritable"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    let fd = cint_of("dup2", "fd", &fd.unwrap())?;
    let fd2 = cint_of("dup2", "fd2", &fd2.unwrap())?;
    let r = unsafe { libc::dup2(fd, fd2) };
    if r < 0 {
        return Err(os_error(errno(), None, None));
    }
    if !truthy(&inheritable, true) {
        unsafe { libc::fcntl(r, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(Value::Int(r as i64))
}

fn pipe() -> R {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(with_host(|h| {
        h.new_tuple(vec![Value::Int(fds[0] as i64), Value::Int(fds[1] as i64)])
    }))
}

fn fsync(args: &[Value]) -> R {
    let fd = cint_of("fsync", "fd", args.first().ok_or_else(missing)?)?;
    syscall(|| unsafe { libc::fsync(fd) } as isize).map_err(|e| os_error(e, None, None))?;
    unit()
}

fn ftruncate(args: &[Value]) -> R {
    let [fd, len] = two("ftruncate", args)?;
    let fd = cint_of("ftruncate", "fd", fd)?;
    let len = int_of("ftruncate", "length", len)?;
    syscall(|| unsafe { libc::ftruncate(fd, len as libc::off_t) } as isize)
        .map_err(|e| os_error(e, None, None))?;
    unit()
}

fn truncate(args: &[Value]) -> R {
    let [p, len] = two("truncate", args)?;
    let len = int_of("truncate", "length", len)?;
    match path_or_fd("truncate", p)? {
        PathOrFd::Fd(fd) => syscall(|| unsafe { libc::ftruncate(fd, len as libc::off_t) } as isize)
            .map_err(|e| os_error(e, None, None))?,
        PathOrFd::Path(p) => {
            syscall(|| unsafe { libc::truncate(p.c.as_ptr(), len as libc::off_t) } as isize)
                .map_err(|e| os_error(e, Some(&p.shown), None))?
        }
    };
    unit()
}

fn set_inheritable(args: &[Value]) -> R {
    let [fd, inh] = two("set_inheritable", args)?;
    let fd = cint_of("set_inheritable", "fd", fd)?;
    let inherit = with_host(|h| h.truthy(inh));
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(os_error(errno(), None, None));
    }
    let new = if inherit {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, new) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn get_inheritable(args: &[Value]) -> R {
    let fd = cint_of("get_inheritable", "fd", args.first().ok_or_else(missing)?)?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Bool(flags & libc::FD_CLOEXEC == 0))
}

fn isatty(args: &[Value]) -> R {
    let fd = cint_of("isatty", "fd", args.first().ok_or_else(missing)?)?;
    Ok(Value::Bool(unsafe { libc::isatty(fd) } == 1))
}

/// `(columns, lines)` of the terminal on `fd`; `os.get_terminal_size` wraps it.
fn terminal_size_raw(args: &[Value]) -> R {
    let fd = match args.first() {
        Some(v) => cint_of("get_terminal_size", "fd", v)?,
        None => libc::STDOUT_FILENO,
    };
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(with_host(|h| {
        h.new_tuple(vec![
            Value::Int(ws.ws_col as i64),
            Value::Int(ws.ws_row as i64),
        ])
    }))
}

// ── directories and links ────────────────────────────────────────────────────

fn mkdir(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, mode, dir_fd] = bind_named(
        "mkdir",
        ["path", "mode", "dir_fd"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("mkdir", &dir_fd)?;
    let path = path_of("mkdir", "path", &p.unwrap())?;
    let mode = match mode {
        Some(m) => cint_of("mkdir", "mode", &m)?,
        None => 0o777,
    };
    syscall(|| unsafe { libc::mkdir(path.c.as_ptr(), mode as libc::mode_t) } as isize)
        .map_err(|e| os_error(e, Some(&path.shown), None))?;
    unit()
}

/// `rmdir` / `unlink` / `remove` / `chdir`: one path in, no result.
fn path_op(
    fname: &str,
    args: &[Value],
    kw: &[(String, Value)],
    op: unsafe extern "C" fn(*const libc::c_char) -> libc::c_int,
) -> R {
    let [p, dir_fd] = bind_named(
        fname,
        ["path", "dir_fd"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd(fname, &dir_fd)?;
    let path = path_of(fname, "path", &p.unwrap())?;
    syscall(|| unsafe { op(path.c.as_ptr()) } as isize)
        .map_err(|e| os_error(e, Some(&path.shown), None))?;
    unit()
}

fn chdir(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p] = bind_named("chdir", ["path"], 0, 1, KwStyle::Unexpected, args, kw)?;
    match path_or_fd("chdir", &p.unwrap())? {
        PathOrFd::Fd(fd) => {
            syscall(|| unsafe { libc::fchdir(fd) } as isize).map_err(|e| os_error(e, None, None))?
        }
        PathOrFd::Path(p) => syscall(|| unsafe { libc::chdir(p.c.as_ptr()) } as isize)
            .map_err(|e| os_error(e, Some(&p.shown), None))?,
    };
    unit()
}

fn rename(fname: &str, args: &[Value], kw: &[(String, Value)]) -> R {
    let [src, dst, sfd, dfd] = bind_named(
        fname,
        ["src", "dst", "src_dir_fd", "dst_dir_fd"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd(fname, &sfd)?;
    no_dir_fd(fname, &dfd)?;
    let src = path_of(fname, "src", &src.unwrap())?;
    let dst = path_of(fname, "dst", &dst.unwrap())?;
    syscall(|| unsafe { libc::rename(src.c.as_ptr(), dst.c.as_ptr()) } as isize)
        .map_err(|e| os_error(e, Some(&src.shown), Some(&dst.shown)))?;
    unit()
}

fn link(args: &[Value], kw: &[(String, Value)]) -> R {
    let [src, dst, sfd, dfd, follow] = bind_named(
        "link",
        ["src", "dst", "src_dir_fd", "dst_dir_fd", "follow_symlinks"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("link", &sfd)?;
    no_dir_fd("link", &dfd)?;
    let src = path_of("link", "src", &src.unwrap())?;
    let dst = path_of("link", "dst", &dst.unwrap())?;
    let flag = if truthy(&follow, true) {
        libc::AT_SYMLINK_FOLLOW
    } else {
        0
    };
    syscall(|| unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            src.c.as_ptr(),
            libc::AT_FDCWD,
            dst.c.as_ptr(),
            flag,
        )
    } as isize)
    .map_err(|e| os_error(e, Some(&src.shown), Some(&dst.shown)))?;
    unit()
}

fn symlink(args: &[Value], kw: &[(String, Value)]) -> R {
    let [src, dst, _is_dir, dir_fd] = bind_named(
        "symlink",
        ["src", "dst", "target_is_directory", "dir_fd"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("symlink", &dir_fd)?;
    let src = path_of("symlink", "src", &src.unwrap())?;
    let dst = path_of("symlink", "dst", &dst.unwrap())?;
    syscall(|| unsafe { libc::symlink(src.c.as_ptr(), dst.c.as_ptr()) } as isize)
        .map_err(|e| os_error(e, Some(&src.shown), Some(&dst.shown)))?;
    unit()
}

fn readlink(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, dir_fd] = bind_named(
        "readlink",
        ["path", "dir_fd"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("readlink", &dir_fd)?;
    let path = path_of("readlink", "path", &p.unwrap())?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let n =
        syscall(|| unsafe { libc::readlink(path.c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) })
            .map_err(|e| os_error(e, Some(&path.shown), None))?;
    buf.truncate(n as usize);
    Ok(path_val(buf, path.bytes))
}

fn listdir(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p] = bind_named("listdir", ["path"], 0, 0, KwStyle::Unexpected, args, kw)?;
    let (dir, bytes) = match p {
        Some(p) if !matches!(p, Value::Undef) => {
            let a = path_of("listdir", "path", &p)?;
            (a.shown.clone(), a.bytes)
        }
        _ => (".".to_string(), false),
    };
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| os_error(e.raw_os_error().unwrap_or(libc::EIO), Some(&dir), None))?;
    let mut names = Vec::new();
    for entry in rd {
        let entry =
            entry.map_err(|e| os_error(e.raw_os_error().unwrap_or(libc::EIO), Some(&dir), None))?;
        use std::os::unix::ffi::OsStrExt;
        names.push(path_val(entry.file_name().as_bytes().to_vec(), bytes));
    }
    Ok(with_host(|h| h.new_list(names)))
}

fn access(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, mode, dir_fd, eff, follow] = bind_named(
        "access",
        ["path", "mode", "dir_fd", "effective_ids", "follow_symlinks"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("access", &dir_fd)?;
    let path = path_of("access", "path", &p.unwrap())?;
    let mode = cint_of("access", "mode", &mode.unwrap())?;
    let mut flags = 0;
    if truthy(&eff, false) {
        flags |= libc::AT_EACCESS;
    }
    if !truthy(&follow, true) {
        flags |= libc::AT_SYMLINK_NOFOLLOW;
    }
    let r = unsafe { libc::faccessat(libc::AT_FDCWD, path.c.as_ptr(), mode, flags) };
    Ok(Value::Bool(r == 0))
}

fn chmod(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, mode, dir_fd, follow] = bind_named(
        "chmod",
        ["path", "mode", "dir_fd", "follow_symlinks"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("chmod", &dir_fd)?;
    let mode = cint_of("chmod", "mode", &mode.unwrap())? as libc::mode_t;
    match path_or_fd("chmod", &p.unwrap())? {
        PathOrFd::Fd(fd) => syscall(|| unsafe { libc::fchmod(fd, mode) } as isize)
            .map_err(|e| os_error(e, None, None))?,
        PathOrFd::Path(p) => {
            let flag = if truthy(&follow, true) {
                0
            } else {
                libc::AT_SYMLINK_NOFOLLOW
            };
            syscall(|| unsafe { libc::fchmodat(libc::AT_FDCWD, p.c.as_ptr(), mode, flag) } as isize)
                .map_err(|e| os_error(e, Some(&p.shown), None))?
        }
    };
    unit()
}

fn fchmod(args: &[Value], kw: &[(String, Value)]) -> R {
    let [fd, mode] = bind_named(
        "fchmod",
        ["fd", "mode"],
        0,
        2,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    let fd = cint_of("fchmod", "fd", &fd.unwrap())?;
    let mode = cint_of("fchmod", "mode", &mode.unwrap())? as libc::mode_t;
    syscall(|| unsafe { libc::fchmod(fd, mode) } as isize).map_err(|e| os_error(e, None, None))?;
    unit()
}

// ── utime ────────────────────────────────────────────────────────────────────

/// A timestamp as a `timespec`, flooring a float the way `_PyTime_ObjectToTimespec`
/// does with `ROUND_FLOOR`.
fn timespec_of(v: &Value) -> Result<libc::timespec, String> {
    if let IndexFit::Fits(n) = with_host(|h| h.index_fit(v)) {
        return Ok(libc::timespec {
            tv_sec: n as libc::time_t,
            tv_nsec: 0,
        });
    }
    match v {
        Value::Float(f) => {
            if !f.is_finite() {
                return Err("ValueError: Invalid value NaN (not a number)".to_string());
            }
            let sec = f.floor();
            let nsec = ((f - sec) * 1e9).floor().clamp(0.0, 999_999_999.0);
            Ok(libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: nsec as _,
            })
        }
        _ => Err(host::type_error(&format!(
            "'{}' object cannot be interpreted as an integer or float",
            with_host(|h| h.type_name(v))
        ))),
    }
}

fn utime(args: &[Value], kw: &[(String, Value)]) -> R {
    let [p, times, ns, dir_fd, follow] = bind_named(
        "utime",
        ["path", "times", "ns", "dir_fd", "follow_symlinks"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    no_dir_fd("utime", &dir_fd)?;
    let (times, ns) = (times.filter(|v| !matches!(v, Value::Undef)), ns);
    let pair = |v: &Value| -> Option<Vec<Value>> {
        with_host(|h| match h.get(v) {
            Some(PyObj::Tuple(t)) if t.len() == 2 => Some(t.to_vec()),
            _ => None,
        })
    };
    let ts: [libc::timespec; 2] = match (times, ns) {
        (Some(_), Some(_)) => {
            return Err(
                "ValueError: utime: you may specify either 'times' or 'ns' but not both"
                    .to_string(),
            )
        }
        (Some(t), None) => {
            let Some(p) = pair(&t) else {
                return Err(host::type_error(
                    "utime: 'times' must be either a tuple of two ints or None",
                ));
            };
            [timespec_of(&p[0])?, timespec_of(&p[1])?]
        }
        (None, Some(n)) => {
            let Some(p) = pair(&n) else {
                return Err(host::type_error("utime: 'ns' must be a tuple of two ints"));
            };
            let split = |v: &Value| -> Result<libc::timespec, String> {
                let n = int_of("utime", "ns", v)?;
                Ok(libc::timespec {
                    tv_sec: n.div_euclid(1_000_000_000) as libc::time_t,
                    tv_nsec: n.rem_euclid(1_000_000_000) as _,
                })
            };
            [split(&p[0])?, split(&p[1])?]
        }
        (None, None) => {
            [libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW as _,
            }; 2]
        }
    };
    match path_or_fd("utime", &p.unwrap())? {
        PathOrFd::Fd(fd) => syscall(|| unsafe { libc::futimens(fd, ts.as_ptr()) } as isize)
            .map_err(|e| os_error(e, None, None))?,
        PathOrFd::Path(p) => {
            let flag = if truthy(&follow, true) {
                0
            } else {
                libc::AT_SYMLINK_NOFOLLOW
            };
            syscall(
                || unsafe { libc::utimensat(libc::AT_FDCWD, p.c.as_ptr(), ts.as_ptr(), flag) }
                    as isize,
            )
            .map_err(|e| os_error(e, Some(&p.shown), None))?
        }
    };
    unit()
}

// ── processes ────────────────────────────────────────────────────────────────

fn kill(args: &[Value]) -> R {
    let [pid, sig] = two("kill", args)?;
    let pid = int_of("kill", "pid", pid)?;
    let sig = cint_of("kill", "signal", sig)?;
    if unsafe { libc::kill(pid as libc::pid_t, sig) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn killpg(args: &[Value]) -> R {
    let [pgid, sig] = two("killpg", args)?;
    let pgid = int_of("killpg", "pgid", pgid)?;
    let sig = cint_of("killpg", "signal", sig)?;
    if unsafe { libc::killpg(pgid as libc::pid_t, sig) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn waitpid(args: &[Value]) -> R {
    let [pid, opts] = two("waitpid", args)?;
    let pid = int_of("waitpid", "pid", pid)? as libc::pid_t;
    let opts = cint_of("waitpid", "options", opts)?;
    let mut status = 0;
    let r = syscall(|| unsafe { libc::waitpid(pid, &mut status, opts) } as isize)
        .map_err(|e| os_error(e, None, None))?;
    Ok(with_host(|h| {
        h.new_tuple(vec![Value::Int(r as i64), Value::Int(status as i64)])
    }))
}

fn wait() -> R {
    waitpid(&[Value::Int(-1), Value::Int(0)])
}

fn fork() -> R {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(pid as i64))
}

/// `WIFEXITED` and friends over a wait status.
fn wait_status(fname: &str, args: &[Value]) -> R {
    let status = cint_of(fname, "status", args.first().ok_or_else(missing)?)?;
    Ok(match fname {
        "WIFEXITED" => Value::Bool(libc::WIFEXITED(status)),
        "WEXITSTATUS" => Value::Int(libc::WEXITSTATUS(status) as i64),
        "WIFSIGNALED" => Value::Bool(libc::WIFSIGNALED(status)),
        "WTERMSIG" => Value::Int(libc::WTERMSIG(status) as i64),
        "WIFSTOPPED" => Value::Bool(libc::WIFSTOPPED(status)),
        "WSTOPSIG" => Value::Int(libc::WSTOPSIG(status) as i64),
        "WCOREDUMP" => Value::Bool(libc::WCOREDUMP(status)),
        _ => Value::Bool(false),
    })
}

/// `os.waitstatus_to_exitcode`.
fn waitstatus_to_exitcode(args: &[Value], kw: &[(String, Value)]) -> R {
    let [st] = bind_named(
        "waitstatus_to_exitcode",
        ["status"],
        0,
        1,
        KwStyle::Unexpected,
        args,
        kw,
    )?;
    let status = cint_of("waitstatus_to_exitcode", "status", &st.unwrap())?;
    if libc::WIFEXITED(status) {
        Ok(Value::Int(libc::WEXITSTATUS(status) as i64))
    } else if libc::WIFSIGNALED(status) {
        Ok(Value::Int(-(libc::WTERMSIG(status) as i64)))
    } else {
        Err(format!("ValueError: invalid wait status: {status}"))
    }
}

/// An `argv`/`envp` element: `str`, `bytes` or `os.PathLike`.
fn exec_string(fname: &str, v: &Value) -> Result<CString, String> {
    Ok(path_of(fname, "args", v)?.c)
}

fn exec_argv(fname: &str, v: &Value) -> Result<Vec<CString>, String> {
    let items = with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => Some(l.to_vec()),
        Some(PyObj::Tuple(t)) => Some(t.to_vec()),
        _ => None,
    })
    .ok_or_else(|| host::type_error(&format!("{fname}() arg 2 must be a tuple or list")))?;
    if items.is_empty() {
        return Err(format!("ValueError: {fname}() arg 2 must not be empty"));
    }
    let argv: Vec<CString> = items
        .iter()
        .map(|a| exec_string(fname, a))
        .collect::<Result<_, _>>()?;
    if argv[0].as_bytes().is_empty() {
        return Err(format!(
            "ValueError: {fname}() arg 2 first element cannot be empty"
        ));
    }
    Ok(argv)
}

fn c_ptrs(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn execv(args: &[Value]) -> R {
    let [path, argv] = two("execv", args)?;
    let path = path_of("execv", "path", path)?;
    let argv = exec_argv("execv", argv)?;
    let ptrs = c_ptrs(&argv);
    unsafe { libc::execv(path.c.as_ptr(), ptrs.as_ptr()) };
    Err(os_error(errno(), Some(&path.shown), None))
}

fn execve(args: &[Value]) -> R {
    let (path, argv, env) = match args {
        [a, b, c] => (a, b, c),
        _ => {
            return Err(host::type_error(&format!(
                "execve() takes exactly 3 arguments ({} given)",
                args.len()
            )))
        }
    };
    let path = path_of("execve", "path", path)?;
    let argv = exec_argv("execve", argv)?;
    let pairs = with_host(|h| match h.get(env) {
        Some(PyObj::Dict(d)) => Some(d.values().cloned().collect::<Vec<_>>()),
        _ => None,
    })
    .ok_or_else(|| host::type_error("execve: environment must be a mapping object"))?;
    let mut envp: Vec<CString> = Vec::new();
    for (k, v) in pairs {
        let mut kv = exec_string("execve", &k)?.into_bytes();
        if kv.is_empty() || kv.contains(&b'=') {
            return Err("ValueError: illegal environment variable name".to_string());
        }
        kv.push(b'=');
        kv.extend(exec_string("execve", &v)?.as_bytes());
        envp.push(CString::new(kv).map_err(|_| "ValueError: embedded null byte".to_string())?);
    }
    let (argp, envp) = (c_ptrs(&argv), c_ptrs(&envp));
    unsafe { libc::execve(path.c.as_ptr(), argp.as_ptr(), envp.as_ptr()) };
    Err(os_error(errno(), Some(&path.shown), None))
}

fn pid_call(fname: &str, args: &[Value], f: unsafe extern "C" fn(libc::pid_t) -> libc::pid_t) -> R {
    let pid = int_of(fname, "pid", args.first().ok_or_else(missing)?)? as libc::pid_t;
    let r = unsafe { f(pid) };
    if r < 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(r as i64))
}

fn setsid() -> R {
    if unsafe { libc::setsid() } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn setpgid(args: &[Value]) -> R {
    let [pid, pgrp] = two("setpgid", args)?;
    let pid = int_of("setpgid", "pid", pid)? as libc::pid_t;
    let pgrp = int_of("setpgid", "pgrp", pgrp)? as libc::pid_t;
    if unsafe { libc::setpgid(pid, pgrp) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    unit()
}

fn getlogin() -> R {
    let p = unsafe { libc::getlogin() };
    if p.is_null() {
        return Err(os_error(errno(), None, None));
    }
    Ok(str_val(
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned(),
    ))
}

fn nice(args: &[Value]) -> R {
    let inc = cint_of("nice", "increment", args.first().ok_or_else(missing)?)?;
    // nice(2) may legitimately return -1, so errno is the only error signal.
    unsafe { *errno_location() = 0 };
    let r = unsafe { libc::nice(inc) };
    if r == -1 && errno() != 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(r as i64))
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut i32 {
    libc::__error()
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno_location() -> *mut i32 {
    libc::__errno_location()
}

// ── system information ───────────────────────────────────────────────────────

fn field(buf: &[libc::c_char]) -> String {
    // SAFETY: the kernel NUL-terminates each utsname field.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// The five `uname` strings; `os.uname` wraps them in `uname_result`.
fn uname_raw() -> R {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } < 0 {
        return Err(os_error(errno(), None, None));
    }
    let parts = [
        field(&u.sysname),
        field(&u.nodename),
        field(&u.release),
        field(&u.version),
        field(&u.machine),
    ];
    Ok(with_host(|h| {
        let items = parts.into_iter().map(|s| h.new_str(s)).collect();
        h.new_tuple(items)
    }))
}

/// The five `times` values in seconds; `os.times` wraps them in `times_result`.
fn times_raw() -> R {
    let mut t: libc::tms = unsafe { std::mem::zeroed() };
    let elapsed = unsafe { libc::times(&mut t) };
    if elapsed == (-1i64) as libc::clock_t {
        return Err(os_error(errno(), None, None));
    }
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let secs = |c: libc::clock_t| Value::Float(c as f64 / hz);
    let vals = [
        secs(t.tms_utime),
        secs(t.tms_stime),
        secs(t.tms_cutime),
        secs(t.tms_cstime),
        secs(elapsed),
    ];
    Ok(with_host(|h| h.new_tuple(vals.to_vec())))
}

/// The `sysconf` names `os.sysconf_names` offers, shared by both builds' libc.
const SYSCONF_NAMES: &[(&str, libc::c_int)] = &[
    ("SC_ARG_MAX", libc::_SC_ARG_MAX),
    ("SC_CHILD_MAX", libc::_SC_CHILD_MAX),
    ("SC_CLK_TCK", libc::_SC_CLK_TCK),
    ("SC_NGROUPS_MAX", libc::_SC_NGROUPS_MAX),
    ("SC_OPEN_MAX", libc::_SC_OPEN_MAX),
    ("SC_PAGESIZE", libc::_SC_PAGESIZE),
    ("SC_PAGE_SIZE", libc::_SC_PAGESIZE),
    ("SC_NPROCESSORS_CONF", libc::_SC_NPROCESSORS_CONF),
    ("SC_NPROCESSORS_ONLN", libc::_SC_NPROCESSORS_ONLN),
    ("SC_HOST_NAME_MAX", libc::_SC_HOST_NAME_MAX),
    ("SC_LOGIN_NAME_MAX", libc::_SC_LOGIN_NAME_MAX),
];

fn sysconf(args: &[Value]) -> R {
    let name = args.first().ok_or_else(missing)?;
    let key = with_host(|h| str_of(h, name));
    let code = match key {
        Some(s) => SYSCONF_NAMES
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, c)| *c)
            .ok_or_else(|| "ValueError: unrecognized configuration name".to_string())?,
        None => cint_of("sysconf", "name", name)?,
    };
    unsafe { *errno_location() = 0 };
    let r = unsafe { libc::sysconf(code) };
    if r == -1 && errno() != 0 {
        return Err(os_error(errno(), None, None));
    }
    Ok(Value::Int(r as i64))
}

/// `posix.sysconf_names` as a ready dict.
pub fn sysconf_names(h: &mut PyHost) -> Value {
    let mut d = crate::host::DictMap::default();
    for (n, c) in SYSCONF_NAMES {
        let k = h.new_str(*n);
        d.insert(
            host::PKey::Str((*n).to_string()),
            (k, Value::Int(*c as i64)),
        );
    }
    h.alloc(PyObj::Dict(d))
}

fn system(args: &[Value]) -> R {
    let cmd = path_of("system", "command", args.first().ok_or_else(missing)?)?;
    Ok(Value::Int(unsafe { libc::system(cmd.c.as_ptr()) } as i64))
}

fn strerror_fn(args: &[Value]) -> R {
    let eno = cint_of("strerror", "code", args.first().ok_or_else(missing)?)?;
    Ok(str_val(strerror(eno)))
}

/// Names bound in the `posix` namespace by this module, beyond the table the
/// importer keeps.
pub const NAMES: &[&str] = &[
    "open",
    "close",
    "read",
    "write",
    "lseek",
    "dup",
    "dup2",
    "pipe",
    "fsync",
    "ftruncate",
    "truncate",
    "set_inheritable",
    "get_inheritable",
    "isatty",
    "stat",
    "lstat",
    "fstat",
    "mkdir",
    "rmdir",
    "unlink",
    "remove",
    "chdir",
    "rename",
    "replace",
    "link",
    "symlink",
    "readlink",
    "listdir",
    "access",
    "chmod",
    "fchmod",
    "utime",
    "kill",
    "killpg",
    "waitpid",
    "wait",
    "_fork",
    "execv",
    "execve",
    "setsid",
    "setpgid",
    "getpgid",
    "getsid",
    "getlogin",
    "nice",
    "sysconf",
    "system",
    "strerror",
    "WIFEXITED",
    "WEXITSTATUS",
    "WIFSIGNALED",
    "WTERMSIG",
    "WIFSTOPPED",
    "WSTOPSIG",
    "WCOREDUMP",
    "waitstatus_to_exitcode",
    "_terminal_size",
    "_uname",
    "_times",
];

/// Constants beyond the libc ones the importer already lists.
pub fn constants() -> Vec<(&'static str, i64)> {
    vec![
        ("O_NOFOLLOW", libc::O_NOFOLLOW as i64),
        ("O_NOCTTY", libc::O_NOCTTY as i64),
        ("O_SYNC", libc::O_SYNC as i64),
        ("O_DIRECTORY", libc::O_DIRECTORY as i64),
        ("O_NDELAY", libc::O_NONBLOCK as i64),
        ("WUNTRACED", libc::WUNTRACED as i64),
        ("EX_USAGE", 64),
        ("EX_DATAERR", 65),
        ("EX_NOINPUT", 66),
        ("EX_NOUSER", 67),
        ("EX_NOHOST", 68),
        ("EX_UNAVAILABLE", 69),
        ("EX_SOFTWARE", 70),
        ("EX_OSERR", 71),
        ("EX_OSFILE", 72),
        ("EX_CANTCREAT", 73),
        ("EX_IOERR", 74),
        ("EX_TEMPFAIL", 75),
        ("EX_PROTOCOL", 76),
        ("EX_NOPERM", 77),
        ("EX_CONFIG", 78),
        ("F_LOCK", libc::F_LOCK as i64),
        ("F_TLOCK", libc::F_TLOCK as i64),
        ("F_ULOCK", libc::F_ULOCK as i64),
        ("F_TEST", libc::F_TEST as i64),
        ("P_NOWAIT", 1),
        ("P_NOWAITO", 1),
        ("P_WAIT", 0),
    ]
}

/// Dispatch `posix.<name>`; `None` leaves the name to the older table in
/// `builtins::call_posix`.
pub fn call(name: &str, args: &[Value], kw: &[(String, Value)]) -> Option<R> {
    Some(match name {
        "open" => open(args, kw),
        "close" => close(args),
        "read" => read(args),
        "write" => write(args),
        "lseek" => lseek(args),
        "dup" => dup(args),
        "dup2" => dup2(args, kw),
        "pipe" => pipe(),
        "fsync" => fsync(args),
        "ftruncate" => ftruncate(args),
        "truncate" => truncate(args),
        "set_inheritable" => set_inheritable(args),
        "get_inheritable" => get_inheritable(args),
        "isatty" => isatty(args),
        "stat" => stat_impl("stat", args, kw, false),
        "lstat" => stat_impl("lstat", args, kw, true),
        "fstat" => fstat(args, kw),
        "mkdir" => mkdir(args, kw),
        "rmdir" => path_op("rmdir", args, kw, libc::rmdir),
        "unlink" | "remove" => path_op(name, args, kw, libc::unlink),
        "chdir" => chdir(args, kw),
        "rename" | "replace" => rename(name, args, kw),
        "link" => link(args, kw),
        "symlink" => symlink(args, kw),
        "readlink" => readlink(args, kw),
        "listdir" => listdir(args, kw),
        "access" => access(args, kw),
        "chmod" => chmod(args, kw),
        "fchmod" => fchmod(args, kw),
        "utime" => utime(args, kw),
        "kill" => kill(args),
        "killpg" => killpg(args),
        "waitpid" => waitpid(args),
        "wait" => wait(),
        "_fork" => fork(),
        "execv" => execv(args),
        "execve" => execve(args),
        "setsid" => setsid(),
        "setpgid" => setpgid(args),
        "getpgid" => pid_call(name, args, libc::getpgid),
        "getsid" => pid_call(name, args, libc::getsid),
        "getlogin" => getlogin(),
        "nice" => nice(args),
        "sysconf" => sysconf(args),
        "system" => system(args),
        "strerror" => strerror_fn(args),
        "WIFEXITED" | "WEXITSTATUS" | "WIFSIGNALED" | "WTERMSIG" | "WIFSTOPPED" | "WSTOPSIG"
        | "WCOREDUMP" => wait_status(name, args),
        "waitstatus_to_exitcode" => waitstatus_to_exitcode(args, kw),
        "_terminal_size" => terminal_size_raw(args),
        "_uname" => uname_raw(),
        "_times" => times_raw(),
        _ => return None,
    })
}

/// The Python source of the `posix._extras` helper: the struct sequences,
/// `DirEntry` and `scandir`, bound into the `posix` namespace by the importer.
pub fn module_source() -> &'static str {
    r#""""Python-shaped parts of the native `posix` module."""


def _structseq(typename, fields):
    module, _, name = typename.rpartition('.')
    count = len(fields)

    def __new__(cls, seq):
        seq = tuple(seq)
        if len(seq) != count:
            raise TypeError('%s() takes a %d-sequence (%d-sequence given)'
                            % (typename, count, len(seq)))
        return tuple.__new__(cls, seq)

    def __repr__(self):
        return '%s(%s)' % (typename, ', '.join(
            '%s=%r' % (f, v) for f, v in zip(fields, self)))

    def __reduce__(self):
        return (type(self), (tuple(self),))

    ns = {
        '__slots__': (),
        '__new__': __new__,
        '__repr__': __repr__,
        '__reduce__': __reduce__,
        '__module__': module,
        'n_sequence_fields': count,
        'n_fields': count,
        'n_unnamed_fields': 0,
    }
    for index, field in enumerate(fields):
        ns[field] = property(lambda self, i=index: self[i])
    return type(name, (tuple,), ns)


terminal_size = _structseq('os.terminal_size', ('columns', 'lines'))
uname_result = _structseq('posix.uname_result',
                          ('sysname', 'nodename', 'release', 'version', 'machine'))
times_result = _structseq('posix.times_result',
                          ('user', 'system', 'children_user', 'children_system', 'elapsed'))


def get_terminal_size(fd=1):
    import posix
    return terminal_size(posix._terminal_size(fd))


def uname():
    import posix
    return uname_result(posix._uname())


def times():
    import posix
    return times_result(posix._times())


_before_fork = []
_after_in_parent = []
_after_in_child = []


def register_at_fork(*, before=None, after_in_parent=None, after_in_child=None):
    if before is None and after_in_parent is None and after_in_child is None:
        raise TypeError("At least one argument is required.")
    for name, hook in (('before', before), ('after_in_parent', after_in_parent),
                       ('after_in_child', after_in_child)):
        if hook is not None and not callable(hook):
            raise TypeError("'%s' must be callable, not %s" % (name, type(hook).__name__))
    if before is not None:
        _before_fork.append(before)
    if after_in_parent is not None:
        _after_in_parent.append(after_in_parent)
    if after_in_child is not None:
        _after_in_child.append(after_in_child)


def fork():
    import posix
    for hook in reversed(_before_fork):
        hook()
    pid = posix._fork()
    hooks = _after_in_child if pid == 0 else _after_in_parent
    for hook in hooks:
        hook()
    return pid


class DirEntry:
    """One entry of a directory scan. Types and stat results are read lazily and
    cached, as the C `DirEntry` caches them."""

    __module__ = 'posix'
    __slots__ = ('name', 'path', '_lstat', '_stat')

    def __init__(self, name, path):
        self.name = name
        self.path = path
        self._lstat = None
        self._stat = None

    def __fspath__(self):
        return self.path

    def __repr__(self):
        return '<DirEntry %r>' % (self.name,)

    def stat(self, *, follow_symlinks=True):
        import posix
        if follow_symlinks:
            if self._stat is None:
                if self.is_symlink():
                    self._stat = posix.stat(self.path)
                else:
                    self._stat = self._read_lstat()
            return self._stat
        return self._read_lstat()

    def _read_lstat(self):
        import posix
        if self._lstat is None:
            self._lstat = posix.lstat(self.path)
        return self._lstat

    def inode(self):
        return self._read_lstat().st_ino

    def is_symlink(self):
        import stat
        return stat.S_ISLNK(self._read_lstat().st_mode)

    def _is_type(self, test, follow_symlinks):
        import stat
        mode = self._read_lstat().st_mode
        if follow_symlinks and stat.S_ISLNK(mode):
            try:
                mode = self.stat().st_mode
            except FileNotFoundError:
                return False
        return test(mode)

    def is_dir(self, *, follow_symlinks=True):
        import stat
        return self._is_type(stat.S_ISDIR, follow_symlinks)

    def is_file(self, *, follow_symlinks=True):
        import stat
        return self._is_type(stat.S_ISREG, follow_symlinks)

    def is_junction(self):
        return False


class ScandirIterator:
    __module__ = 'posix'

    def __init__(self, names, prefix):
        self._names = iter(names)
        self._prefix = prefix

    def __iter__(self):
        return self

    def __next__(self):
        name = next(self._names)
        return DirEntry(name, self._prefix + name)

    def close(self):
        self._names = iter(())

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def scandir(path=None):
    import posix
    if path is None:
        return ScandirIterator(posix.listdir('.'), '')
    if isinstance(path, int):
        raise NotImplementedError('scandir: path should be string, bytes, os.PathLike or None, not int')
    path = posix.fspath(path)
    names = posix.listdir(path)
    sep = b'/' if isinstance(path, bytes) else '/'
    prefix = path if path.endswith(sep) else path + sep
    return ScandirIterator(names, prefix)
"#
}

//! `_posixsubprocess` and `select` — what `subprocess` and `selectors` stand on.
//!
//! `fork_exec` is a port of `child_exec` / `subprocess_fork_exec` from CPython's
//! `Modules/_posixsubprocess.c`: everything the child needs (argument vectors,
//! the executable search list, the environment) is converted to C strings BEFORE
//! `fork`, so the child only makes libc calls until it execs, plus the
//! `preexec_fn` callback when one was given. A failure is reported to the parent
//! over `errpipe_write` as `<ExcName>:<errno hex>:<message>`, which
//! `subprocess.py` decodes.
//!
//! `select.select` is `poll(2)` underneath, so it has no `FD_SETSIZE` limit.

use crate::host::{self, with_host, PyHost, PyObj};
use crate::stdlib::pyposix::{cint_of, errno, int_of, os_error, path_of};
use fusevm::Value;
use std::ffi::CString;

type R = Result<Value, String>;

/// The `_posixsubprocess` namespace.
pub fn subprocess_entries(h: &mut PyHost) -> Vec<(String, Value)> {
    vec![(
        "fork_exec".to_string(),
        h.alloc(PyObj::Builtin("_posixsubprocess.fork_exec".into())),
    )]
}

/// The `select` namespace.
pub fn select_entries(h: &mut PyHost) -> Vec<(String, Value)> {
    vec![
        (
            "select".to_string(),
            h.alloc(PyObj::Builtin("select.select".into())),
        ),
        (
            "error".to_string(),
            h.alloc(PyObj::Builtin("OSError".into())),
        ),
        ("PIPE_BUF".to_string(), Value::Int(libc::PIPE_BUF as i64)),
    ]
}

pub fn call_subprocess(name: &str, args: &[Value]) -> Option<R> {
    (name == "fork_exec").then(|| fork_exec(args))
}

pub fn call_select(name: &str, args: &[Value]) -> Option<R> {
    (name == "select").then(|| select(args))
}

// ── select ───────────────────────────────────────────────────────────────────

/// The descriptor behind a `select` argument: an `int`, or an object with a
/// `fileno()` method (`PyObject_AsFileDescriptor`).
fn fd_of(v: &Value) -> Result<i32, String> {
    let direct = with_host(|h| match h.index_fit(v) {
        host::IndexFit::Fits(n) if !matches!(v, Value::Bool(_)) => Some(n),
        _ => None,
    });
    let n = match direct {
        Some(n) => n,
        None => {
            let not_fd = || host::type_error("argument must be an int, or have a fileno() method.");
            let r = host::call_method(v, "fileno", vec![], vec![]).map_err(|_| not_fd())?;
            match with_host(|h| h.index_fit(&r)) {
                host::IndexFit::Fits(n) => n,
                _ => return Err(host::type_error("fileno() returned a non-integer")),
            }
        }
    };
    if n < 0 {
        return Err(format!(
            "ValueError: file descriptor cannot be a negative integer ({n})"
        ));
    }
    i32::try_from(n).map_err(|_| "OverflowError: file descriptor out of range".to_string())
}

fn select(args: &[Value]) -> R {
    if !(3..=4).contains(&args.len()) {
        return Err(host::type_error(&format!(
            "select expected at least 3 arguments, got {}",
            args.len()
        )));
    }
    let lists = [&args[0], &args[1], &args[2]]
        .iter()
        .map(|l| host::iter_vec(l))
        .collect::<Result<Vec<_>, _>>()?;
    let deadline = match args.get(3) {
        None | Some(Value::Undef) => None,
        Some(t) => {
            let secs = match t {
                Value::Float(f) => *f,
                other => int_of("select", "timeout", other)? as f64,
            };
            if secs.is_nan() || secs < 0.0 {
                return Err("ValueError: timeout must be non-negative".to_string());
            }
            Some(std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.min(1e9)))
        }
    };
    const EVENTS: [libc::c_short; 3] = [libc::POLLIN, libc::POLLOUT, libc::POLLPRI];
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut index: Vec<Vec<(usize, Value)>> = vec![Vec::new(); 3];
    for (kind, items) in lists.iter().enumerate() {
        for obj in items {
            let fd = fd_of(obj)?;
            let slot = match fds.iter().position(|p| p.fd == fd) {
                Some(i) => i,
                None => {
                    fds.push(libc::pollfd {
                        fd,
                        events: 0,
                        revents: 0,
                    });
                    fds.len() - 1
                }
            };
            fds[slot].events |= EVENTS[kind];
            index[kind].push((slot, obj.clone()));
        }
    }
    loop {
        let ms = match deadline {
            None => -1,
            Some(d) => {
                let left = d.saturating_duration_since(std::time::Instant::now());
                left.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int
            }
        };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n >= 0 {
            break;
        }
        if errno() != libc::EINTR {
            return Err(os_error(errno(), None, None));
        }
    }
    if let Some(bad) = fds.iter().find(|p| p.revents & libc::POLLNVAL != 0) {
        let _ = bad;
        return Err(os_error(libc::EBADF, None, None));
    }
    let ready = |slot: usize, mask: libc::c_short| fds[slot].revents & mask != 0;
    let masks = [
        libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        libc::POLLOUT | libc::POLLERR,
        libc::POLLPRI,
    ];
    Ok(with_host(|h| {
        let out: Vec<Value> = index
            .iter()
            .zip(masks)
            .map(|(entries, mask)| {
                let hits = entries
                    .iter()
                    .filter(|(slot, _)| ready(*slot, mask))
                    .map(|(_, v)| v.clone())
                    .collect();
                h.new_list(hits)
            })
            .collect();
        h.new_tuple(out)
    }))
}

// ── fork_exec ────────────────────────────────────────────────────────────────

/// Write `<class>:<errno hex>:<message>` to the parent. Only `write` is used;
/// the total is far below `PIPE_BUF`, so it lands whole.
fn report(fd: i32, class: &str, eno: i32, msg: &str) {
    let line = format!("{class}:{eno:x}:{msg}");
    unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
}

/// Report a failure to the parent and leave without running any destructors.
fn exit_reporting(fd: i32, class: &str, eno: i32, msg: &str) -> ! {
    report(fd, class, eno, msg);
    unsafe { libc::_exit(255) }
}

fn set_inheritable(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    }
}

/// `dup` that leaves the copy non-inheritable, as `_Py_dup` does.
fn dup_cloexec(fd: i32) -> i32 {
    unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) }
}

/// Close every descriptor from 3 up except those in `keep` (sorted).
fn close_open_fds(keep: &[i32]) {
    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(256, 1 << 20) as i32;
    for fd in 3..max {
        if keep.binary_search(&fd).is_err() {
            unsafe { libc::close(fd) };
        }
    }
}

/// Everything `child_exec` needs, converted before `fork`.
struct ChildPlan {
    argv: Vec<CString>,
    exec_list: Vec<CString>,
    envp: Option<Vec<CString>>,
    cwd: Option<CString>,
    keep: Vec<i32>,
    close_fds: bool,
    fds: [i32; 6],
    errpipe: [i32; 2],
    restore_signals: bool,
    call_setsid: bool,
    pgid_to_set: i32,
    gid: Option<libc::gid_t>,
    groups: Option<Vec<libc::gid_t>>,
    uid: Option<libc::uid_t>,
    umask: i32,
    preexec: Option<Value>,
}

fn ptrs(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn child_exec(plan: &ChildPlan) -> ! {
    let [p2cread, p2cwrite, c2pread, c2pwrite, errread, errwrite] = plan.fds;
    let [errpipe_read, errpipe_write] = plan.errpipe;
    // `noexec` marks a failure before the exec loop: the parent then raises
    // without a filename. The exec loop clears it, so its failure names the program.
    let mut msg = "noexec";
    let fail =
        |class: &str, eno: i32, msg: &str| -> ! { exit_reporting(errpipe_write, class, eno, msg) };
    for fd in [p2cwrite, c2pread, errread, errpipe_read] {
        if fd != -1 {
            unsafe { libc::close(fd) };
        }
    }
    // A descriptor that landed on 0, 1 or 2 would be overwritten by the dup2s
    // below (bpo-12607), so move it first.
    let (mut c2pwrite, mut errwrite) = (c2pwrite, errwrite);
    if c2pwrite == 0 {
        c2pwrite = dup_cloexec(c2pwrite);
        if c2pwrite < 0 {
            fail("OSError", errno(), msg);
        }
    }
    while errwrite == 0 || errwrite == 1 {
        errwrite = dup_cloexec(errwrite);
        if errwrite < 0 {
            fail("OSError", errno(), msg);
        }
    }
    // `dup2` clears CLOEXEC; a no-op `dup2` (fd already the target) must clear
    // it by hand (bpo-10806).
    for (src, dst) in [(p2cread, 0), (c2pwrite, 1), (errwrite, 2)] {
        if src == dst {
            set_inheritable(dst);
        } else if src != -1 && unsafe { libc::dup2(src, dst) } < 0 {
            fail("OSError", errno(), msg);
        }
    }
    if let Some(cwd) = &plan.cwd {
        if unsafe { libc::chdir(cwd.as_ptr()) } < 0 {
            fail("OSError", errno(), "noexec:chdir");
        }
    }
    if plan.umask >= 0 {
        unsafe { libc::umask(plan.umask as libc::mode_t) };
    }
    if plan.restore_signals {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
        }
    }
    if plan.call_setsid && unsafe { libc::setsid() } < 0 {
        fail("OSError", errno(), msg);
    }
    if plan.pgid_to_set >= 0 && unsafe { libc::setpgid(0, plan.pgid_to_set) } < 0 {
        fail("OSError", errno(), msg);
    }
    if let Some(groups) = &plan.groups {
        if unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) } < 0 {
            fail("OSError", errno(), msg);
        }
    }
    if let Some(gid) = plan.gid {
        if unsafe { libc::setregid(gid, gid) } < 0 {
            fail("OSError", errno(), msg);
        }
    }
    if let Some(uid) = plan.uid {
        if unsafe { libc::setreuid(uid, uid) } < 0 {
            fail("OSError", errno(), msg);
        }
    }
    if let Some(f) = &plan.preexec {
        if host::invoke(f, vec![], vec![]).is_err() {
            fail("SubprocessError", 0, "Exception occurred in preexec_fn.");
        }
    }
    // Close after `preexec_fn`, which may have opened descriptors.
    if plan.close_fds {
        close_open_fds(&plan.keep);
    }
    // The `Lib/os.py` `_execvpe` PATH search, over the list `subprocess` built.
    msg = "";
    let argv = ptrs(&plan.argv);
    let envp = plan.envp.as_deref().map(ptrs);
    let mut saved = 0;
    for exe in &plan.exec_list {
        unsafe {
            match &envp {
                Some(e) => libc::execve(exe.as_ptr(), argv.as_ptr(), e.as_ptr()),
                None => libc::execv(exe.as_ptr(), argv.as_ptr()),
            };
        }
        let e = errno();
        if e != libc::ENOENT && e != libc::ENOTDIR && saved == 0 {
            saved = e;
        }
    }
    fail("OSError", if saved != 0 { saved } else { errno() }, msg)
}

fn seq(v: &Value) -> Option<Vec<Value>> {
    with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => Some(l.to_vec()),
        Some(PyObj::Tuple(t)) => Some(t.to_vec()),
        _ => None,
    })
}

fn is_none(v: &Value) -> bool {
    matches!(v, Value::Undef)
}

fn plan_from(args: &[Value]) -> Result<ChildPlan, String> {
    let c_list = |v: &Value, what: &str| -> Result<Vec<CString>, String> {
        let items = seq(v).ok_or_else(|| {
            host::type_error(&format!("fork_exec: {what} must be a list or tuple"))
        })?;
        items
            .iter()
            .map(|i| path_of("fork_exec", what, i).map(|p| p.c))
            .collect()
    };
    let argv = if is_none(&args[0]) {
        Vec::new()
    } else {
        c_list(&args[0], "args")?
    };
    let exec_list = c_list(&args[1], "executable_list")?;
    let keep = seq(&args[3])
        .ok_or_else(|| host::type_error("fork_exec: fds_to_keep must be a tuple"))?
        .iter()
        .map(|v| cint_of("fork_exec", "fds_to_keep", v))
        .collect::<Result<Vec<_>, _>>()?;
    if keep.windows(2).any(|w| w[0] >= w[1]) {
        return Err("ValueError: bad value(s) in fds_to_keep".to_string());
    }
    let cwd = if is_none(&args[4]) {
        None
    } else {
        Some(path_of("fork_exec", "cwd", &args[4])?.c)
    };
    let envp = if is_none(&args[5]) {
        None
    } else {
        Some(c_list(&args[5], "env_list")?)
    };
    let mut fds = [0i32; 6];
    for (i, slot) in fds.iter_mut().enumerate() {
        *slot = cint_of("fork_exec", "fd", &args[6 + i])?;
    }
    let errpipe = [
        cint_of("fork_exec", "errpipe_read", &args[12])?,
        cint_of("fork_exec", "errpipe_write", &args[13])?,
    ];
    let flag = |v: &Value| with_host(|h| h.truthy(v));
    let opt_id = |v: &Value, what: &str| -> Result<Option<u32>, String> {
        if is_none(v) {
            return Ok(None);
        }
        let n = int_of("fork_exec", what, v)?;
        u32::try_from(n)
            .map(Some)
            .map_err(|_| format!("OverflowError: {what} is out of range"))
    };
    let groups = if is_none(&args[18]) {
        None
    } else {
        Some(
            seq(&args[18])
                .ok_or_else(|| host::type_error("fork_exec: extra_groups must be a list"))?
                .iter()
                .map(|g| opt_id(g, "group").map(|g| g.unwrap_or(0)))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    Ok(ChildPlan {
        argv,
        exec_list,
        envp,
        cwd,
        keep,
        close_fds: flag(&args[2]),
        fds,
        errpipe,
        restore_signals: flag(&args[14]),
        call_setsid: flag(&args[15]),
        pgid_to_set: cint_of("fork_exec", "process_group", &args[16])?,
        gid: opt_id(&args[17], "gid")?,
        groups,
        uid: opt_id(&args[19], "uid")?,
        umask: cint_of("fork_exec", "umask", &args[20])?,
        preexec: (!is_none(&args[21])).then(|| args[21].clone()),
    })
}

fn fork_exec(args: &[Value]) -> R {
    if args.len() != 22 {
        return Err(host::type_error(&format!(
            "fork_exec expected 22 arguments, got {}",
            args.len()
        )));
    }
    let plan = plan_from(args)?;
    // SAFETY: the child only returns through `child_exec`, which never returns.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => child_exec(&plan),
        p if p < 0 => Err(os_error(errno(), None, None)),
        p => Ok(Value::Int(p as i64)),
    }
}

//! The native `posix` layer (`src/stdlib/pyposix.rs`) driven through `os`.
//!
//! Only the self-contained build runs these: with `stdlib-ffi` on, `os` is
//! CPython's own. Every case works in a fresh `tempfile.mkdtemp()` directory.
#![cfg(not(feature = "stdlib-ffi"))]

use pythonrs::{eval_str, host};

/// Run `src`, then return the `repr` of global `name`.
fn g(src: &str, name: &str) -> String {
    eval_str(src).expect("program should run without error");
    host::with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

#[test]
fn fd_round_trip_and_bad_descriptor() {
    let src = "\
import os, tempfile
d = tempfile.mkdtemp()
p = os.path.join(d, 'f')
fd = os.open(p, os.O_CREAT | os.O_RDWR | os.O_TRUNC, 0o600)
n = os.write(fd, b'hello world')
os.lseek(fd, 6, 0)
tail = os.read(fd, 100)
size = os.fstat(fd).st_size
dup = os.dup(fd)
os.ftruncate(dup, 5)
size2 = os.fstat(fd).st_size
os.close(dup)
os.close(fd)
try:
    os.read(fd, 1)
except OSError as e:
    bad = (type(e).__name__, e.errno, e.filename)
x = (n, tail, size, size2, bad, os.get_inheritable(os.open(p, os.O_RDONLY)))
";
    assert_eq!(
        g(src, "x"),
        "(11, b'world', 11, 5, ('OSError', 9, None), False)"
    );
}

#[test]
fn errors_carry_subclass_errno_and_both_filenames() {
    let src = "\
import os, tempfile
d = tempfile.mkdtemp()
a = os.path.join(d, 'a')
b = os.path.join(d, 'b')
try:
    os.rename(a, b)
except FileNotFoundError as e:
    r = (e.errno, e.filename == a, e.filename2 == b, str(e) == \"[Errno 2] No such file or directory: %r -> %r\" % (a, b))
os.mkdir(a)
try:
    os.mkdir(a)
except FileExistsError as e:
    m = (e.errno, e.filename == a)
try:
    os.rmdir(os.path.join(d, 'missing'))
except OSError as e:
    q = type(e).__name__
x = (r, m, q)
";
    assert_eq!(
        g(src, "x"),
        "((2, True, True, True), (17, True), 'FileNotFoundError')"
    );
}

#[test]
fn stat_result_has_float_and_nanosecond_times() {
    let src = "\
import os, tempfile
p = os.path.join(tempfile.mkdtemp(), 'f')
open(p, 'w').close()
os.utime(p, ns=(1_500_000_000_123_456_789, 1_600_000_000_987_654_321))
st = os.stat(p)
x = (
    st.st_mtime_ns, st.st_atime_ns,
    isinstance(st.st_mtime, float), type(st[8]).__name__,
    st.st_mtime == st.st_mtime_ns / 1e9 or abs(st.st_mtime - 1600000000.987654) < 1e-3,
    len(st), st.st_size, st.st_nlink,
)
";
    assert_eq!(
        g(src, "x"),
        "(1600000000987654321, 1500000000123456789, True, 'int', True, 10, 0, 1)"
    );
}

#[test]
fn utime_times_floor_and_chmod_bits() {
    let src = "\
import os, stat, tempfile
p = os.path.join(tempfile.mkdtemp(), 'f')
open(p, 'w').close()
os.utime(p, (10.75, -0.5))
st = os.lstat(p)
os.chmod(p, 0o640)
fd = os.open(p, os.O_RDONLY)
os.fchmod(fd, 0o604)
mode = stat.S_IMODE(os.stat(fd).st_mode)
try:
    os.utime(p, (1, 2), ns=(1, 2))
except ValueError as e:
    both = str(e)
x = (st.st_atime_ns, st.st_mtime_ns, oct(mode), both)
";
    assert_eq!(
        g(src, "x"),
        "(10750000000, -500000000, '0o604', \"utime: you may specify either 'times' or 'ns' but not both\")"
    );
}

#[test]
fn links_and_readlink_keep_the_path_type() {
    let src = "\
import os, tempfile
d = tempfile.mkdtemp()
t = os.path.join(d, 't')
open(t, 'w').write('x')
s = os.path.join(d, 's')
h = os.path.join(d, 'h')
os.symlink('t', s)
os.link(t, h)
x = (
    os.readlink(s), os.readlink(os.fsencode(s)),
    os.path.islink(s), os.lstat(s).st_size, os.stat(t).st_nlink,
    os.stat(s).st_ino == os.stat(t).st_ino,
    os.lstat(s).st_ino == os.stat(t).st_ino,
)
";
    assert_eq!(g(src, "x"), "('t', b't', True, 1, 2, True, False)");
}

#[test]
fn scandir_entries_and_walk() {
    let src = "\
import os, tempfile
d = tempfile.mkdtemp()
os.mkdir(os.path.join(d, 'sub'))
open(os.path.join(d, 'a.txt'), 'w').write('hi')
open(os.path.join(d, 'sub', 'b.txt'), 'w').write('hi')
os.symlink('sub', os.path.join(d, 'ln'))
with os.scandir(d) as it:
    ents = sorted((e.name, e.is_dir(), e.is_file(), e.is_symlink(), e.is_dir(follow_symlinks=False)) for e in it)
e0 = next(os.scandir(d))
walked = sorted((os.path.relpath(r, d), sorted(ds), sorted(fs)) for r, ds, fs in os.walk(d))
x = (ents, e0.path == os.path.join(d, e0.name), os.fspath(e0) == e0.path, repr(e0) == '<DirEntry %r>' % e0.name, walked)
";
    assert_eq!(
        g(src, "x"),
        "([('a.txt', False, True, False, False), ('ln', True, False, True, False), ('sub', True, False, False, True)], True, True, True, [('.', ['ln', 'sub'], ['a.txt']), ('sub', [], ['b.txt'])])"
    );
}

#[test]
fn struct_sequences_construct_and_repr() {
    let src = "\
import os
ts = os.terminal_size((80, 24))
u = os.uname()
x = (
    repr(ts), ts.columns, ts.lines, tuple(ts), isinstance(ts, tuple),
    type(u).__name__, len(u), u.sysname == os.uname()[0],
    len(os.times()), os.sysconf('SC_PAGE_SIZE') > 0,
)
try:
    os.terminal_size((1, 2, 3))
except TypeError as e:
    err = str(e)
";
    eval_str(src).unwrap();
    assert_eq!(
        g(src, "x"),
        "('os.terminal_size(columns=80, lines=24)', 80, 24, (80, 24), True, 'uname_result', 5, True, 5, True)"
    );
    assert_eq!(
        g(src, "err"),
        "'os.terminal_size() takes a 2-sequence (3-sequence given)'"
    );
}

#[test]
fn pipe_dup2_and_wait_status_decoding() {
    let src = "\
import os
r, w = os.pipe()
os.write(w, b'ab')
os.close(w)
got = os.read(r, 10)
eof = os.read(r, 10)
os.close(r)
try:
    os.waitpid(-1, os.WNOHANG)
except ChildProcessError as e:
    ech = e.errno
x = (
    got, eof, ech,
    os.waitstatus_to_exitcode(3 << 8), os.waitstatus_to_exitcode(9),
    os.WIFEXITED(3 << 8), os.WEXITSTATUS(3 << 8), os.WIFSIGNALED(9), os.WTERMSIG(9),
)
";
    assert_eq!(g(src, "x"), "(b'ab', b'', 10, 3, -9, True, 3, True, 9)");
}

#[test]
fn script_directory_modules_import() {
    let src = "\
import os, sys, tempfile
d = tempfile.mkdtemp()
os.mkdir(os.path.join(d, 'pkg_local'))
open(os.path.join(d, 'pkg_local', '__init__.py'), 'w').write('from .leaf import VALUE\\n')
open(os.path.join(d, 'pkg_local', 'leaf.py'), 'w').write('VALUE = 41 + 1\\n')
open(os.path.join(d, 'solo_local.py'), 'w').write('import sys\\nNAME = __name__\\nFRAME = sys._getframemodulename(0)\\n')
sys.path.insert(0, d)
import pkg_local, solo_local
x = (pkg_local.VALUE, solo_local.NAME, solo_local.FRAME, sys._getframemodulename(0))
";
    assert_eq!(g(src, "x"), "(42, 'solo_local', 'solo_local', '__main__')");
}

//! The `os` standard-library module plus its `os.path` submodule.
//!
//! `os.path` is exposed as a genuine nested `PyObj::Module` value stored under the
//! `path` attribute, so `os.path.join(...)` resolves attribute-then-call exactly
//! like CPython. The path functions are ported from CPython's `posixpath`
//! (`join`, `split`, `basename`, `dirname`, `splitext`, `normpath`, `abspath`,
//! `isabs`, `exists`, `isdir`, `isfile`, `expanduser`) — this build targets POSIX
//! hosts (macOS / Linux), so `sep = "/"` and `name = "posix"`.
//!
//! Wiring (done by the parent): an `import_module` arm for `"os"` that calls
//! [`entries`], and a `call_builtin_function` arm routing both `os.*` and
//! `os.path.*` names to [`call`].

use crate::host::{type_error, PKey, PyHost, PyObj};
use fusevm::Value;
use indexmap::IndexMap;

/// The `os` module namespace, including constants and the nested `os.path`
/// module.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    // Build os.path as a real nested module whose callables are Builtin handles
    // named "os.path.<fn>" — the VM invokes those back through `call`.
    let mut path_ns: IndexMap<String, Value> = IndexMap::new();
    for name in [
        "join",
        "split",
        "basename",
        "dirname",
        "splitext",
        "normpath",
        "abspath",
        "isabs",
        "exists",
        "isdir",
        "isfile",
        "expanduser",
    ] {
        let b = h.alloc(PyObj::Builtin(format!("os.path.{name}")));
        path_ns.insert(name.to_string(), b);
    }
    path_ns.insert("sep".into(), h.new_str("/"));
    path_ns.insert("extsep".into(), h.new_str("."));
    path_ns.insert("pathsep".into(), h.new_str(":"));
    path_ns.insert("curdir".into(), h.new_str("."));
    path_ns.insert("pardir".into(), h.new_str(".."));
    let path_mod = h.alloc(PyObj::Module {
        name: "posixpath".into(),
        ns: path_ns,
    });

    let environ = build_environ(h);

    let mut out: Vec<(String, Value)> = Vec::new();
    for name in ["getcwd", "getenv", "listdir", "getpid"] {
        out.push((name.into(), h.alloc(PyObj::Builtin(format!("os.{name}")))));
    }
    out.push(("path".into(), path_mod));
    out.push(("environ".into(), environ));
    out.push(("sep".into(), h.new_str("/")));
    out.push(("linesep".into(), h.new_str("\n")));
    out.push(("pathsep".into(), h.new_str(":")));
    out.push(("extsep".into(), h.new_str(".")));
    out.push(("curdir".into(), h.new_str(".")));
    out.push(("pardir".into(), h.new_str("..")));
    out.push(("name".into(), h.new_str("posix")));
    out
}

/// A snapshot `dict` of the current process environment.
fn build_environ(h: &mut PyHost) -> Value {
    let mut map: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    for (k, v) in std::env::vars() {
        let kv = h.new_str(k.clone());
        let vv = h.new_str(v);
        map.insert(PKey::Str(k), (kv, vv));
    }
    h.new_dict(map)
}

/// Dispatch an `os.*` / `os.path.*` builtin. `None` if not ours.
/// The `os`/`os.path` function names this module owns (already `os.`-stripped).
const OS_FUNCS: &[&str] = &[
    "getcwd",
    "getenv",
    "listdir",
    "getpid",
    "path.join",
    "path.split",
    "path.basename",
    "path.dirname",
    "path.splitext",
    "path.normpath",
    "path.abspath",
    "path.isabs",
    "path.exists",
    "path.isdir",
    "path.isfile",
    "path.expanduser",
];

pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    // `fname` arrives already stripped of the `os.` prefix by the caller; tolerate
    // a full `os.path.join` too.
    let rest = fname.strip_prefix("os.").unwrap_or(fname);
    if !OS_FUNCS.contains(&rest) {
        return None;
    }
    Some(os_dispatch(h, rest, args))
}

/// Dispatch an owned `os`/`os.path` function — returns `Result` so `?` is usable.
fn os_dispatch(h: &mut PyHost, rest: &str, args: &[Value]) -> Result<Value, String> {
    match rest {
        "getcwd" => os_getcwd(h),
        "getenv" => os_getenv(h, args),
        "listdir" => os_listdir(h, args),
        "getpid" => Ok(Value::Int(std::process::id() as i64)),

        "path.join" => path_join(h, args),
        "path.split" => path_split(h, args),
        "path.basename" => Ok(h.new_str(basename(&arg_str(h, args, 0)?))),
        "path.dirname" => Ok(h.new_str(dirname(&arg_str(h, args, 0)?))),
        "path.splitext" => path_splitext(h, args),
        "path.normpath" => Ok(h.new_str(normpath(&arg_str(h, args, 0)?))),
        "path.abspath" => path_abspath(h, args),
        "path.isabs" => Ok(Value::Bool(arg_str(h, args, 0)?.starts_with('/'))),
        "path.exists" => Ok(Value::Bool(
            std::path::Path::new(&arg_str(h, args, 0)?).exists(),
        )),
        "path.isdir" => Ok(Value::Bool(
            std::path::Path::new(&arg_str(h, args, 0)?).is_dir(),
        )),
        "path.isfile" => Ok(Value::Bool(
            std::path::Path::new(&arg_str(h, args, 0)?).is_file(),
        )),
        "path.expanduser" => Ok(h.new_str(expanduser(&arg_str(h, args, 0)?))),
        _ => unreachable!("os_dispatch called with unowned name"),
    }
}

// ── os functions ─────────────────────────────────────────────────────────────

fn os_getcwd(h: &mut PyHost) -> Result<Value, String> {
    match std::env::current_dir() {
        Ok(p) => Ok(h.new_str(p.to_string_lossy().into_owned())),
        Err(e) => Err(format!("OSError: {e}")),
    }
}

fn os_getenv(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let key = arg_str(h, args, 0)?;
    match std::env::var(&key) {
        Ok(v) => Ok(h.new_str(v)),
        Err(_) => Ok(args.get(1).cloned().unwrap_or(Value::Undef)),
    }
}

fn os_listdir(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let path = match args.first() {
        None | Some(Value::Undef) => ".".to_string(),
        Some(_) => arg_str(h, args, 0)?,
    };
    let rd = std::fs::read_dir(&path).map_err(|e| format!("FileNotFoundError: {path}: {e}"))?;
    let mut names: Vec<Value> = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| format!("OSError: {e}"))?;
        names.push(h.new_str(ent.file_name().to_string_lossy().into_owned()));
    }
    Ok(h.new_list(names))
}

// ── posixpath ports ──────────────────────────────────────────────────────────

/// posixpath.join: later absolute components reset the accumulated path.
fn path_join(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    if args.is_empty() {
        return Err(type_error("join() missing required argument: 'a'"));
    }
    let mut path = arg_str(h, args, 0)?;
    for i in 1..args.len() {
        let b = arg_str(h, args, i)?;
        if b.starts_with('/') {
            path = b;
        } else if path.is_empty() || path.ends_with('/') {
            path.push_str(&b);
        } else {
            path.push('/');
            path.push_str(&b);
        }
    }
    Ok(h.new_str(path))
}

/// posixpath.split -> (head, tail).
fn path_split(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let p = arg_str(h, args, 0)?;
    let i = p.rfind('/').map(|x| x + 1).unwrap_or(0);
    let (mut head, tail) = (p[..i].to_string(), p[i..].to_string());
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        while head.ends_with('/') {
            head.pop();
        }
    }
    let hv = h.new_str(head);
    let tv = h.new_str(tail);
    Ok(h.new_tuple(vec![hv, tv]))
}

fn basename(p: &str) -> String {
    let i = p.rfind('/').map(|x| x + 1).unwrap_or(0);
    p[i..].to_string()
}

fn dirname(p: &str) -> String {
    let i = p.rfind('/').map(|x| x + 1).unwrap_or(0);
    let mut head = p[..i].to_string();
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        while head.ends_with('/') {
            head.pop();
        }
    }
    head
}

/// posixpath.splitext -> (root, ext); leading dots of the final component are
/// not treated as an extension separator.
fn path_splitext(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let p = arg_str(h, args, 0)?;
    // `/` and `.` are ASCII, so byte offsets from `rfind` are valid char
    // boundaries and byte-wise scanning of the filename is correct.
    let bytes = p.as_bytes();
    let sep_index = p.rfind('/').map(|x| x as isize).unwrap_or(-1);
    let dot_index = p.rfind('.').map(|x| x as isize).unwrap_or(-1);
    if dot_index > sep_index {
        let mut filename_index = sep_index + 1;
        // Skip all leading dots of the filename.
        while filename_index < dot_index {
            if bytes[filename_index as usize] != b'.' {
                let (root, ext) = p.split_at(dot_index as usize);
                let rv = h.new_str(root.to_string());
                let ev = h.new_str(ext.to_string());
                return Ok(h.new_tuple(vec![rv, ev]));
            }
            filename_index += 1;
        }
    }
    let rv = h.new_str(p);
    let ev = h.new_str("");
    Ok(h.new_tuple(vec![rv, ev]))
}

/// posixpath.normpath: collapse redundant separators, `.` and `..`.
fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let mut initial_slashes = if path.starts_with('/') { 1 } else { 0 };
    // POSIX: exactly two leading slashes are preserved; three or more collapse.
    if initial_slashes == 1 && path.starts_with("//") && !path.starts_with("///") {
        initial_slashes = 2;
    }
    let mut new_comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".."
            || (initial_slashes == 0 && new_comps.is_empty())
            || (new_comps.last() == Some(&".."))
        {
            new_comps.push(comp);
        } else if !new_comps.is_empty() {
            new_comps.pop();
        }
    }
    let mut result = new_comps.join("/");
    if initial_slashes > 0 {
        let prefix = "/".repeat(initial_slashes);
        result = format!("{prefix}{result}");
    }
    if result.is_empty() {
        ".".into()
    } else {
        result
    }
}

/// posixpath.abspath: normalize, joining with the cwd if not already absolute.
fn path_abspath(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let p = arg_str(h, args, 0)?;
    let full = if p.starts_with('/') {
        p
    } else {
        let cwd = std::env::current_dir()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());
        if cwd.ends_with('/') {
            format!("{cwd}{p}")
        } else {
            format!("{cwd}/{p}")
        }
    };
    Ok(h.new_str(normpath(&full)))
}

/// posixpath.expanduser: a leading `~` -> `$HOME`, `~/x` -> `$HOME/x`.
fn expanduser(p: &str) -> String {
    if !p.starts_with('~') {
        return p.to_string();
    }
    // Only bare `~` or `~/...` are expanded (no `~user` lookup here).
    let rest = &p[1..];
    if !rest.is_empty() && !rest.starts_with('/') {
        return p.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return p.to_string();
    }
    let mut home = home;
    if home != "/" {
        while home.ends_with('/') {
            home.pop();
        }
    }
    format!("{home}{rest}")
}

// ── arg helpers ──────────────────────────────────────────────────────────────

/// A positional string argument, accepting native and heap strings.
fn arg_str(h: &PyHost, args: &[Value], i: usize) -> Result<String, String> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok((**s).clone()),
        Some(v) => h
            .as_str(v)
            .ok_or_else(|| type_error(&format!("expected str, not {}", h.type_name(v)))),
        None => Err(type_error("missing required string argument")),
    }
}

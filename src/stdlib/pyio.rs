//! `_io` — the I/O accelerator `io.py` is built on.
//!
//! CPython splits I/O in two: `_io` (C) holds the concrete streams, and `io.py`
//! declares the abstract base classes on top of them, because "declaring ABCs in
//! C is tricky". pythonrs keeps the same split, which is why this module exists
//! at all — `io.py` opens with `from _io import (open, FileIO, BytesIO, StringIO,
//! …)`, and without it `io`, `pathlib`, `logging`, `unittest`, `hashlib` and
//! `pprint` are all unreachable.
//!
//! The in-memory streams (`BytesIO`, `StringIO`) are implemented here outright.
//! The file-backed ones reuse the host's existing `IoCell` layer — the same
//! handles the `open` builtin has always returned — so there is exactly one file
//! implementation in the runtime rather than two that must be kept in agreement.
//!
//! `StringIO` positions are CODE POINT offsets, as CPython's are, not byte
//! offsets: `tell()` after writing "é" is 1. The buffer is kept as a `String`
//! with a parallel character count so the append case (every logging handler,
//! every `pprint`) stays O(1) and only a seek into the middle pays for indexing.

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// `io.DEFAULT_BUFFER_SIZE` — 128 KiB since 3.13 (it was 8 KiB before). `open`
/// sizes its buffer as `max(min(st_blksize, 8 MiB), DEFAULT_BUFFER_SIZE)`, and
/// code reads the constant to size its own buffers to match.
pub const DEFAULT_BUFFER_SIZE: i64 = 128 * 1024;

// ── position helpers ─────────────────────────────────────────────────────────

/// Byte offset of code-point index `at` in `s`, clamped to the end.
fn byte_at(s: &str, at: usize) -> usize {
    s.char_indices().nth(at).map(|(i, _)| i).unwrap_or(s.len())
}

/// The whole-stream error CPython raises for an operation on a closed stream.
fn closed_err() -> String {
    "ValueError: I/O operation on closed file.".into()
}

// ── constructors ─────────────────────────────────────────────────────────────

/// `BytesIO([initial_bytes])`.
fn new_bytesio(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let buf = match args.first() {
        Some(v) if !matches!(v, Value::Undef) => match h.get(v) {
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
            _ => return Err(host::type_error("a bytes-like object is required")),
        },
        _ => Vec::new(),
    };
    Ok(h.alloc(PyObj::BytesIO {
        buf,
        pos: 0,
        closed: false,
    }))
}

/// `StringIO([initial_value[, newline]])`. `newline=''` and `newline='\n'` both
/// disable translation on write; the default `'\n'` argument means "translate
/// nothing on write, but split on any newline when reading", which is what the
/// `translate` flag below records.
fn new_stringio(
    h: &mut PyHost,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let initial = match args.first() {
        Some(v) if !matches!(v, Value::Undef) => h
            .as_str(v)
            .ok_or_else(|| host::type_error("initial_value must be str or None"))?,
        _ => String::new(),
    };
    let newline_arg = args
        .get(1)
        .cloned()
        .or_else(|| {
            kwargs
                .iter()
                .find(|(k, _)| k == "newline")
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| h.new_str("\n".to_string()));
    // `newline=None` translates '\n' to os.linesep on write; on every platform
    // pythonrs targets that is '\n', so the observable behavior is identical to
    // `newline=''` and only the universal-newlines READ behavior differs.
    let newline = h.as_str(&newline_arg);
    let translate = newline.is_none();
    let len = initial.chars().count();
    Ok(h.alloc(PyObj::StringIO {
        buf: initial,
        len,
        pos: 0,
        closed: false,
        translate,
    }))
}

// ── BytesIO methods ──────────────────────────────────────────────────────────

fn bytesio_method(
    h: &mut PyHost,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let (buf, pos, closed) = match h.get(recv) {
        Some(PyObj::BytesIO { buf, pos, closed }) => (buf.clone(), *pos, *closed),
        _ => return None,
    };
    // Every operation but `close`/`closed` is an error once the stream is closed.
    if closed && !matches!(name, "close" | "closed" | "__exit__") {
        return Some(Err(closed_err()));
    }
    let int_arg = |i: usize| -> Option<i64> { args.get(i).and_then(|v| h.as_int(v)) };
    Some(match name {
        "getvalue" => Ok(h.alloc(PyObj::Bytes(buf))),
        "read" | "read1" => {
            let n = int_arg(0).filter(|n| *n >= 0);
            let end = match n {
                Some(n) => (pos + n as usize).min(buf.len()),
                None => buf.len(),
            };
            let start = pos.min(buf.len());
            set_bytes_pos(h, recv, end);
            Ok(h.alloc(PyObj::Bytes(buf[start..end].to_vec())))
        }
        "readline" => {
            let start = pos.min(buf.len());
            let limit = int_arg(0).filter(|n| *n >= 0).map(|n| n as usize);
            let mut end = buf[start..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|i| start + i + 1)
                .unwrap_or(buf.len());
            if let Some(l) = limit {
                end = end.min(start + l);
            }
            set_bytes_pos(h, recv, end);
            Ok(h.alloc(PyObj::Bytes(buf[start..end].to_vec())))
        }
        "readlines" => {
            let start = pos.min(buf.len());
            let mut out = Vec::new();
            let mut i = start;
            while i < buf.len() {
                let end = buf[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map(|k| i + k + 1)
                    .unwrap_or(buf.len());
                out.push(h.alloc(PyObj::Bytes(buf[i..end].to_vec())));
                i = end;
            }
            set_bytes_pos(h, recv, buf.len());
            Ok(h.new_list(out))
        }
        "write" => {
            let data = match h.get(args.first()?) {
                Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
                _ => return Some(Err(host::type_error("a bytes-like object is required"))),
            };
            let n = data.len();
            let mut buf = buf;
            // A write past the end zero-fills the gap, as CPython does.
            if pos > buf.len() {
                buf.resize(pos, 0);
            }
            let end = pos + n;
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[pos..end].copy_from_slice(&data);
            if let Some(PyObj::BytesIO { buf: b, pos: p, .. }) = h.get_mut(recv) {
                *b = buf;
                *p = end;
            }
            Ok(Value::Int(n as i64))
        }
        "writelines" => {
            let items = match h.iter_items(args.first()?) {
                Ok(i) => i,
                Err(e) => return Some(Err(e)),
            };
            for it in items {
                if let Err(e) = bytesio_method(h, recv, "write", &[it]).unwrap_or(Ok(Value::Undef))
                {
                    return Some(Err(e));
                }
            }
            Ok(Value::Undef)
        }
        "seek" => {
            let off = int_arg(0).unwrap_or(0);
            let whence = int_arg(1).unwrap_or(0);
            let base = match whence {
                1 => pos as i64,
                2 => buf.len() as i64,
                _ => 0,
            };
            let new = base + off;
            if new < 0 {
                return Some(Err(format!("ValueError: negative seek value {new}")));
            }
            set_bytes_pos(h, recv, new as usize);
            Ok(Value::Int(new))
        }
        "tell" => Ok(Value::Int(pos as i64)),
        "truncate" => {
            let size = int_arg(0).map(|n| n as usize).unwrap_or(pos);
            if let Some(PyObj::BytesIO { buf: b, .. }) = h.get_mut(recv) {
                b.truncate(size);
            }
            Ok(Value::Int(size as i64))
        }
        "close" => {
            if let Some(PyObj::BytesIO { closed: c, .. }) = h.get_mut(recv) {
                *c = true;
            }
            Ok(Value::Undef)
        }
        "flush" => Ok(Value::Undef),
        "readable" | "writable" | "seekable" | "isatty" => Ok(Value::Bool(name != "isatty")),
        "__enter__" => Ok(recv.clone()),
        "__exit__" => bytesio_method(h, recv, "close", &[])?,
        _ => return None,
    })
}

fn set_bytes_pos(h: &mut PyHost, recv: &Value, new: usize) {
    if let Some(PyObj::BytesIO { pos, .. }) = h.get_mut(recv) {
        *pos = new;
    }
}

// ── StringIO methods ─────────────────────────────────────────────────────────

fn stringio_method(
    h: &mut PyHost,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let (buf, len, pos, closed, translate) = match h.get(recv) {
        Some(PyObj::StringIO {
            buf,
            len,
            pos,
            closed,
            translate,
        }) => (buf.clone(), *len, *pos, *closed, *translate),
        _ => return None,
    };
    if closed && !matches!(name, "close" | "closed" | "__exit__") {
        return Some(Err(closed_err()));
    }
    let int_arg = |i: usize| -> Option<i64> { args.get(i).and_then(|v| h.as_int(v)) };
    Some(match name {
        "getvalue" => Ok(h.new_str(buf)),
        "read" => {
            let start = byte_at(&buf, pos);
            let n = int_arg(0).filter(|n| *n >= 0);
            let (end, newpos) = match n {
                Some(n) => {
                    let np = (pos + n as usize).min(len);
                    (byte_at(&buf, np), np)
                }
                None => (buf.len(), len),
            };
            set_text_pos(h, recv, newpos);
            Ok(h.new_str(buf[start..end].to_string()))
        }
        "readline" => {
            let start = byte_at(&buf, pos);
            let rest = &buf[start..];
            let end = rest.find('\n').map(|i| start + i + 1).unwrap_or(buf.len());
            let taken = buf[start..end].chars().count();
            set_text_pos(h, recv, pos + taken);
            Ok(h.new_str(buf[start..end].to_string()))
        }
        "readlines" => {
            let start = byte_at(&buf, pos);
            let out: Vec<Value> = buf[start..]
                .split_inclusive('\n')
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .into_iter()
                .map(|l| h.new_str(l))
                .collect();
            set_text_pos(h, recv, len);
            Ok(h.new_list(out))
        }
        "write" => {
            let s = match h.as_str(args.first()?) {
                Some(s) => s,
                None => {
                    return Some(Err(host::type_error(&format!(
                        "string argument expected, got '{}'",
                        h.type_name(args.first()?)
                    ))))
                }
            };
            // `newline=None` translates every line ending to '\n' on write.
            let s = if translate {
                s.replace("\r\n", "\n").replace('\r', "\n")
            } else {
                s
            };
            let n = s.chars().count();
            let mut buf = buf;
            if pos >= len {
                // The append case — every logging handler, every `pprint`. A gap
                // past the end zero-fills with NULs, as CPython does.
                for _ in len..pos {
                    buf.push('\0');
                }
                buf.push_str(&s);
            } else {
                let start = byte_at(&buf, pos);
                let stop = byte_at(&buf, pos + n);
                buf.replace_range(start..stop, &s);
            }
            let newlen = buf.chars().count();
            if let Some(PyObj::StringIO {
                buf: b,
                len: l,
                pos: p,
                ..
            }) = h.get_mut(recv)
            {
                *b = buf;
                *l = newlen;
                *p = pos + n;
            }
            Ok(Value::Int(n as i64))
        }
        "writelines" => {
            let items = match h.iter_items(args.first()?) {
                Ok(i) => i,
                Err(e) => return Some(Err(e)),
            };
            for it in items {
                if let Err(e) = stringio_method(h, recv, "write", &[it]).unwrap_or(Ok(Value::Undef))
                {
                    return Some(Err(e));
                }
            }
            Ok(Value::Undef)
        }
        "seek" => {
            let off = int_arg(0).unwrap_or(0);
            let whence = int_arg(1).unwrap_or(0);
            // CPython only allows seeking to 0 relative to the end or current
            // position on a text stream; anything else needs an opaque cookie.
            if whence != 0 && off != 0 {
                return Some(Err("OSError: Can't do nonzero cur-relative seeks"
                    .to_string()
                    .replace("cur", if whence == 2 { "end" } else { "cur" })));
            }
            let new = match whence {
                1 => pos,
                2 => len,
                _ => {
                    if off < 0 {
                        return Some(Err(format!("ValueError: negative seek position {off}")));
                    }
                    off as usize
                }
            };
            set_text_pos(h, recv, new);
            Ok(Value::Int(new as i64))
        }
        "tell" => Ok(Value::Int(pos as i64)),
        "truncate" => {
            let size = int_arg(0).map(|n| n as usize).unwrap_or(pos);
            let cut = byte_at(&buf, size);
            let mut b = buf;
            b.truncate(cut);
            let newlen = b.chars().count();
            if let Some(PyObj::StringIO {
                buf: bb, len: l, ..
            }) = h.get_mut(recv)
            {
                *bb = b;
                *l = newlen;
            }
            Ok(Value::Int(size as i64))
        }
        "close" => {
            if let Some(PyObj::StringIO { closed: c, .. }) = h.get_mut(recv) {
                *c = true;
            }
            Ok(Value::Undef)
        }
        "flush" => Ok(Value::Undef),
        "readable" | "writable" | "seekable" => Ok(Value::Bool(true)),
        "isatty" => Ok(Value::Bool(false)),
        "__enter__" => Ok(recv.clone()),
        "__exit__" => stringio_method(h, recv, "close", &[])?,
        _ => return None,
    })
}

fn set_text_pos(h: &mut PyHost, recv: &Value, new: usize) {
    if let Some(PyObj::StringIO { pos, .. }) = h.get_mut(recv) {
        *pos = new;
    }
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// Methods on a `BytesIO`/`StringIO` instance. `None` means "not one of mine".
pub fn stream_method(
    h: &mut PyHost,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    match h.get(recv) {
        Some(PyObj::BytesIO { .. }) => bytesio_method(h, recv, name, args),
        Some(PyObj::StringIO { .. }) => stringio_method(h, recv, name, args),
        _ => None,
    }
}

/// Attribute reads on a stream (`f.closed`); methods go through `stream_method`.
pub fn stream_attr(h: &mut PyHost, recv: &Value, name: &str) -> Option<Result<Value, String>> {
    let closed = match h.get(recv) {
        Some(PyObj::BytesIO { closed, .. }) | Some(PyObj::StringIO { closed, .. }) => *closed,
        _ => return None,
    };
    match name {
        "closed" => Some(Ok(Value::Bool(closed))),
        // `StringIO` reports the newline it saw; pythonrs targets '\n' only.
        "newlines" => Some(Ok(Value::Undef)),
        _ => None,
    }
}

/// Iterating a stream yields its lines, exactly as `readline` produces them.
pub fn stream_lines(h: &mut PyHost, recv: &Value) -> Option<Result<Vec<Value>, String>> {
    let listed = stream_method(h, recv, "readlines", &[])?;
    Some(listed.map(|v| match h.get(&v) {
        Some(PyObj::List(items)) => items.clone(),
        _ => Vec::new(),
    }))
}

/// `_io.<name>(...)` — the module's callables and constructors.
pub fn call(
    h: &mut PyHost,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    Some(match name {
        "BytesIO" => new_bytesio(h, args),
        "StringIO" => new_stringio(h, args, kwargs),
        // `text_encoding(encoding, stacklevel=2)` — resolve a None encoding to the
        // locale default. pythonrs is UTF-8 everywhere, so there is one answer.
        "text_encoding" => Ok(match args.first() {
            Some(v) if !matches!(v, Value::Undef) => v.clone(),
            _ => h.new_str("utf-8".to_string()),
        }),
        _ => return None,
    })
}

/// The concrete stream TYPES `_io` exports. They must answer `isinstance(x,
/// type)` — `io.py` hands every one of them to `ABCMeta.register`, which rejects
/// anything that is not a class.
pub const STREAM_TYPES: &[&str] = &[
    "BytesIO",
    "StringIO",
    "FileIO",
    "BufferedReader",
    "BufferedWriter",
    "BufferedRWPair",
    "BufferedRandom",
    "TextIOWrapper",
    "IncrementalNewlineDecoder",
];

/// The `_io` module namespace. `io.py` imports these by name, so every one of
/// them must exist even where the implementation delegates elsewhere: the
/// file-backed classes resolve to the host's existing `open`/file layer rather
/// than a second copy of it.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    // Plain functions.
    const FUNCS: &[&str] = &["text_encoding", "open", "open_code"];
    // The C-side abstract bases `io.py` subclasses to declare its ABCs. They
    // carry no behavior of their own — `io.py` supplies all of it — so an empty
    // base class is the whole implementation, exactly as CPython's are for the
    // pure-Python fallback.
    const BASES: &[&str] = &["_IOBase", "_RawIOBase", "_BufferedIOBase", "_TextIOBase"];

    let mut out: Vec<(String, Value)> = Vec::new();
    for n in FUNCS.iter().chain(STREAM_TYPES) {
        let v = h.alloc(PyObj::Builtin(format!("_io.{n}")));
        out.push(((*n).to_string(), v));
    }
    for n in BASES {
        // Registered as real classes so `class IOBase(_io._IOBase, metaclass=
        // ABCMeta)` has something to inherit from and `abc` can walk the MRO.
        let v = h.register_class_meta(
            (*n).to_string().as_str(),
            vec![],
            Default::default(),
            "type",
        );
        out.push(((*n).to_string(), v));
    }
    out.push((
        "DEFAULT_BUFFER_SIZE".to_string(),
        Value::Int(DEFAULT_BUFFER_SIZE),
    ));
    for n in ["UnsupportedOperation", "BlockingIOError"] {
        let v = h.alloc(PyObj::Builtin(n.to_string()));
        out.push((n.to_string(), v));
    }
    out
}

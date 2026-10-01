//! `UnicodeEncodeError`, `UnicodeDecodeError` and `UnicodeTranslateError`:
//! their constructor, their attributes and their `__str__`.
//!
//! Ported from CPython's `Objects/exceptions.c` (`UnicodeEncodeError_init` /
//! `_str`, `UnicodeDecodeError_init` / `_str`, `UnicodeTranslateError_init` /
//! `_str`). The constructor arguments ARE the exception's state:
//! `args == (encoding, object, start, end, reason)` (no `encoding` for the
//! translate error), and `.encoding`, `.object`, `.start`, `.end` and `.reason`
//! read the same values back. `__str__` is rendered from them, so it is never
//! stored.
//!
//! A codec failing inside pythonrs raises before any exception object exists —
//! an error is its `"Class: message"` line until `synth_exc` builds the object
//! a handler binds. [`raise`] therefore leaves the structured arguments beside
//! the line it returns, and [`take_pending`] hands them to `synth_exc` on an
//! exact line match, as `host::ForeignExc` does for an exception raised over
//! the stdlib-ffi bridge. The record lives in a thread-local rather than on
//! `PyHost` because the codecs raise from inside host borrows.

use crate::host::{self, with_host, PyHost, PyObj};
use fusevm::Value;
use std::cell::RefCell;

/// The object a codec was working on: the `bytes` a decoder read, or the
/// `str` an encoder or translator read.
#[derive(Clone, Debug)]
pub enum CodecInput {
    Bytes(Vec<u8>),
    Str(String),
}

/// The structured arguments of one Unicode error, as a codec raise site knows
/// them. `start`/`end` index `object` (bytes for a decoder, characters for an
/// encoder), `end` exclusive.
#[derive(Clone, Debug)]
pub struct UnicodeErrorArgs {
    pub class: &'static str,
    pub encoding: String,
    pub object: CodecInput,
    pub start: usize,
    pub end: usize,
    pub reason: String,
}

thread_local! {
    /// The Unicode error last raised by a native codec, with the line it
    /// rendered to (see the module docs).
    static PENDING: RefCell<Option<(String, UnicodeErrorArgs)>> = const { RefCell::new(None) };
}

/// The error line for `args`, with the arguments recorded so the exception
/// `synth_exc` builds from that line carries them.
pub fn raise(args: UnicodeErrorArgs) -> String {
    let (start, end) = (args.start as i64, args.end as i64);
    let msg = match &args.object {
        CodecInput::Bytes(b) => decode_str(&args.encoding, b, start, end, &args.reason),
        CodecInput::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            encode_str(&args.encoding, &chars, start, end, &args.reason)
        }
    };
    let line = format!("{}: {msg}", args.class);
    PENDING.with(|p| *p.borrow_mut() = Some((line.clone(), args)));
    line
}

/// The arguments recorded for `line`, consumed.
pub fn take_pending(line: &str) -> Option<UnicodeErrorArgs> {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        match &*p {
            Some((l, _)) if l == line => p.take().map(|(_, a)| a),
            _ => None,
        }
    })
}

/// The argument tuple a recorded error is constructed from.
pub fn arg_values(h: &mut PyHost, a: &UnicodeErrorArgs) -> Vec<Value> {
    let object = match &a.object {
        CodecInput::Bytes(b) => h.alloc(PyObj::Bytes(b.clone())),
        CodecInput::Str(s) => h.new_str(s.clone()),
    };
    let mut args = Vec::with_capacity(5);
    if a.class != "UnicodeTranslateError" {
        args.push(h.new_str(a.encoding.clone()));
    }
    args.push(object);
    args.push(Value::Int(a.start as i64));
    args.push(Value::Int(a.end as i64));
    args.push(h.new_str(a.reason.clone()));
    args
}

/// Whether `class` is one of the three structured Unicode errors.
pub fn is_unicode_error_class(class: &str) -> bool {
    matches!(
        class,
        "UnicodeEncodeError" | "UnicodeDecodeError" | "UnicodeTranslateError"
    )
}

/// The constructor's argument checks (`PyArg_ParseTuple` with `"UOnnU"`, or
/// `"UnnU"` for the translate error, then the object's own type check), worded
/// as /opt/homebrew/bin/python3.14 words them, followed by the attributes.
/// Runs without the host borrow: an `__index__` bound may run user code.
pub fn init(e: &Value, class: &str, args: &[Value]) -> Result<(), String> {
    let translate = class == "UnicodeTranslateError";
    let want = if translate { 4 } else { 5 };
    if args.len() != want {
        return Err(host::type_error(&format!(
            "function takes exactly {want} arguments ({} given)",
            args.len()
        )));
    }
    let str_arg = |i: usize| -> Result<Value, String> {
        let v = &args[i];
        if with_host(|h| matches!(h.get(v), Some(PyObj::Str(_)))) {
            Ok(v.clone())
        } else {
            let t = with_host(|h| h.type_name(v));
            Err(host::type_error(&format!(
                "argument {} must be str, not {t}",
                i + 1
            )))
        }
    };
    let ssize_arg = |i: usize| -> Result<Value, String> {
        let v = crate::builtins::index_dunder(&args[i])?.unwrap_or_else(|| args[i].clone());
        match with_host(|h| h.big_val(&v)) {
            Some(n) if i64::try_from(&n).is_ok() => Ok(Value::Int(i64::try_from(&n).unwrap())),
            Some(_) => Err("OverflowError: Python int too large to convert to C ssize_t".into()),
            None => {
                let t = with_host(|h| h.type_name(&v));
                Err(host::type_error(&format!(
                    "'{t}' object cannot be interpreted as an integer"
                )))
            }
        }
    };
    let off = usize::from(!translate);
    let encoding = if translate { Value::Undef } else { str_arg(0)? };
    let object = &args[off];
    let start = ssize_arg(off + 1)?;
    let end = ssize_arg(off + 2)?;
    let reason = str_arg(off + 3)?;
    // The object's own check comes after the format's: a decoder's is any
    // bytes-like object, kept as `bytes`; the others' is a `str`.
    let object = if class == "UnicodeDecodeError" {
        match crate::builtins::as_bytes_object(object)? {
            Some(b) => with_host(|h| h.alloc(PyObj::Bytes(b))),
            None => {
                let t = with_host(|h| h.type_name(object));
                return Err(host::type_error(&format!(
                    "a bytes-like object is required, not '{t}'"
                )));
            }
        }
    } else if with_host(|h| matches!(h.get(object), Some(PyObj::Str(_)))) {
        object.clone()
    } else {
        let t = with_host(|h| h.type_name(object));
        return Err(host::type_error(&format!(
            "argument {} must be str, not {t}",
            off + 1
        )));
    };
    with_host(|h| set_attrs(h, e, [encoding, object, start, end, reason]));
    Ok(())
}

/// Bind the attributes of an exception whose arguments are already the
/// checked tuple — one a codec raised, natively or over the bridge.
pub fn bind_args(h: &mut PyHost, e: &Value, class: &str, args: &[Value]) {
    let attrs = match (class, args) {
        ("UnicodeTranslateError", [o, s, en, r]) => {
            [Value::Undef, o.clone(), s.clone(), en.clone(), r.clone()]
        }
        (_, [enc, o, s, en, r]) if class != "UnicodeTranslateError" => {
            [enc.clone(), o.clone(), s.clone(), en.clone(), r.clone()]
        }
        _ => return,
    };
    set_attrs(h, e, attrs);
}

fn set_attrs(h: &mut PyHost, e: &Value, vals: [Value; 5]) {
    for (name, v) in ["encoding", "object", "start", "end", "reason"]
        .into_iter()
        .zip(vals)
    {
        let _ = h.set_attr(e, name, v);
    }
}

/// `__str__` of a Unicode error built from `args`, or `None` when the arguments
/// are not the checked tuple (a bare `UnicodeError('m')` renders as any
/// exception does).
pub fn message(h: &PyHost, class: &str, args: &[Value]) -> Option<String> {
    if !is_unicode_error_class(class) {
        return None;
    }
    let int = |v: &Value| match v {
        Value::Int(n) => Some(*n),
        _ => None,
    };
    if class == "UnicodeTranslateError" {
        let [o, s, e, r] = args else { return None };
        let chars: Vec<char> = h.as_str(o)?.chars().collect();
        return Some(translate_str(&chars, int(s)?, int(e)?, &h.str_of(r)));
    }
    let [enc, o, s, e, r] = args else { return None };
    let (enc, start, end, reason) = (h.str_of(enc), int(s)?, int(e)?, h.str_of(r));
    match (class, h.get(o)) {
        ("UnicodeDecodeError", Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b))) => {
            Some(decode_str(&enc, b, start, end, &reason))
        }
        ("UnicodeEncodeError", Some(PyObj::Str(_))) => {
            let chars: Vec<char> = h.as_str(o)?.chars().collect();
            Some(encode_str(&enc, &chars, start, end, &reason))
        }
        _ => None,
    }
}

/// `__str__` of the Unicode error `v`, read from its attributes as CPython's
/// `_str` functions read the instance fields — so assigning `e.reason` or
/// `e.start` changes the rendering while `args` keeps the constructor's values.
/// `None` when `v` is not one whose attributes are bound.
pub fn str_from_attrs(h: &PyHost, v: &Value, class: &str) -> Option<String> {
    if !is_unicode_error_class(class) {
        return None;
    }
    let Value::Obj(id) = v else { return None };
    let attrs = h.func_attrs.get(id)?;
    let get = |n: &str| attrs.get(n).cloned();
    let fields = if class == "UnicodeTranslateError" {
        vec![get("object")?, get("start")?, get("end")?, get("reason")?]
    } else {
        vec![
            get("encoding")?,
            get("object")?,
            get("start")?,
            get("end")?,
            get("reason")?,
        ]
    };
    message(h, class, &fields)
}

/// The one offending item when `[start, end)` names exactly one in range —
/// the condition every `_str` function tests before naming it.
fn single(len: usize, start: i64, end: i64) -> bool {
    (0..len as i64).contains(&start) && (0..=len as i64).contains(&end) && end == start + 1
}

/// How `_str` spells one character: `\xNN`, `\uNNNN` or `\UNNNNNNNN` by size.
fn char_escape(c: char) -> String {
    match c as u32 {
        n @ 0..=0xff => format!("\\x{n:02x}"),
        n @ 0x100..=0xffff => format!("\\u{n:04x}"),
        n => format!("\\U{n:08x}"),
    }
}

/// `UnicodeEncodeError_str`.
fn encode_str(enc: &str, object: &[char], start: i64, end: i64, reason: &str) -> String {
    if single(object.len(), start, end) {
        let c = char_escape(object[start as usize]);
        format!("'{enc}' codec can't encode character '{c}' in position {start}: {reason}")
    } else {
        let last = end - 1;
        format!("'{enc}' codec can't encode characters in position {start}-{last}: {reason}")
    }
}

/// `UnicodeDecodeError_str`.
fn decode_str(enc: &str, object: &[u8], start: i64, end: i64, reason: &str) -> String {
    if single(object.len(), start, end) {
        let b = object[start as usize];
        format!("'{enc}' codec can't decode byte 0x{b:02x} in position {start}: {reason}")
    } else {
        let last = end - 1;
        format!("'{enc}' codec can't decode bytes in position {start}-{last}: {reason}")
    }
}

/// `UnicodeTranslateError_str`.
fn translate_str(object: &[char], start: i64, end: i64, reason: &str) -> String {
    if single(object.len(), start, end) {
        let c = char_escape(object[start as usize]);
        format!("can't translate character '{c}' in position {start}: {reason}")
    } else {
        let last = end - 1;
        format!("can't translate characters in position {start}-{last}: {reason}")
    }
}

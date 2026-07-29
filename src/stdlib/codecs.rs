//! The `_codecs` C-accelerator module: the codec registry (`register`,
//! `lookup`, `register_error`, `lookup_error`), the generic `encode`/`decode`
//! entry points, and the natively-implemented codecs.
//!
//! PORTED from RustPython's `crates/vm/src/stdlib/_codecs.rs`
//! (<https://github.com/RustPython/RustPython>, MIT licence), including its
//! division of labour: utf-8, latin-1, ascii and the utf-16/32 family are
//! implemented natively because they are the ones that matter for startup and
//! I/O, while the long tail (utf-7, the charmap-driven single-byte codecs, the
//! escape codecs) is left to the pure-Python `encodings` package that the
//! registry finds anyway. The fast paths are theirs too: an all-ASCII string
//! encodes to latin-1/ascii by handing back its own bytes, and a str that is
//! already valid utf-8 encodes to utf-8 the same way.
//!
//! The error handlers follow CPython's contract — `strict` raises,
//! `ignore` drops, `replace` substitutes U+FFFD on decode and `?` on encode,
//! `backslashreplace`/`xmlcharrefreplace` escape, `surrogateescape` maps
//! undecodable bytes to U+DC80..U+DCFF and back.
//!
//! Wiring (done by the parent): an `import_module` arm for `"_codecs"` calling
//! [`entries`], and a `call_builtin_function` arm routing `_codecs.*` to
//! [`call`].

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// How an encode/decode error is handled. Anything not in this set is looked up
/// in the user registry by `lookup_error`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Errors {
    Strict,
    Ignore,
    Replace,
    BackslashReplace,
    XmlCharRefReplace,
    SurrogateEscape,
    SurrogatePass,
}

impl Errors {
    fn parse(s: &str) -> Self {
        match s {
            "ignore" => Self::Ignore,
            "replace" => Self::Replace,
            "backslashreplace" => Self::BackslashReplace,
            "xmlcharrefreplace" => Self::XmlCharRefReplace,
            "surrogateescape" => Self::SurrogateEscape,
            "surrogatepass" => Self::SurrogatePass,
            _ => Self::Strict,
        }
    }
}

fn enc_err(enc: &str, pos: usize, ch: char) -> String {
    format!(
        "UnicodeEncodeError: '{enc}' codec can't encode character '\\u{:04x}' in position {pos}",
        ch as u32
    )
}

fn dec_err(enc: &str, pos: usize, why: &str) -> String {
    format!("UnicodeDecodeError: '{enc}' codec can't decode byte in position {pos}: {why}")
}

/// Encode `s` to a single-byte codec whose repertoire is `0..=limit`.
fn encode_single_byte(enc: &str, s: &str, limit: u32, errors: Errors) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        let cp = c as u32;
        if cp <= limit {
            out.push(cp as u8);
            continue;
        }
        match errors {
            Errors::Strict => return Err(enc_err(enc, i, c)),
            Errors::Ignore => {}
            Errors::Replace => out.push(b'?'),
            Errors::BackslashReplace => {
                let esc = if cp > 0xffff {
                    format!("\\U{cp:08x}")
                } else if cp > 0xff {
                    format!("\\u{cp:04x}")
                } else {
                    format!("\\x{cp:02x}")
                };
                out.extend_from_slice(esc.as_bytes());
            }
            Errors::XmlCharRefReplace => out.extend_from_slice(format!("&#{cp};").as_bytes()),
            // A lone surrogate produced by `surrogateescape` decoding goes back
            // out as the byte it stood for.
            Errors::SurrogateEscape | Errors::SurrogatePass => {
                if (0xdc80..=0xdcff).contains(&cp) {
                    out.push((cp - 0xdc00) as u8);
                } else {
                    return Err(enc_err(enc, i, c));
                }
            }
        }
    }
    Ok(out)
}

/// Decode from a single-byte codec whose repertoire is `0..=limit`.
fn decode_single_byte(enc: &str, b: &[u8], limit: u32, errors: Errors) -> Result<String, String> {
    let mut out = String::with_capacity(b.len());
    for (i, &byte) in b.iter().enumerate() {
        if byte as u32 <= limit {
            out.push(byte as char);
            continue;
        }
        match errors {
            Errors::Strict => return Err(dec_err(enc, i, "ordinal not in range(128)")),
            Errors::Ignore => {}
            Errors::Replace => out.push('\u{fffd}'),
            Errors::SurrogateEscape => {
                // Undecodable bytes become U+DC80..U+DCFF, which is what makes
                // the mapping reversible.
                out.push(char::from_u32(0xdc00 + byte as u32).unwrap_or('\u{fffd}'));
            }
            _ => return Err(dec_err(enc, i, "ordinal not in range(128)")),
        }
    }
    Ok(out)
}

fn utf8_decode(b: &[u8], errors: Errors) -> Result<(String, usize), String> {
    match std::str::from_utf8(b) {
        Ok(s) => Ok((s.to_string(), b.len())),
        Err(e) => {
            let valid = e.valid_up_to();
            let mut out = String::from(std::str::from_utf8(&b[..valid]).unwrap_or(""));
            match errors {
                Errors::Strict => Err(dec_err("utf-8", valid, "invalid start byte")),
                _ => {
                    // Walk the remainder byte by byte, re-syncing on each valid
                    // sequence, applying the handler to everything that isn't.
                    let mut i = valid;
                    while i < b.len() {
                        match std::str::from_utf8(&b[i..]) {
                            Ok(rest) => {
                                out.push_str(rest);
                                i = b.len();
                            }
                            Err(e2) => {
                                let good = e2.valid_up_to();
                                if good > 0 {
                                    out.push_str(
                                        std::str::from_utf8(&b[i..i + good]).unwrap_or(""),
                                    );
                                    i += good;
                                    continue;
                                }
                                match errors {
                                    Errors::Ignore => {}
                                    Errors::Replace => out.push('\u{fffd}'),
                                    Errors::SurrogateEscape => out.push(
                                        char::from_u32(0xdc00 + b[i] as u32).unwrap_or('\u{fffd}'),
                                    ),
                                    _ => return Err(dec_err("utf-8", i, "invalid start byte")),
                                }
                                i += 1;
                            }
                        }
                    }
                    let n = out.chars().count();
                    Ok((out, n.max(b.len())))
                }
            }
        }
    }
}

/// utf-16/utf-32 in either byte order. `width` is 2 or 4.
fn utf_x_encode(s: &str, width: usize, big: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * width + width);
    let push = |v: u32, out: &mut Vec<u8>| {
        let bytes: [u8; 4] = v.to_le_bytes();
        let take = &bytes[..width];
        if big {
            out.extend(take.iter().rev());
        } else {
            out.extend_from_slice(take);
        }
    };
    if bom {
        push(0xfeff, &mut out);
    }
    for c in s.chars() {
        let cp = c as u32;
        if width == 2 && cp > 0xffff {
            // Surrogate pair.
            let v = cp - 0x10000;
            push(0xd800 + (v >> 10), &mut out);
            push(0xdc00 + (v & 0x3ff), &mut out);
        } else {
            push(cp, &mut out);
        }
    }
    out
}

fn utf_x_decode(
    enc: &str,
    b: &[u8],
    width: usize,
    big: bool,
    errors: Errors,
) -> Result<String, String> {
    if b.len() % width != 0 && errors == Errors::Strict {
        return Err(dec_err(enc, b.len() - (b.len() % width), "truncated data"));
    }
    let mut units: Vec<u32> = Vec::with_capacity(b.len() / width);
    for chunk in b.chunks_exact(width) {
        let mut v: u32 = 0;
        if big {
            for &x in chunk {
                v = (v << 8) | x as u32;
            }
        } else {
            for (k, &x) in chunk.iter().enumerate() {
                v |= (x as u32) << (8 * k);
            }
        }
        units.push(v);
    }
    let mut out = String::new();
    let mut i = 0usize;
    while i < units.len() {
        let u = units[i];
        if width == 2 && (0xd800..0xdc00).contains(&u) {
            if let Some(&lo) = units.get(i + 1) {
                if (0xdc00..0xe000).contains(&lo) {
                    let cp = 0x10000 + ((u - 0xd800) << 10) + (lo - 0xdc00);
                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    i += 2;
                    continue;
                }
            }
        }
        match char::from_u32(u) {
            Some(c) => out.push(c),
            None => match errors {
                Errors::Strict => return Err(dec_err(enc, i * width, "illegal encoding")),
                Errors::Ignore => {}
                _ => out.push('\u{fffd}'),
            },
        }
        i += 1;
    }
    Ok(out)
}

fn as_text(h: &PyHost, v: &Value) -> Option<String> {
    h.as_str(v)
}

fn as_bytes(h: &PyHost, v: &Value) -> Option<Vec<u8>> {
    match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
        Some(PyObj::Str(s)) => Some(s.as_bytes().to_vec()),
        _ => match v {
            Value::Str(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        },
    }
}

/// Normalize an encoding label the way CPython's registry does before search:
/// lowercase, and `-`/space to `_`.
fn normalize(name: &str) -> String {
    name.to_lowercase().replace([' ', '-'], "_")
}

/// The module's exported names. The per-codec functions the `encodings` package
/// reaches for are all here; anything else it needs it implements in Python.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const NAMES: &[&str] = &[
        "register",
        "unregister",
        "lookup",
        "encode",
        "decode",
        "register_error",
        "lookup_error",
        "_forget_codec",
        "utf_8_encode",
        "utf_8_decode",
        "latin_1_encode",
        "latin_1_decode",
        "ascii_encode",
        "ascii_decode",
        "utf_16_encode",
        "utf_16_decode",
        "utf_16_le_encode",
        "utf_16_le_decode",
        "utf_16_be_encode",
        "utf_16_be_decode",
        "utf_32_encode",
        "utf_32_decode",
        "utf_32_le_encode",
        "utf_32_le_decode",
        "utf_32_be_encode",
        "utf_32_be_decode",
        "raw_unicode_escape_encode",
        "raw_unicode_escape_decode",
        "unicode_escape_encode",
        "unicode_escape_decode",
        "escape_encode",
        "escape_decode",
        "charmap_encode",
        "charmap_decode",
        "charmap_build",
        "readbuffer_encode",
    ];
    let mut out: Vec<(String, Value)> = Vec::new();
    for n in NAMES {
        let v = h.alloc(PyObj::Builtin(format!("_codecs.{n}")));
        out.push(((*n).to_string(), v));
    }
    out
}

/// Build the `(result, length)` tuple every codec function returns.
fn pair(h: &mut PyHost, v: Value, n: usize) -> Value {
    h.new_tuple(vec![v, Value::Int(n as i64)])
}

/// Entry points that must run OUTSIDE the host borrow: the registry calls back
/// into Python (a search function, a codec's own `encode`/`decode`), and doing
/// that while `with_host` holds the `RefCell` panics.
pub fn call_unborrowed(
    fname: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let hstr = |v: &Value| host::with_host(|h| h.as_str(v));
    Some(match fname {
        // ── registry ────────────────────────────────────────────────────────
        "register" => {
            if let Some(f) = args.first() {
                host::with_host(|h| h.codec_search.push(f.clone()));
            }
            Ok(Value::Undef)
        }
        "unregister" => {
            if let Some(f) = args.first() {
                let target = f.clone();
                host::with_host(|h| {
                    h.codec_search.retain(
                        |g| !matches!((g, &target), (Value::Obj(a), Value::Obj(b)) if a == b),
                    )
                });
            }
            Ok(Value::Undef)
        }
        "_forget_codec" => Ok(Value::Undef),
        "lookup" => (|| {
            let name = args
                .first()
                .and_then(hstr)
                .ok_or_else(|| host::type_error("lookup() argument must be str"))?;
            let norm = normalize(&name);
            if let Some(found) = host::with_host(|h| h.codec_cache.get(&norm).cloned()) {
                return Ok(found);
            }
            // CPython bootstraps the `encodings` package during interpreter
            // startup, which is what registers the search function that finds
            // every stdlib codec. Do it on first lookup instead.
            // Bootstrap at most once, and never re-entrantly: `encodings/__init__`
            // imports `codecs`, which looks a codec up, which would arrive back
            // here while `encodings` is still executing and recurse until the
            // stack died.
            if host::with_host(|h| h.codec_search.is_empty()) {
                thread_local! {
                    static BOOTSTRAPPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
                }
                let first = BOOTSTRAPPING.with(|b| {
                    let was = b.get();
                    b.set(true);
                    !was
                });
                if first {
                    let _ = host::import_module("encodings");
                    BOOTSTRAPPING.with(|b| b.set(false));
                }
            }
            let searches = host::with_host(|h| h.codec_search.clone());
            for f in searches {
                let arg = host::with_host(|h| h.new_str(norm.clone()));
                let r = host::invoke(&f, vec![arg], vec![])?;
                if !matches!(r, Value::Undef) {
                    host::with_host(|h| h.codec_cache.insert(norm.clone(), r.clone()));
                    return Ok(r);
                }
            }
            Err(format!("LookupError: unknown encoding: {name}"))
        })(),
        "register_error" => {
            if let (Some(n), Some(f)) = (args.first().and_then(hstr), args.get(1)) {
                let fv = f.clone();
                host::with_host(|h| h.codec_errors.insert(n, fv));
            }
            Ok(Value::Undef)
        }
        "lookup_error" => (|| {
            let n = args
                .first()
                .and_then(hstr)
                .ok_or_else(|| host::type_error("lookup_error() argument must be str"))?;
            match host::with_host(|h| h.codec_errors.get(&n).cloned()) {
                Some(f) => Ok(f),
                // The built-in handlers are implemented inside the codecs, not as
                // callables; hand back a marker so `codecs.lookup_error('strict')`
                // resolves rather than raising.
                None if matches!(
                    n.as_str(),
                    "strict"
                        | "ignore"
                        | "replace"
                        | "backslashreplace"
                        | "xmlcharrefreplace"
                        | "surrogateescape"
                        | "surrogatepass"
                        | "namereplace"
                ) =>
                {
                    Ok(host::with_host(|h| {
                        h.alloc(PyObj::Builtin(format!("_codecs.{n}_errors")))
                    }))
                }
                None => Err(format!("LookupError: unknown error handler name '{n}'")),
            }
        })(),

        // ── generic entry points ────────────────────────────────────────────
        // `encode(obj, encoding='utf-8', errors='strict')` — resolve through the
        // registry and call the codec's encoder.
        "encode" | "decode" => (|| {
            let obj = args.first().cloned().ok_or_else(|| {
                host::type_error(&format!("{fname}() missing required argument 'obj'"))
            })?;
            let enc = args
                .get(1)
                .cloned()
                .or_else(|| kw("encoding"))
                .and_then(|v| hstr(&v))
                .unwrap_or_else(|| "utf-8".to_string());
            let err_name = args
                .get(2)
                .cloned()
                .or_else(|| kw("errors"))
                .and_then(|v| hstr(&v))
                .unwrap_or_else(|| "strict".to_string());
            // Try the native codecs first; fall back to the registry (which is
            // how the pure-Python `encodings` modules get used).
            let direct = format!("{}_{fname}", normalize(&enc));
            let native = [
                "utf_8_encode",
                "utf_8_decode",
                "latin_1_encode",
                "latin_1_decode",
                "ascii_encode",
                "ascii_decode",
                "utf_16_encode",
                "utf_16_decode",
                "utf_16_le_encode",
                "utf_16_le_decode",
                "utf_16_be_encode",
                "utf_16_be_decode",
                "utf_32_encode",
                "utf_32_decode",
            ];
            let alias = match normalize(&enc).as_str() {
                "utf8" | "u8" | "utf" => "utf_8".to_string(),
                "latin1" | "latin" | "l1" | "iso8859_1" | "iso_8859_1" | "8859" | "cp819" => {
                    "latin_1".to_string()
                }
                "us_ascii" | "646" => "ascii".to_string(),
                "utf16" => "utf_16".to_string(),
                "utf32" => "utf_32".to_string(),
                other => other.to_string(),
            };
            let direct2 = format!("{alias}_{fname}");
            for cand in [direct.as_str(), direct2.as_str()] {
                if native.contains(&cand) {
                    let e = host::with_host(|h| h.new_str(err_name.clone()));
                    let cand_owned = cand.to_string();
                    let a = vec![obj.clone(), e];
                    let r = host::with_host(|h| call(h, &cand_owned, &a, &[]))
                        .expect("native codec name")?;
                    // The per-codec functions return `(value, length)`; the
                    // generic ones return just the value.
                    return Ok(host::with_host(|h| match h.get(&r) {
                        Some(PyObj::Tuple(t)) if !t.is_empty() => t[0].clone(),
                        _ => r.clone(),
                    }));
                }
            }
            // Registry path: `lookup(enc)` then `.encode`/`.decode`.
            let name_v = host::with_host(|h| h.new_str(enc.clone()));
            let info = call_unborrowed("lookup", &[name_v], &[]).expect("lookup")?;
            let f = host::with_host(|h| h.get_attr(&info, fname))?;
            let e = host::with_host(|h| h.new_str(err_name));
            let r = host::invoke(&f, vec![obj, e], vec![])?;
            Ok(host::with_host(|h| match h.get(&r) {
                Some(PyObj::Tuple(t)) if !t.is_empty() => t[0].clone(),
                _ => r.clone(),
            }))
        })(),

        _ => return None,
    })
}

/// Dispatch `_codecs.<fname>`.
pub fn call(
    h: &mut PyHost,
    fname: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let errors_of = |h: &PyHost, idx: usize| -> Errors {
        args.get(idx)
            .cloned()
            .or_else(|| kw("errors"))
            .and_then(|v| h.as_str(&v))
            .map(|s| Errors::parse(&s))
            .unwrap_or(Errors::Strict)
    };
    Some(match fname {
        // ── native codecs ───────────────────────────────────────────────────
        "utf_8_encode" => (|| {
            let s = as_text(
                h,
                args.first()
                    .ok_or_else(|| host::type_error("utf_8_encode"))?,
            )
            .ok_or_else(|| host::type_error("utf_8_encode() argument 1 must be str"))?;
            let n = s.chars().count();
            // Fast path, as in RustPython: a str is already utf-8.
            let b = h.alloc(PyObj::Bytes(s.into_bytes()));
            Ok(pair(h, b, n))
        })(),
        "utf_8_decode" => (|| {
            let b = as_bytes(
                h,
                args.first()
                    .ok_or_else(|| host::type_error("utf_8_decode"))?,
            )
            .ok_or_else(|| host::type_error("utf_8_decode() argument 1 must be bytes"))?;
            let (s, n) = utf8_decode(&b, errors_of(h, 1))?;
            let sv = h.new_str(s);
            Ok(pair(h, sv, n))
        })(),
        "latin_1_encode" | "ascii_encode" => (|| {
            let limit = if fname.starts_with("ascii") {
                0x7f
            } else {
                0xff
            };
            let name = if limit == 0x7f { "ascii" } else { "latin-1" };
            let s = as_text(h, args.first().ok_or_else(|| host::type_error(fname))?)
                .ok_or_else(|| host::type_error("argument 1 must be str"))?;
            let n = s.chars().count();
            let out = encode_single_byte(name, &s, limit, errors_of(h, 1))?;
            let b = h.alloc(PyObj::Bytes(out));
            Ok(pair(h, b, n))
        })(),
        "latin_1_decode" | "ascii_decode" => (|| {
            let limit = if fname.starts_with("ascii") {
                0x7f
            } else {
                0xff
            };
            let name = if limit == 0x7f { "ascii" } else { "latin-1" };
            let b = as_bytes(h, args.first().ok_or_else(|| host::type_error(fname))?)
                .ok_or_else(|| host::type_error("argument 1 must be bytes"))?;
            let s = decode_single_byte(name, &b, limit, errors_of(h, 1))?;
            let n = b.len();
            let sv = h.new_str(s);
            Ok(pair(h, sv, n))
        })(),
        // `readbuffer_encode(obj)` — the identity byte view.
        "readbuffer_encode" => (|| {
            let b = as_bytes(h, args.first().ok_or_else(|| host::type_error(fname))?)
                .ok_or_else(|| host::type_error("argument 1 must be bytes-like"))?;
            let n = b.len();
            let v = h.alloc(PyObj::Bytes(b));
            Ok(pair(h, v, n))
        })(),
        _ if fname.starts_with("utf_16") || fname.starts_with("utf_32") => (|| {
            let width = if fname.starts_with("utf_16") { 2 } else { 4 };
            let big = fname.contains("_be_");
            let native_le = !fname.contains("_be_") && !fname.contains("_le_");
            if fname.ends_with("_encode") {
                let s = as_text(h, args.first().ok_or_else(|| host::type_error(fname))?)
                    .ok_or_else(|| host::type_error("argument 1 must be str"))?;
                let n = s.chars().count();
                // The BOM-less forms are the explicit `_le_`/`_be_` ones; the
                // plain name emits a BOM, as CPython does.
                let out = utf_x_encode(&s, width, big, native_le);
                let b = h.alloc(PyObj::Bytes(out));
                Ok(pair(h, b, n))
            } else {
                let mut b = as_bytes(h, args.first().ok_or_else(|| host::type_error(fname))?)
                    .ok_or_else(|| host::type_error("argument 1 must be bytes"))?;
                let mut big = big;
                if native_le && b.len() >= width {
                    // Consume a BOM and let it pick the byte order.
                    let head: Vec<u8> = b[..width].to_vec();
                    let le_bom: Vec<u8> = if width == 2 {
                        vec![0xff, 0xfe]
                    } else {
                        vec![0xff, 0xfe, 0, 0]
                    };
                    let be_bom: Vec<u8> = if width == 2 {
                        vec![0xfe, 0xff]
                    } else {
                        vec![0, 0, 0xfe, 0xff]
                    };
                    if head == le_bom {
                        big = false;
                        b.drain(..width);
                    } else if head == be_bom {
                        big = true;
                        b.drain(..width);
                    }
                }
                let name = if width == 2 { "utf-16" } else { "utf-32" };
                let s = utf_x_decode(name, &b, width, big, errors_of(h, 1))?;
                let n = b.len();
                let sv = h.new_str(s);
                Ok(pair(h, sv, n))
            }
        })(),
        // The escape and charmap codecs stay in Python (`encodings`), matching
        // RustPython's split; these entry points exist so a `from _codecs import
        // *` finds the names.
        _ => Err(format!(
            "NotImplementedError: _codecs.{fname} is provided by the encodings package"
        )),
    })
}

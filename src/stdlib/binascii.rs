//! The `binascii` C-accelerator module: `hexlify`/`b2a_hex`,
//! `unhexlify`/`a2b_hex`, `b2a_base64`, `a2b_base64`, `crc32`.
//!
//! PORTED from RustPython's `crates/stdlib/src/binascii.rs`
//! (<https://github.com/RustPython/RustPython>, MIT licence). The base64 decoder
//! is the interesting part and is theirs line for line: a 256-entry table
//! mapping ASCII to its 6-bit value (with `=` mapped to 0), then a four-state
//! quad machine that shifts nibbles across byte boundaries. Its behaviour in
//! non-strict mode — silently skipping anything not in the alphabet, and
//! stopping at padding once a quad is at least half full — is what CPython does
//! and what `base64.b64decode` relies on. `strict_mode` reproduces CPython's
//! four distinct errors (leading pad, non-base64 byte, discontinuous padding,
//! excess data after padding).
//!
//! Rewritten for pythonrs's `Value`/`PyHost`: RustPython's `ArgBytesLike` /
//! `ArgAsciiBuffer` argument adapters and its `crc32fast` dependency have no
//! equivalent here, so buffers are read through the host and CRC-32 is the
//! standard reflected table computed once on first use.
//!
//! Wiring (done by the parent): an `import_module` arm for `"binascii"` calling
//! [`entries`], and a `call_builtin_function` arm routing `binascii.*` to
//! [`call`].

use crate::host::{PyHost, PyObj};
use fusevm::Value;

fn err(msg: &str) -> String {
    format!("binascii.Error: {msg}")
}

/// Bytes out of any buffer-ish value: `bytes`, `bytearray`, or (for the ASCII
/// -only entry points) a `str`.
fn buf(h: &PyHost, v: &Value) -> Result<Vec<u8>, String> {
    match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(b.clone()),
        Some(PyObj::Str(s)) => Ok(s.as_bytes().to_vec()),
        _ => match v {
            Value::Str(s) => Ok(s.as_bytes().to_vec()),
            _ => Err(crate::host::type_error(
                "argument should be a bytes-like object or ASCII string",
            )),
        },
    }
}

fn hex_nibble(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        _ => b'a' + (n - 10),
    }
}

fn unhex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

const PAD: u8 = b'=';

/// ASCII → 6-bit value, `-1` for "not in the alphabet". `=` maps to 0 (it is
/// the pad character; the quad machine tracks padding separately).
#[rustfmt::skip]
const BASE64_TABLE: [i8; 256] = [
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,62, -1,-1,-1,63,
    52,53,54,55, 56,57,58,59, 60,61,-1,-1, -1, 0,-1,-1, /* '=' -> 0 */
    -1, 0, 1, 2,  3, 4, 5, 6,  7, 8, 9,10, 11,12,13,14,
    15,16,17,18, 19,20,21,22, 23,24,25,-1, -1,-1,-1,-1,
    -1,26,27,28, 29,30,31,32, 33,34,35,36, 37,38,39,40,
    41,42,43,44, 45,46,47,48, 49,50,51,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
    -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
];

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b2a_base64(data: &[u8], newline: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4 + 1);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63]);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63]);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63]
        } else {
            PAD
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63]
        } else {
            PAD
        });
    }
    if newline {
        out.push(b'\n');
    }
    out
}

/// The quad machine, ported from RustPython.
fn a2b_base64(b: &[u8], strict: bool) -> Result<Vec<u8>, String> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    if strict && b[0] == PAD {
        return Err(err("Leading padding not allowed"));
    }
    let mut decoded: Vec<u8> = Vec::new();
    let mut quad_pos = 0usize;
    let mut pads = 0usize;
    let mut left_char: u8 = 0;
    let mut padding_started = false;
    for (i, &el) in b.iter().enumerate() {
        if el == PAD {
            padding_started = true;
            pads += 1;
            if quad_pos >= 2 && quad_pos + pads >= 4 {
                if strict && i + 1 < b.len() {
                    return Err(err("Excess data after padding"));
                }
                return Ok(decoded);
            }
            continue;
        }
        let binary_char = BASE64_TABLE[el as usize];
        if binary_char == -1 {
            if strict {
                return Err(err("Only base64 data is allowed"));
            }
            continue;
        }
        if strict && padding_started {
            return Err(err("Discontinuous padding not allowed"));
        }
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left_char = binary_char as u8;
            }
            1 => {
                quad_pos = 2;
                decoded.push((left_char << 2) | (binary_char >> 4) as u8);
                left_char = (binary_char & 0x0f) as u8;
            }
            2 => {
                quad_pos = 3;
                decoded.push((left_char << 4) | (binary_char >> 2) as u8);
                left_char = (binary_char & 0x03) as u8;
            }
            _ => {
                quad_pos = 0;
                decoded.push((left_char << 6) | binary_char as u8);
                left_char = 0;
            }
        }
    }
    match quad_pos {
        0 => Ok(decoded),
        1 => Err(err("Invalid base64-encoded string: number of data characters (1) cannot be 1 more than a multiple of 4")),
        _ => Err(err("Incorrect padding")),
    }
}

/// Reflected CRC-32 (the zlib/PNG polynomial), table built on first use.
fn crc32(data: &[u8], init: u32) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut crc = !init;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// The module's exported names.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const NAMES: &[&str] = &[
        "hexlify",
        "b2a_hex",
        "unhexlify",
        "a2b_hex",
        "b2a_base64",
        "a2b_base64",
        "crc32",
    ];
    let mut out: Vec<(String, Value)> = Vec::new();
    for n in NAMES {
        let v = h.alloc(PyObj::Builtin(format!("binascii.{n}")));
        out.push(((*n).to_string(), v));
    }
    // `binascii.Error` is a ValueError subclass in CPython; `base64` catches it.
    let e = h.alloc(PyObj::Builtin("binascii.Error".to_string()));
    out.push(("Error".to_string(), e));
    let i = h.alloc(PyObj::Builtin("binascii.Incomplete".to_string()));
    out.push(("Incomplete".to_string(), i));
    out
}

/// Dispatch `binascii.<fname>`.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value], kwargs: &[(String, Value)]) -> Option<Result<Value, String>> {
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    Some(match fname {
        "hexlify" | "b2a_hex" => (|| {
            let data = buf(h, args.first().ok_or_else(|| err("missing argument"))?)?;
            let sep = match args.get(1).cloned().or_else(|| kw("sep")) {
                Some(v) => {
                    let s = buf(h, &v)?;
                    if s.len() != 1 {
                        return Err("ValueError: sep must be length 1.".to_string());
                    }
                    Some(s[0])
                }
                None => None,
            };
            let per = args
                .get(2)
                .cloned()
                .or_else(|| kw("bytes_per_sep"))
                .and_then(|v| h.as_int(&v))
                .unwrap_or(1);
            let mut out = Vec::with_capacity(data.len() * 2);
            for (i, b) in data.iter().enumerate() {
                if let (Some(s), true) = (sep, per > 0) {
                    if i != 0 && i % (per as usize) == 0 {
                        out.push(s);
                    }
                }
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0xf));
            }
            Ok(h.alloc(PyObj::Bytes(out)))
        })(),
        "unhexlify" | "a2b_hex" => (|| {
            let data = buf(h, args.first().ok_or_else(|| err("missing argument"))?)?;
            if data.len() % 2 != 0 {
                return Err(err("Odd-length string"));
            }
            let mut out = Vec::with_capacity(data.len() / 2);
            for pair in data.chunks(2) {
                match (unhex_nibble(pair[0]), unhex_nibble(pair[1])) {
                    (Some(a), Some(b)) => out.push((a << 4) | b),
                    _ => return Err(err("Non-hexadecimal digit found")),
                }
            }
            Ok(h.alloc(PyObj::Bytes(out)))
        })(),
        "b2a_base64" => (|| {
            let data = buf(h, args.first().ok_or_else(|| err("missing argument"))?)?;
            let newline = match args.get(1).cloned().or_else(|| kw("newline")) {
                Some(v) => h.truthy(&v),
                None => true,
            };
            Ok(h.alloc(PyObj::Bytes(b2a_base64(&data, newline))))
        })(),
        "a2b_base64" => (|| {
            let data = buf(h, args.first().ok_or_else(|| err("missing argument"))?)?;
            let strict = match args.get(1).cloned().or_else(|| kw("strict_mode")) {
                Some(v) => h.truthy(&v),
                None => false,
            };
            Ok(h.alloc(PyObj::Bytes(a2b_base64(&data, strict)?)))
        })(),
        "crc32" => (|| {
            let data = buf(h, args.first().ok_or_else(|| err("missing argument"))?)?;
            let init = args
                .get(1)
                .and_then(|v| h.as_int(v))
                .unwrap_or(0) as u32;
            Ok(Value::Int(crc32(&data, init) as i64))
        })(),
        _ => return None,
    })
}

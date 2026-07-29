//! The `_struct` C-accelerator module: `pack`, `unpack`, `unpack_from`,
//! `pack_into`, `calcsize`, `iter_unpack`.
//!
//! PORTED from RustPython's `crates/stdlib/src/pystruct.rs` and the
//! `FormatSpec`/`FormatCode` machinery in `crates/vm/src/buffer.rs`
//! (<https://github.com/RustPython/RustPython>, MIT licence). The structure is
//! theirs: an `Endianness` parsed off the leading character, a `FormatType`
//! keyed by the format character, a per-type `FormatInfo { size, align }` that
//! differs between native and standard mode, and a `FormatCode` list carrying
//! each item's repeat count and its preceding alignment padding. Argument
//! marshalling is rewritten against pythonrs's `Value`/`PyHost` object model,
//! which is the only part that could not carry over.
//!
//! Wiring (done by the parent): an `import_module` arm for `"_struct"` calling
//! [`entries`], and a `call_builtin_function` arm routing `_struct.*` to
//! [`call`].

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// `@` native, `=` standard-with-host-order, `<` little, `>`/`!` big.
///
/// Native mode is the only one that pads for alignment and the only one whose
/// sizes follow the C ABI; every other mode uses fixed standard sizes and no
/// padding. That single distinction drives `FormatInfo` below.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Endianness {
    Native,
    Little,
    Big,
}

impl Endianness {
    /// Consume a leading byte-order character if present, defaulting to native.
    fn parse(bytes: &[u8], i: &mut usize) -> (Self, bool) {
        match bytes.first() {
            Some(b'@') => {
                *i += 1;
                (Self::Native, true)
            }
            Some(b'=') => {
                *i += 1;
                (
                    if cfg!(target_endian = "big") {
                        Self::Big
                    } else {
                        Self::Little
                    },
                    false,
                )
            }
            Some(b'<') => {
                *i += 1;
                (Self::Little, false)
            }
            Some(b'>') | Some(b'!') => {
                *i += 1;
                (Self::Big, false)
            }
            _ => (Self::Native, true),
        }
    }
}

/// What a format character means. Values are the characters themselves, as in
/// RustPython, so `try_from` is a plain byte match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FormatType {
    Pad,
    SByte,
    UByte,
    Char,
    Str,
    Pascal,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    SSizeT,
    SizeT,
    LongLong,
    ULongLong,
    Bool,
    Half,
    Float,
    Double,
    VoidP,
}

impl FormatType {
    fn from_byte(c: u8) -> Option<Self> {
        Some(match c {
            b'x' => Self::Pad,
            b'b' => Self::SByte,
            b'B' => Self::UByte,
            b'c' => Self::Char,
            b's' => Self::Str,
            b'p' => Self::Pascal,
            b'h' => Self::Short,
            b'H' => Self::UShort,
            b'i' => Self::Int,
            b'I' => Self::UInt,
            b'l' => Self::Long,
            b'L' => Self::ULong,
            b'n' => Self::SSizeT,
            b'N' => Self::SizeT,
            b'q' => Self::LongLong,
            b'Q' => Self::ULongLong,
            b'?' => Self::Bool,
            b'e' => Self::Half,
            b'f' => Self::Float,
            b'd' => Self::Double,
            b'P' => Self::VoidP,
            _ => return None,
        })
    }

    /// `(size, align)` for this code. Standard mode fixes `l`/`L` at 4 bytes and
    /// aligns nothing; native mode follows the host C ABI, where they are 8 on
    /// LP64. `n`/`N`/`P` exist only in native mode (CPython rejects them
    /// otherwise), which the parser enforces.
    fn info(self, native: bool) -> (usize, usize) {
        let (std_size, native_size, native_align) = match self {
            Self::Pad
            | Self::SByte
            | Self::UByte
            | Self::Char
            | Self::Str
            | Self::Pascal
            | Self::Bool => (1usize, 1usize, 1usize),
            Self::Short | Self::UShort => (2, 2, 2),
            Self::Int | Self::UInt => (4, 4, 4),
            Self::Long | Self::ULong => (4, 8, 8),
            Self::SSizeT | Self::SizeT | Self::VoidP => (8, 8, 8),
            Self::LongLong | Self::ULongLong => (8, 8, 8),
            Self::Half => (2, 2, 2),
            Self::Float => (4, 4, 4),
            Self::Double => (8, 8, 8),
        };
        if native {
            (native_size, native_align)
        } else {
            (std_size, 0)
        }
    }

    /// How many Python arguments one code consumes: `x` none, `s`/`p` one
    /// (the whole repeat is a single bytes object), everything else `repeat`.
    fn arg_count(self, repeat: usize) -> usize {
        match self {
            Self::Pad => 0,
            Self::Str | Self::Pascal => 1,
            _ => repeat,
        }
    }
}

struct FormatCode {
    repeat: usize,
    code: FormatType,
    size: usize,
    pre_padding: usize,
}

struct FormatSpec {
    endian: Endianness,
    codes: Vec<FormatCode>,
    size: usize,
    arg_count: usize,
}

/// Bytes of padding needed before an item of alignment `align` at `offset`.
fn compensate_alignment(offset: usize, align: usize) -> usize {
    if align != 0 && offset != 0 {
        // `a % b == a & (b-1)` for power-of-two `b`.
        (align - 1) - ((offset - 1) & (align - 1))
    } else {
        0
    }
}

fn err(msg: &str) -> String {
    format!("struct.error: {msg}")
}

impl FormatSpec {
    fn parse(fmt: &[u8]) -> Result<Self, String> {
        let mut i = 0usize;
        let (endian, native) = Endianness::parse(fmt, &mut i);
        let mut offset = 0usize;
        let mut arg_count = 0usize;
        let mut codes = Vec::new();
        while i < fmt.len() {
            // Whitespace is allowed between items.
            while matches!(fmt.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                i += 1;
            }
            if i >= fmt.len() {
                break;
            }
            let mut repeat: usize = 1;
            if fmt[i].is_ascii_digit() {
                repeat = 0;
                while let Some(c) = fmt.get(i).filter(|c| c.is_ascii_digit()) {
                    repeat = repeat
                        .checked_mul(10)
                        .and_then(|r| r.checked_add((c - b'0') as usize))
                        .ok_or_else(|| err("total struct size too long"))?;
                    i += 1;
                }
                if i >= fmt.len() {
                    return Err(err("repeat count given without format specifier"));
                }
            }
            let c = fmt[i];
            i += 1;
            let code = FormatType::from_byte(c)
                .filter(|t| {
                    // `n`/`N`/`P` are native-mode only, exactly as CPython.
                    !matches!(
                        t,
                        FormatType::SSizeT | FormatType::SizeT | FormatType::VoidP
                    ) || native
                })
                .ok_or_else(|| err("bad char in struct format"))?;
            let (size, align) = code.info(native);
            let padding = compensate_alignment(offset, align);
            offset += padding;
            arg_count += code.arg_count(repeat);
            let item_bytes = match code {
                // `s`/`p` take `repeat` BYTES, not `repeat` items.
                FormatType::Str | FormatType::Pascal => repeat,
                _ => size
                    .checked_mul(repeat)
                    .ok_or_else(|| err("total struct size too long"))?,
            };
            codes.push(FormatCode {
                repeat,
                code,
                size,
                pre_padding: padding,
            });
            offset = offset
                .checked_add(item_bytes)
                .ok_or_else(|| err("total struct size too long"))?;
        }
        Ok(Self {
            endian,
            codes,
            size: offset,
            arg_count,
        })
    }

    fn write_int(&self, out: &mut Vec<u8>, v: i128, size: usize) {
        let le = self.endian != Endianness::Big;
        let bytes = v.to_le_bytes();
        if le {
            out.extend_from_slice(&bytes[..size]);
        } else {
            let mut b = bytes[..size].to_vec();
            b.reverse();
            out.extend_from_slice(&b);
        }
    }

    fn read_int(&self, data: &[u8], signed: bool) -> i128 {
        let mut buf = data.to_vec();
        if self.endian == Endianness::Big {
            buf.reverse();
        }
        let mut wide = [0u8; 16];
        wide[..buf.len()].copy_from_slice(&buf);
        if signed && buf.last().is_some_and(|b| b & 0x80 != 0) {
            for byte in wide.iter_mut().skip(buf.len()) {
                *byte = 0xff;
            }
        }
        i128::from_le_bytes(wide)
    }

    fn pack(&self, h: &mut PyHost, args: &[Value]) -> Result<Vec<u8>, String> {
        if args.len() != self.arg_count {
            return Err(err(&format!(
                "pack expected {} items for packing (got {})",
                self.arg_count,
                args.len()
            )));
        }
        let mut out: Vec<u8> = Vec::with_capacity(self.size);
        let mut arg = 0usize;
        for code in &self.codes {
            out.extend(std::iter::repeat(0u8).take(code.pre_padding));
            match code.code {
                FormatType::Pad => out.extend(std::iter::repeat(0u8).take(code.repeat)),
                FormatType::Str | FormatType::Pascal => {
                    let v = &args[arg];
                    arg += 1;
                    let mut b = match h.get(v) {
                        Some(PyObj::Bytes(b)) => b.clone(),
                        Some(PyObj::Bytearray(b)) => b.clone(),
                        _ => return Err(err("argument for 's' must be a bytes object")),
                    };
                    if matches!(code.code, FormatType::Pascal) {
                        // `p`: a length byte, then the data, truncated to fit.
                        let cap = code.repeat.saturating_sub(1);
                        b.truncate(cap);
                        out.push(b.len() as u8);
                        out.extend_from_slice(&b);
                        out.extend(std::iter::repeat(0u8).take(cap - b.len()));
                    } else {
                        b.truncate(code.repeat);
                        let pad = code.repeat - b.len();
                        out.extend_from_slice(&b);
                        out.extend(std::iter::repeat(0u8).take(pad));
                    }
                }
                _ => {
                    for _ in 0..code.repeat {
                        let v = &args[arg];
                        arg += 1;
                        self.pack_one(h, code, v, &mut out)?;
                    }
                }
            }
        }
        Ok(out)
    }

    fn pack_one(
        &self,
        h: &mut PyHost,
        code: &FormatCode,
        v: &Value,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        match code.code {
            FormatType::Char => {
                let b = match h.get(v) {
                    Some(PyObj::Bytes(b)) if b.len() == 1 => b[0],
                    _ => return Err(err("char format requires a bytes object of length 1")),
                };
                out.push(b);
            }
            FormatType::Bool => {
                let t = h.truthy(v);
                out.push(u8::from(t));
            }
            FormatType::Half | FormatType::Float | FormatType::Double => {
                let f = h
                    .num_val(v)
                    .ok_or_else(|| err("required argument is not a float"))?;
                let bytes: Vec<u8> = match code.code {
                    FormatType::Float => (f as f32).to_le_bytes().to_vec(),
                    FormatType::Double => f.to_le_bytes().to_vec(),
                    // IEEE 754 binary16, built by hand — no half-float in std.
                    _ => f16_to_le_bytes(f).to_vec(),
                };
                if self.endian == Endianness::Big {
                    out.extend(bytes.iter().rev());
                } else {
                    out.extend_from_slice(&bytes);
                }
            }
            _ => {
                let n = h
                    .as_int(v)
                    .map(i128::from)
                    .or_else(|| h.big_val(v).and_then(|b| bigint_to_i128(&b)))
                    .ok_or_else(|| err("required argument is not an integer"))?;
                let signed = matches!(
                    code.code,
                    FormatType::SByte
                        | FormatType::Short
                        | FormatType::Int
                        | FormatType::Long
                        | FormatType::LongLong
                        | FormatType::SSizeT
                );
                let bits = code.size * 8;
                let (lo, hi) = if signed {
                    (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
                } else {
                    (
                        0,
                        if bits >= 128 {
                            i128::MAX
                        } else {
                            (1i128 << bits) - 1
                        },
                    )
                };
                if n < lo || n > hi {
                    return Err(err(&format!(
                        "'{}' format requires {lo} <= number <= {hi}",
                        code_char(code.code)
                    )));
                }
                self.write_int(out, n, code.size);
            }
        }
        Ok(())
    }

    fn unpack(&self, h: &mut PyHost, data: &[u8]) -> Result<Vec<Value>, String> {
        if data.len() != self.size {
            return Err(err(&format!(
                "unpack requires a buffer of {} bytes",
                self.size
            )));
        }
        let mut out = Vec::with_capacity(self.arg_count);
        let mut pos = 0usize;
        for code in &self.codes {
            pos += code.pre_padding;
            match code.code {
                FormatType::Pad => pos += code.repeat,
                FormatType::Str => {
                    out.push(h.alloc(PyObj::Bytes(data[pos..pos + code.repeat].to_vec())));
                    pos += code.repeat;
                }
                FormatType::Pascal => {
                    let n = (data[pos] as usize).min(code.repeat.saturating_sub(1));
                    out.push(h.alloc(PyObj::Bytes(data[pos + 1..pos + 1 + n].to_vec())));
                    pos += code.repeat;
                }
                _ => {
                    for _ in 0..code.repeat {
                        let chunk = &data[pos..pos + code.size];
                        out.push(self.unpack_one(h, code.code, chunk));
                        pos += code.size;
                    }
                }
            }
        }
        Ok(out)
    }

    fn unpack_one(&self, h: &mut PyHost, code: FormatType, chunk: &[u8]) -> Value {
        match code {
            FormatType::Char => h.alloc(PyObj::Bytes(chunk.to_vec())),
            FormatType::Bool => Value::Bool(chunk[0] != 0),
            FormatType::Half | FormatType::Float | FormatType::Double => {
                let mut b = chunk.to_vec();
                if self.endian == Endianness::Big {
                    b.reverse();
                }
                let f = match code {
                    FormatType::Float => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
                    FormatType::Double => {
                        f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                    }
                    _ => f16_from_le_bytes([b[0], b[1]]),
                };
                Value::Float(f)
            }
            _ => {
                let signed = matches!(
                    code,
                    FormatType::SByte
                        | FormatType::Short
                        | FormatType::Int
                        | FormatType::Long
                        | FormatType::LongLong
                        | FormatType::SSizeT
                );
                let n = self.read_int(chunk, signed);
                match i64::try_from(n) {
                    Ok(v) => Value::Int(v),
                    Err(_) => h.norm_big(num_bigint::BigInt::from(n)),
                }
            }
        }
    }
}

fn code_char(c: FormatType) -> char {
    match c {
        FormatType::Pad => 'x',
        FormatType::SByte => 'b',
        FormatType::UByte => 'B',
        FormatType::Char => 'c',
        FormatType::Str => 's',
        FormatType::Pascal => 'p',
        FormatType::Short => 'h',
        FormatType::UShort => 'H',
        FormatType::Int => 'i',
        FormatType::UInt => 'I',
        FormatType::Long => 'l',
        FormatType::ULong => 'L',
        FormatType::SSizeT => 'n',
        FormatType::SizeT => 'N',
        FormatType::LongLong => 'q',
        FormatType::ULongLong => 'Q',
        FormatType::Bool => '?',
        FormatType::Half => 'e',
        FormatType::Float => 'f',
        FormatType::Double => 'd',
        FormatType::VoidP => 'P',
    }
}

fn bigint_to_i128(b: &num_bigint::BigInt) -> Option<i128> {
    use num_traits::ToPrimitive;
    b.to_i128()
}

/// IEEE 754 binary16 encode/decode. RustPython uses the `half` crate; doing it
/// inline keeps the dependency list unchanged.
fn f16_to_le_bytes(f: f64) -> [u8; 2] {
    let bits = (f as f32).to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = (bits & 0x007f_ffff) >> 13;
    let half = if exp <= 0 {
        sign
    } else if exp >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | mant as u16
    };
    half.to_le_bytes()
}

fn f16_from_le_bytes(b: [u8; 2]) -> f64 {
    let h = u16::from_le_bytes(b);
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x03ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: renormalize into a float32.
            let mut e = -1i32;
            let mut m = mant;
            while m & 0x0400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (((e + 127 - 15 + 1) as u32) << 23) | ((m & 0x03ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | (((exp - 15 + 127) as u32) << 23) | (mant << 13)
    };
    f32::from_bits(bits) as f64
}

/// The module's exported names.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const NAMES: &[&str] = &[
        "pack",
        "unpack",
        "pack_into",
        "unpack_from",
        "calcsize",
        "iter_unpack",
        "_clearcache",
        "Struct",
    ];
    let mut out: Vec<(String, Value)> = Vec::new();
    for n in NAMES {
        let v = h.alloc(PyObj::Builtin(format!("_struct.{n}")));
        out.push(((*n).to_string(), v));
    }
    let e = h.alloc(PyObj::Builtin("struct.error".to_string()));
    out.push(("error".to_string(), e));
    let d = h.new_str("Functions to convert between Python values and C structs.".to_string());
    out.push(("__doc__".to_string(), d));
    out
}

fn fmt_bytes(h: &PyHost, v: &Value) -> Result<Vec<u8>, String> {
    match h.get(v) {
        Some(PyObj::Str(s)) => Ok(s.as_bytes().to_vec()),
        Some(PyObj::Bytes(b)) => Ok(b.clone()),
        _ => match v {
            Value::Str(s) => Ok(s.as_bytes().to_vec()),
            _ => Err(host::type_error("Struct() argument 1 must be str or bytes")),
        },
    }
}

fn buffer_bytes(h: &PyHost, v: &Value) -> Result<Vec<u8>, String> {
    match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(b.clone()),
        _ => Err(host::type_error("a bytes-like object is required")),
    }
}

/// Methods on a compiled `struct.Struct`. `None` means `recv` is not one, so the
/// caller carries on with its normal dispatch.
///
/// The format is re-parsed per call rather than cached in the object; `Struct`
/// exists for the API (`base64.b85encode` builds one at import time), and the
/// parse is linear in a format string that is always tiny.
pub fn struct_method(
    h: &mut PyHost,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let fmt = match h.get(recv) {
        Some(PyObj::StructFmt(f)) => f.clone(),
        _ => return None,
    };
    let mut all: Vec<Value> = vec![h.new_str(fmt)];
    all.extend_from_slice(args);
    Some(match name {
        "pack" | "unpack" | "unpack_from" | "iter_unpack" => match call(h, name, &all) {
            Some(r) => r,
            None => Err(host::type_error(&format!("Struct.{name}"))),
        },
        // `pack_into(buffer, offset, *values)` — the module function takes the
        // format first, so the already-prepended format lines the arguments up.
        "pack_into" => match call(h, "pack_into", &all) {
            Some(r) => r,
            None => Err(host::type_error("Struct.pack_into")),
        },
        _ => Err(host::type_error(&format!(
            "'Struct' object has no attribute '{name}'"
        ))),
    })
}

/// `Struct.size` / `.format` — attributes, not methods.
pub fn struct_attr_of(h: &mut PyHost, recv: &Value, name: &str) -> Option<Result<Value, String>> {
    let fmt = match h.get(recv) {
        Some(PyObj::StructFmt(f)) => f.clone(),
        _ => return None,
    };
    Some(match name {
        "size" => FormatSpec::parse(fmt.as_bytes()).map(|s| Value::Int(s.size as i64)),
        "format" => Ok(h.new_str(fmt)),
        _ => return None,
    })
}

/// Dispatch `_struct.<fname>`.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    Some(match fname {
        // `Struct(fmt)` — validate the format now, as CPython does, so a bad
        // format raises at construction rather than at first use.
        "Struct" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            FormatSpec::parse(&fmt)?;
            let s = String::from_utf8_lossy(&fmt).into_owned();
            Ok(h.alloc(PyObj::StructFmt(s)))
        })(),
        "calcsize" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            Ok(Value::Int(FormatSpec::parse(&fmt)?.size as i64))
        })(),
        "pack" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            let spec = FormatSpec::parse(&fmt)?;
            let packed = spec.pack(h, &args[1..])?;
            Ok(h.alloc(PyObj::Bytes(packed)))
        })(),
        "unpack" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            let spec = FormatSpec::parse(&fmt)?;
            let data = buffer_bytes(h, args.get(1).ok_or_else(|| err("missing buffer"))?)?;
            let vals = spec.unpack(h, &data)?;
            Ok(h.new_tuple(vals))
        })(),
        "unpack_from" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            let spec = FormatSpec::parse(&fmt)?;
            let data = buffer_bytes(h, args.get(1).ok_or_else(|| err("missing buffer"))?)?;
            let off = args.get(2).and_then(|v| h.as_int(v)).unwrap_or(0);
            let start = if off < 0 {
                data.len().saturating_sub((-off) as usize)
            } else {
                off as usize
            };
            if start + spec.size > data.len() {
                return Err(err(&format!(
                    "unpack_from requires a buffer of at least {} bytes",
                    start + spec.size
                )));
            }
            let vals = spec.unpack(h, &data[start..start + spec.size])?;
            Ok(h.new_tuple(vals))
        })(),
        "pack_into" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            let spec = FormatSpec::parse(&fmt)?;
            let target = args.get(1).cloned().ok_or_else(|| err("missing buffer"))?;
            let off = args.get(2).and_then(|v| h.as_int(v)).unwrap_or(0).max(0) as usize;
            let packed = spec.pack(h, &args[3..])?;
            match h.get_mut(&target) {
                Some(PyObj::Bytearray(b)) => {
                    if off + packed.len() > b.len() {
                        return Err(err(&format!(
                            "pack_into requires a buffer of at least {} bytes",
                            off + packed.len()
                        )));
                    }
                    b[off..off + packed.len()].copy_from_slice(&packed);
                    Ok(Value::Undef)
                }
                _ => Err(host::type_error(
                    "argument must be read-write bytes-like object",
                )),
            }
        })(),
        "iter_unpack" => (|| {
            let fmt = fmt_bytes(h, args.first().ok_or_else(|| err("missing format"))?)?;
            let spec = FormatSpec::parse(&fmt)?;
            if spec.size == 0 {
                return Err(err("cannot iteratively unpack with a struct of length 0"));
            }
            let data = buffer_bytes(h, args.get(1).ok_or_else(|| err("missing buffer"))?)?;
            if data.len() % spec.size != 0 {
                return Err(err(
                    "iterative unpacking requires a buffer of a multiple of the struct size",
                ));
            }
            // Eager: the chunks are materialised as a list of tuples, which
            // iterates identically for every documented use.
            let mut rows = Vec::with_capacity(data.len() / spec.size);
            for chunk in data.chunks(spec.size) {
                let vals = spec.unpack(h, chunk)?;
                rows.push(h.new_tuple(vals));
            }
            let list = h.new_list(rows);
            h.make_iter(&list)
        })(),
        "_clearcache" => Ok(Value::Undef),
        _ => return None,
    })
}

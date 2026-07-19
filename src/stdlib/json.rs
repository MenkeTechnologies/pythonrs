//! The `json` standard-library module: `dumps` (serialize) and `loads` (parse).
//!
//! Both directions are hand-written rather than delegated to `serde_json` so the
//! object model stays exactly CPython's: `loads` preserves object key insertion
//! order (serde_json without the `preserve_order` feature would sort keys), and
//! integers stay arbitrary-precision (`PyObj::BigInt`) instead of collapsing to
//! f64. `dumps` matches CPython's default spacing (`", "` / `": "`), the
//! `indent=` block form, and `ensure_ascii=True` escaping of non-ASCII code
//! points.
//!
//! Wiring (done by the parent): an `import_module` arm for `"json"` that calls
//! [`entries`], and a `call_builtin_function` arm that routes `json.*` names to
//! [`call`].

use crate::host::{type_error, PKey, PyHost, PyObj};
use fusevm::Value;
use indexmap::IndexMap;

/// The module namespace: `("attr", value)` pairs. Only two callables, both first
/// class `PyObj::Builtin` handles the VM invokes back through [`call`].
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    vec![
        ("dumps".into(), h.alloc(PyObj::Builtin("json.dumps".into()))),
        ("loads".into(), h.alloc(PyObj::Builtin("json.loads".into()))),
    ]
}

/// Dispatch a `json.*` builtin. Returns `None` for names this module does not own
/// so the parent dispatcher can keep looking.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "dumps" => Some(dumps(h, args)),
        "loads" => Some(loads(h, args)),
        _ => None,
    }
}

// ── dumps ────────────────────────────────────────────────────────────────────

fn dumps(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let obj = match args.first() {
        Some(v) => v.clone(),
        None => {
            return Err(type_error(
                "dumps() missing 1 required positional argument: 'obj'",
            ))
        }
    };
    // Positional `indent` (the fixed `call` signature carries no kwargs, so a
    // keyword `indent=` only reaches here if the parent folds kwargs into args).
    let indent: Option<String> = match args.get(1) {
        None | Some(Value::Undef) => None,
        Some(Value::Int(n)) => Some(" ".repeat((*n).max(0) as usize)),
        Some(Value::Bool(_)) => None,
        Some(v) => h.as_str(v),
    };
    let mut out = String::new();
    ser(h, &obj, indent.as_deref(), 0, &mut out)?;
    Ok(h.new_str(out))
}

/// Serialize `v` into `out`. `indent` is the per-level unit string when in block
/// mode, or `None` for compact `", "` / `": "` mode.
fn ser(
    h: &PyHost,
    v: &Value,
    indent: Option<&str>,
    level: usize,
    out: &mut String,
) -> Result<(), String> {
    match v {
        Value::Undef => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) => out.push_str(&ser_float(*f)),
        Value::Str(s) => encode_str(s, out),
        Value::Obj(_) => match h.get(v) {
            Some(PyObj::Str(s)) => encode_str(s, out),
            Some(PyObj::BigInt(b)) => out.push_str(&b.to_string()),
            Some(PyObj::List(items)) | Some(PyObj::Tuple(items)) => {
                ser_array(h, &items.clone(), indent, level, out)?
            }
            Some(PyObj::Dict(map)) => {
                let pairs: Vec<(Value, Value)> = map.values().cloned().collect();
                ser_object(h, &pairs, indent, level, out)?
            }
            _ => {
                return Err(type_error(&format!(
                    "Object of type {} is not JSON serializable",
                    h.type_name(v)
                )))
            }
        },
        _ => {
            return Err(type_error(&format!(
                "Object of type {} is not JSON serializable",
                h.type_name(v)
            )))
        }
    }
    Ok(())
}

fn ser_array(
    h: &PyHost,
    items: &[Value],
    indent: Option<&str>,
    level: usize,
    out: &mut String,
) -> Result<(), String> {
    if items.is_empty() {
        out.push_str("[]");
        return Ok(());
    }
    out.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
            if indent.is_none() {
                out.push(' ');
            }
        }
        newline_indent(indent, level + 1, out);
        ser(h, it, indent, level + 1, out)?;
    }
    newline_indent(indent, level, out);
    out.push(']');
    Ok(())
}

fn ser_object(
    h: &PyHost,
    pairs: &[(Value, Value)],
    indent: Option<&str>,
    level: usize,
    out: &mut String,
) -> Result<(), String> {
    if pairs.is_empty() {
        out.push_str("{}");
        return Ok(());
    }
    out.push('{');
    for (i, (k, val)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
            if indent.is_none() {
                out.push(' ');
            }
        }
        newline_indent(indent, level + 1, out);
        let key = coerce_key(h, k)?;
        encode_str(&key, out);
        out.push_str(": ");
        ser(h, val, indent, level + 1, out)?;
    }
    newline_indent(indent, level, out);
    out.push('}');
    Ok(())
}

/// In block mode, emit a newline + `indent * level`. No-op in compact mode.
fn newline_indent(indent: Option<&str>, level: usize, out: &mut String) {
    if let Some(unit) = indent {
        out.push('\n');
        for _ in 0..level {
            out.push_str(unit);
        }
    }
}

/// CPython coerces scalar dict keys to strings; non-scalar keys are an error.
fn coerce_key(h: &PyHost, k: &Value) -> Result<String, String> {
    match k {
        Value::Str(s) => Ok((**s).clone()),
        Value::Bool(b) => Ok(if *b { "true".into() } else { "false".into() }),
        Value::Int(n) => Ok(n.to_string()),
        Value::Float(f) => Ok(ser_float(*f)),
        Value::Undef => Ok("null".into()),
        Value::Obj(_) => match h.get(k) {
            Some(PyObj::Str(s)) => Ok(s.clone()),
            Some(PyObj::BigInt(b)) => Ok(b.to_string()),
            _ => Err(type_error(&format!(
                "keys must be str, int, float, bool or None, not {}",
                h.type_name(k)
            ))),
        },
        _ => Err(type_error("keys must be str, int, float, bool or None")),
    }
}

/// JSON number form of a float: `Infinity` / `-Infinity` / `NaN` per CPython's
/// default `allow_nan=True`, otherwise Python's `repr(float)`.
fn ser_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f < 0.0 {
            "-Infinity".into()
        } else {
            "Infinity".into()
        }
    } else {
        crate::host::fmt_float(f)
    }
}

/// Encode a string as a JSON string literal with `ensure_ascii=True` (non-ASCII
/// escaped as `\uXXXX`, astral code points as surrogate pairs).
fn encode_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let u = c as u32;
                if u > 0xFFFF {
                    let v = u - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                } else {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
}

// ── loads ────────────────────────────────────────────────────────────────────

fn loads(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let s = match args.first() {
        Some(Value::Str(s)) => (**s).clone(),
        Some(v) => match h.as_str(v) {
            Some(s) => s,
            None => {
                return Err(type_error(&format!(
                    "the JSON object must be str, not {}",
                    h.type_name(v)
                )))
            }
        },
        None => {
            return Err(type_error(
                "loads() missing 1 required positional argument: 's'",
            ))
        }
    };
    let chars: Vec<char> = s.chars().collect();
    let mut p = Parser {
        chars: &chars,
        i: 0,
    };
    p.skip_ws();
    let v = p.parse_value(h)?;
    p.skip_ws();
    if p.i != p.chars.len() {
        return Err(json_err("Extra data", &p));
    }
    Ok(v)
}

struct Parser<'a> {
    chars: &'a [char],
    i: usize,
}

fn json_err(msg: &str, p: &Parser) -> String {
    format!("ValueError: {msg}: char {}", p.i)
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, h: &mut PyHost) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(h),
            Some('[') => self.parse_array(h),
            Some('"') => {
                let s = self.parse_string()?;
                Ok(h.new_str(s))
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(h),
            Some('t') => self.parse_literal("true", Value::Bool(true)),
            Some('f') => self.parse_literal("false", Value::Bool(false)),
            Some('n') => self.parse_literal("null", Value::Undef),
            Some('N') => self.parse_literal("NaN", Value::Float(f64::NAN)),
            Some('I') => self.parse_literal("Infinity", Value::Float(f64::INFINITY)),
            _ => Err(json_err("Expecting value", self)),
        }
    }

    fn parse_literal(&mut self, word: &str, val: Value) -> Result<Value, String> {
        for wc in word.chars() {
            if self.peek() == Some(wc) {
                self.i += 1;
            } else {
                return Err(json_err("Expecting value", self));
            }
        }
        Ok(val)
    }

    fn parse_object(&mut self, h: &mut PyHost) -> Result<Value, String> {
        self.i += 1; // '{'
        let mut map: IndexMap<PKey, (Value, Value)> = IndexMap::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.i += 1;
            return Ok(h.new_dict(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err(json_err(
                    "Expecting property name enclosed in double quotes",
                    self,
                ));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(json_err("Expecting ':' delimiter", self));
            }
            self.i += 1;
            let val = self.parse_value(h)?;
            let kv = h.new_str(key.clone());
            map.insert(PKey::Str(key), (kv, val));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                }
                Some('}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(json_err("Expecting ',' delimiter", self)),
            }
        }
        Ok(h.new_dict(map))
    }

    fn parse_array(&mut self, h: &mut PyHost) -> Result<Value, String> {
        self.i += 1; // '['
        let mut items: Vec<Value> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.i += 1;
            return Ok(h.new_list(items));
        }
        loop {
            let val = self.parse_value(h)?;
            items.push(val);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                }
                Some(']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(json_err("Expecting ',' delimiter", self)),
            }
        }
        Ok(h.new_list(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.i += 1; // opening '"'
        let mut s = String::new();
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err(json_err("Unterminated string", self)),
            };
            self.i += 1;
            match c {
                '"' => break,
                '\\' => {
                    let e = match self.peek() {
                        Some(e) => e,
                        None => return Err(json_err("Unterminated string", self)),
                    };
                    self.i += 1;
                    match e {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'b' => s.push('\u{08}'),
                        'f' => s.push('\u{0c}'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate — expect a following \uXXXX low.
                                if self.peek() == Some('\\') {
                                    self.i += 1;
                                    if self.peek() == Some('u') {
                                        self.i += 1;
                                        let lo = self.parse_hex4()?;
                                        if (0xDC00..=0xDFFF).contains(&lo) {
                                            let combined =
                                                0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            s.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                                            continue;
                                        }
                                        s.push('\u{fffd}');
                                        s.push(char::from_u32(lo).unwrap_or('\u{fffd}'));
                                        continue;
                                    }
                                }
                                s.push('\u{fffd}');
                            } else {
                                s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return Err(json_err("Invalid \\escape", self)),
                    }
                }
                c => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut val = 0u32;
        for _ in 0..4 {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err(json_err("Invalid \\uXXXX escape", self)),
            };
            let d = c
                .to_digit(16)
                .ok_or_else(|| json_err("Invalid \\uXXXX escape", self))?;
            val = val * 16 + d;
            self.i += 1;
        }
        Ok(val)
    }

    fn parse_number(&mut self, h: &mut PyHost) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
            // -Infinity
            if self.peek() == Some('I') {
                self.parse_literal("Infinity", Value::Undef)?;
                return Ok(Value::Float(f64::NEG_INFINITY));
            }
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => self.i += 1,
                '.' | 'e' | 'E' | '+' | '-' => {
                    is_float = true;
                    self.i += 1;
                }
                _ => break,
            }
        }
        let text: String = self.chars[start..self.i].iter().collect();
        if is_float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| json_err("Expecting value", self))
        } else if let Ok(n) = text.parse::<i64>() {
            Ok(Value::Int(n))
        } else {
            // Overflows i64 — keep arbitrary precision like CPython.
            use std::str::FromStr;
            match num_bigint::BigInt::from_str(&text) {
                Ok(b) => Ok(h.alloc(PyObj::BigInt(b))),
                Err(_) => Err(json_err("Expecting value", self)),
            }
        }
    }
}

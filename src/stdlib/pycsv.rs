//! `_csv` — the CSV reader/writer `csv.py` is built on.
//!
//! `csv.py` supplies the dialect classes, `DictReader`/`DictWriter` and the
//! `Sniffer`; the parsing and quoting themselves live here, as they do in
//! CPython. The grammar is not RFC 4180 exactly — it is what CPython's
//! `Modules/_csv.c` state machine accepts, which is deliberately more permissive
//! (a bare quote mid-field, a field that starts unquoted and later quotes, a
//! `\r` that is not followed by `\n`), and code depends on that tolerance when
//! reading files other tools produced.
//!
//! A `Dialect` is validated at construction, exactly as `csv.Dialect._validate`
//! expects: it builds `_csv.Dialect(self)` purely to have the parameters checked
//! and turns any `TypeError` into a `csv.Error`.

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// `csv.QUOTE_*`. The values are part of the module's contract — code writes
/// `quoting=csv.QUOTE_ALL` and compares against the integers.
pub const QUOTE_MINIMAL: i64 = 0;
pub const QUOTE_ALL: i64 = 1;
pub const QUOTE_NONNUMERIC: i64 = 2;
pub const QUOTE_NONE: i64 = 3;
pub const QUOTE_STRINGS: i64 = 4;
pub const QUOTE_NOTNULL: i64 = 5;

/// The parameters that decide how a row is written and read.
#[derive(Clone, Debug, PartialEq)]
pub struct Dialect {
    pub delimiter: char,
    pub quotechar: Option<char>,
    pub escapechar: Option<char>,
    pub doublequote: bool,
    pub skipinitialspace: bool,
    pub lineterminator: String,
    pub quoting: i64,
    pub strict: bool,
}

impl Default for Dialect {
    /// `csv.excel`, which is what every entry point falls back to.
    fn default() -> Self {
        Dialect {
            delimiter: ',',
            quotechar: Some('"'),
            escapechar: None,
            doublequote: true,
            skipinitialspace: false,
            lineterminator: "\r\n".to_string(),
            quoting: QUOTE_MINIMAL,
            strict: false,
        }
    }
}

impl Dialect {
    /// Read the dialect parameters off `src` — a dialect NAME, a dialect class or
    /// instance, or nothing — then apply any explicit keyword overrides, which
    /// take precedence as CPython's do.
    pub fn resolve_public(
        h: &mut PyHost,
        src: Option<&Value>,
        kwargs: &[(String, Value)],
    ) -> Result<Dialect, String> {
        Dialect::resolve(h, src, kwargs)
    }

    fn resolve(
        h: &mut PyHost,
        src: Option<&Value>,
        kwargs: &[(String, Value)],
    ) -> Result<Dialect, String> {
        let mut d = Dialect::default();
        if let Some(v) = src {
            if let Some(name) = h.as_str(v) {
                d = registry_get(&name)
                    .ok_or_else(|| format!("_csv.Error: unknown dialect '{name}'"))?;
            } else if !matches!(v, Value::Undef) {
                d.read_from(h, v)?;
            }
        }
        for (k, v) in kwargs {
            d.set_field(h, k, v)?;
        }
        d.validate()?;
        Ok(d)
    }

    /// Pull each parameter off a dialect object, ignoring the ones it leaves
    /// `None` — `csv.Dialect` declares every attribute as a `None` placeholder
    /// and subclasses fill in only what they change.
    fn read_from(&mut self, h: &mut PyHost, obj: &Value) -> Result<(), String> {
        for field in [
            "delimiter",
            "quotechar",
            "escapechar",
            "doublequote",
            "skipinitialspace",
            "lineterminator",
            "quoting",
            "strict",
        ] {
            if let Ok(v) = h.get_attr(obj, field) {
                if !matches!(v, Value::Undef) {
                    self.set_field(h, field, &v)?;
                }
            }
        }
        Ok(())
    }

    fn set_field(&mut self, h: &PyHost, name: &str, v: &Value) -> Result<(), String> {
        let one_char = |v: &Value, what: &str| -> Result<char, String> {
            let s = h
                .as_str(v)
                .ok_or_else(|| host::type_error(&format!("\"{what}\" must be string")))?;
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(host::type_error(&format!(
                    "\"{what}\" must be a 1-character string"
                ))),
            }
        };
        match name {
            "delimiter" => self.delimiter = one_char(v, "delimiter")?,
            "quotechar" => {
                self.quotechar = if matches!(v, Value::Undef) {
                    None
                } else {
                    Some(one_char(v, "quotechar")?)
                }
            }
            "escapechar" => {
                self.escapechar = if matches!(v, Value::Undef) {
                    None
                } else {
                    Some(one_char(v, "escapechar")?)
                }
            }
            "doublequote" => self.doublequote = h.truthy(v),
            "skipinitialspace" => self.skipinitialspace = h.truthy(v),
            "strict" => self.strict = h.truthy(v),
            "lineterminator" => {
                self.lineterminator = h
                    .as_str(v)
                    .ok_or_else(|| host::type_error("\"lineterminator\" must be a string"))?
            }
            "quoting" => {
                self.quoting = h
                    .as_int(v)
                    .ok_or_else(|| host::type_error("\"quoting\" must be an integer"))?
            }
            // An unknown keyword is an error, as it is in CPython — a typo in a
            // dialect parameter would otherwise be silently ignored.
            _ => return Err(host::type_error(&format!("'{name}' is an invalid keyword argument"))),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.quoting != QUOTE_NONE && self.quotechar.is_none() {
            return Err(host::type_error(
                "quotechar must be set if quoting enabled",
            ));
        }
        if !(QUOTE_MINIMAL..=QUOTE_NOTNULL).contains(&self.quoting) {
            return Err(host::type_error("bad \"quoting\" value"));
        }
        Ok(())
    }

    /// Whether a written field needs quoting under this dialect.
    fn needs_quotes(&self, field: &str, was_string: bool, was_none: bool) -> bool {
        match self.quoting {
            QUOTE_ALL => true,
            QUOTE_NONNUMERIC => was_string || was_none,
            QUOTE_STRINGS => was_string,
            QUOTE_NOTNULL => !was_none,
            QUOTE_NONE => false,
            // QUOTE_MINIMAL: only when the text would otherwise be ambiguous.
            // An embedded quote forces quoting only when it would have to be
            // DOUBLED; with `doublequote=False` and an `escapechar` the quote is
            // escaped in place and the field stays bare, as CPython writes it.
            _ => {
                field.contains(self.delimiter)
                    || field.contains('\r')
                    || field.contains('\n')
                    || self.lineterminator.chars().any(|c| field.contains(c))
                    || (self.doublequote
                        && self.quotechar.is_some_and(|q| field.contains(q)))
            }
        }
    }
}

// ── the dialect registry ─────────────────────────────────────────────────────

thread_local! {
    static DIALECTS: std::cell::RefCell<Vec<(String, Dialect)>> =
        std::cell::RefCell::new(builtin_dialects());
}

/// The three dialects `csv` registers at import.
fn builtin_dialects() -> Vec<(String, Dialect)> {
    let excel = Dialect::default();
    let excel_tab = Dialect {
        delimiter: '\t',
        ..Dialect::default()
    };
    let unix = Dialect {
        lineterminator: "\n".to_string(),
        quoting: QUOTE_ALL,
        ..Dialect::default()
    };
    vec![
        ("excel".to_string(), excel),
        ("excel-tab".to_string(), excel_tab),
        ("unix".to_string(), unix),
    ]
}

fn registry_get(name: &str) -> Option<Dialect> {
    DIALECTS.with(|d| {
        d.borrow()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    })
}

// ── writing ──────────────────────────────────────────────────────────────────

/// Render one field. `was_string`/`was_none` drive the type-sensitive quoting
/// modes, which is why the caller passes them rather than just the text.
fn write_field(out: &mut String, d: &Dialect, field: &str, was_string: bool, was_none: bool) -> Result<(), String> {
    let quote = d.needs_quotes(field, was_string, was_none);
    let q = d.quotechar.unwrap_or('"');
    if quote {
        out.push(q);
    }
    for c in field.chars() {
        if c == q && d.quotechar.is_some() && quote {
            // Inside quotes an embedded quote is doubled, or escaped when the
            // dialect says so.
            if d.doublequote {
                out.push(q);
            } else if let Some(e) = d.escapechar {
                out.push(e);
            } else {
                return Err("_csv.Error: need to escape, but no escapechar set".to_string());
            }
        } else if !quote && (c == d.delimiter || c == q || c == '\r' || c == '\n') {
            // Unquoted output has to escape anything structural.
            match d.escapechar {
                Some(e) => out.push(e),
                None if d.quoting == QUOTE_NONE => {
                    return Err("_csv.Error: need to escape, but no escapechar set".to_string())
                }
                None => {}
            }
        }
        out.push(c);
    }
    if quote {
        out.push(q);
    }
    Ok(())
}

/// Render a whole row, terminator included.
pub fn format_row(h: &mut PyHost, d: &Dialect, row: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    for (i, v) in row.iter().enumerate() {
        if i > 0 {
            out.push(d.delimiter);
        }
        let was_none = matches!(v, Value::Undef);
        let was_string = h.as_str(v).is_some();
        // `None` writes as the empty field; everything else uses `str()`.
        let text = if was_none { String::new() } else { h.str_of(v) };
        write_field(&mut out, d, &text, was_string, was_none)?;
    }
    out.push_str(&d.lineterminator);
    Ok(out)
}

// ── reading ──────────────────────────────────────────────────────────────────

/// Split `src` into rows of fields.
///
/// This is CPython's state machine, not RFC 4180: a quote may open a field
/// mid-way, a lone `\r` ends a record, and a doubled quote inside a quoted field
/// is one literal quote. Real-world CSV depends on every one of those.
pub fn parse_rows(src: &str, d: &Dialect) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_started = false;
    let mut any = false;
    let chars: Vec<char> = src.chars().collect();
    let q = d.quotechar;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if Some(c) == q {
                // A doubled quote is a literal one; a single quote closes.
                if d.doublequote && chars.get(i + 1) == q.as_ref() {
                    field.push(c);
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            if Some(c) == d.escapechar {
                if let Some(n) = chars.get(i + 1) {
                    field.push(*n);
                    i += 2;
                    continue;
                }
            }
            field.push(c);
            i += 1;
            continue;
        }
        match c {
            _ if Some(c) == q && d.quoting != QUOTE_NONE => {
                in_quotes = true;
                field_started = true;
                any = true;
            }
            _ if Some(c) == d.escapechar => {
                if let Some(n) = chars.get(i + 1) {
                    field.push(*n);
                    i += 2;
                    any = true;
                    continue;
                }
            }
            _ if c == d.delimiter => {
                row.push(std::mem::take(&mut field));
                field_started = false;
                any = true;
            }
            '\r' | '\n' => {
                // `\r\n` is one terminator; a lone `\r` or `\n` also ends the row.
                if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
                // A blank line is an EMPTY ROW (`[]`), not a skipped one — the
                // reader reports it so a caller can see the gap. A row with any
                // content closes normally.
                if any || field_started || !field.is_empty() || !row.is_empty() {
                    row.push(std::mem::take(&mut field));
                }
                rows.push(std::mem::take(&mut row));
                field.clear();
                field_started = false;
                any = false;
            }
            ' ' if d.skipinitialspace && !field_started => {}
            _ => {
                field.push(c);
                field_started = true;
                any = true;
            }
        }
        i += 1;
    }
    if any || field_started || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

// ── module surface ───────────────────────────────────────────────────────────

/// `_csv.<fn>(...)`. The reader and writer objects are built by `csv.py` on top
/// of these, so what this exposes is the parsing, the formatting, and the
/// registry.
pub fn call(
    h: &mut PyHost,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    Some(match name {
        // `reader(iterable, dialect='excel', **fmtparams)` — the rows, eagerly.
        // CPython's reader is lazy over the source iterator; `csv.py` only ever
        // iterates it, and materializing keeps the parser a pure function.
        // `reader` is handled by the caller, outside the host borrow: iterating
        // the source runs user code.
        "reader" => return None,
        // `writer(fileobj, dialect='excel', **fmtparams)` — a handle carrying the
        // stream and the resolved dialect; `csv.py` calls `writerow` on it.
        "writer" => (|| -> Result<Value, String> {
            let stream = args.first().cloned().unwrap_or(Value::Undef);
            let d = Dialect::resolve(h, args.get(1), kwargs)?;
            Ok(h.alloc(PyObj::CsvWriter {
                stream,
                dialect: Box::new(d),
            }))
        })(),
        "register_dialect" => (|| -> Result<Value, String> {
            let name = h
                .as_str(args.first().unwrap_or(&Value::Undef))
                .ok_or_else(|| host::type_error("dialect name must be a string"))?;
            let d = Dialect::resolve(h, args.get(1), kwargs)?;
            DIALECTS.with(|reg| {
                let mut reg = reg.borrow_mut();
                reg.retain(|(n, _)| *n != name);
                reg.push((name, d));
            });
            Ok(Value::Undef)
        })(),
        "unregister_dialect" => {
            let name = h.as_str(args.first().unwrap_or(&Value::Undef)).unwrap_or_default();
            let known = DIALECTS.with(|reg| {
                let mut reg = reg.borrow_mut();
                let before = reg.len();
                reg.retain(|(n, _)| *n != name);
                reg.len() != before
            });
            if known {
                Ok(Value::Undef)
            } else {
                Err(format!("_csv.Error: unknown dialect '{name}'"))
            }
        }
        "get_dialect" => (|| -> Result<Value, String> {
            let name = h.as_str(args.first().unwrap_or(&Value::Undef)).unwrap_or_default();
            let d = registry_get(&name)
                .ok_or_else(|| format!("_csv.Error: unknown dialect '{name}'"))?;
            Ok(h.alloc(PyObj::CsvDialect(Box::new(d))))
        })(),
        "list_dialects" => {
            let names = DIALECTS.with(|reg| {
                reg.borrow().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
            });
            let vals: Vec<Value> = names.into_iter().map(|n| h.new_str(n)).collect();
            Ok(h.new_list(vals))
        }
        // `Dialect(obj)` — validate the parameters and hand back the resolved
        // dialect. `csv.Dialect._validate` calls this purely for the check.
        "Dialect" => (|| -> Result<Value, String> {
            let d = Dialect::resolve(h, args.first(), kwargs)?;
            Ok(h.alloc(PyObj::CsvDialect(Box::new(d))))
        })(),
        // The field-size limit exists to bound memory on hostile input; this
        // parser reads whole rows already, so the value is reported and stored
        // but never enforced.
        "field_size_limit" => Ok(Value::Int(131_072)),
        _ => return None,
    })
}

/// The stream and dialect behind a writer, if `recv` is one.
pub fn writer_parts(h: &PyHost, recv: &Value) -> Option<(Value, Dialect)> {
    match h.get(recv) {
        Some(PyObj::CsvWriter { stream, dialect }) => Some((stream.clone(), (**dialect).clone())),
        _ => None,
    }
}

/// `writer.writerow(row)` / `.writerows(rows)`.
///
/// Driven from OUTSIDE the host borrow: writing calls the stream's `write`, and
/// iterating the row runs user code — both re-enter the interpreter.
pub fn write_rows(recv: &Value, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let (stream, dialect) = host::with_host(|h| writer_parts(h, recv))?;
    let arg = args.first().cloned().unwrap_or(Value::Undef);
    Some((|| -> Result<Value, String> {
        let rows: Vec<Value> = match name {
            "writerow" => vec![arg],
            "writerows" => crate::host::iter_vec(&arg)?,
            _ => return Err(host::type_error("not a writer method")),
        };
        let mut last = Value::Undef;
        for r in rows {
            let row = crate::host::iter_vec(&r)?;
            let text = host::with_host(|h| format_row(h, &dialect, &row))?;
            let sv = host::with_host(|h| h.new_str(text));
            last = crate::host::call_method(&stream, "write", vec![sv], vec![])?;
        }
        Ok(last)
    })())
}

/// Attributes of a writer (`.dialect`), a reader (`.line_num`), or a dialect.
pub fn attr(h: &mut PyHost, recv: &Value, name: &str) -> Option<Result<Value, String>> {
    let d = match h.get(recv) {
        Some(PyObj::CsvReader { idx, dialect, .. }) => {
            if name == "line_num" {
                return Some(Ok(Value::Int(*idx as i64)));
            }
            (**dialect).clone()
        }
        Some(PyObj::CsvDialect(d)) => (**d).clone(),
        Some(PyObj::CsvWriter { dialect, .. }) => {
            if name == "dialect" {
                let d = (**dialect).clone();
                return Some(Ok(h.alloc(PyObj::CsvDialect(Box::new(d)))));
            }
            (**dialect).clone()
        }
        _ => return None,
    };
    Some(match name {
        "delimiter" => Ok(h.new_str(d.delimiter.to_string())),
        "quotechar" => Ok(match d.quotechar {
            Some(c) => h.new_str(c.to_string()),
            None => Value::Undef,
        }),
        "escapechar" => Ok(match d.escapechar {
            Some(c) => h.new_str(c.to_string()),
            None => Value::Undef,
        }),
        "doublequote" => Ok(Value::Bool(d.doublequote)),
        "skipinitialspace" => Ok(Value::Bool(d.skipinitialspace)),
        "strict" => Ok(Value::Bool(d.strict)),
        "lineterminator" => Ok(h.new_str(d.lineterminator.clone())),
        "quoting" => Ok(Value::Int(d.quoting)),
        _ => return None,
    })
}

/// The `_csv` namespace.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const FNS: &[&str] = &[
        "reader",
        "writer",
        "register_dialect",
        "unregister_dialect",
        "get_dialect",
        "list_dialects",
        "field_size_limit",
        "Dialect",
    ];
    let mut out: Vec<(String, Value)> = FNS
        .iter()
        .map(|f| ((*f).to_string(), h.alloc(PyObj::Builtin(format!("_csv.{f}")))))
        .collect();
    for (k, v) in [
        ("QUOTE_MINIMAL", QUOTE_MINIMAL),
        ("QUOTE_ALL", QUOTE_ALL),
        ("QUOTE_NONNUMERIC", QUOTE_NONNUMERIC),
        ("QUOTE_NONE", QUOTE_NONE),
        ("QUOTE_STRINGS", QUOTE_STRINGS),
        ("QUOTE_NOTNULL", QUOTE_NOTNULL),
    ] {
        out.push((k.to_string(), Value::Int(v)));
    }
    // `csv.Error` is its own exception class; `Exception` is its base.
    let err = h.alloc(PyObj::Builtin("_csv.Error".into()));
    out.push(("Error".to_string(), err));
    out
}

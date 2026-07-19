//! The `datetime` standard-library module — a deliberately partial, host-type-free
//! subset.
//!
//! ## Representation (no host classes yet)
//!
//! pythonrs has no dedicated `datetime`/`date`/`timedelta` heap objects, so this
//! module represents a datetime as a **plain `dict`** with integer fields:
//!
//! ```text
//! {"year": Y, "month": M, "day": D,
//!  "hour": h, "minute": m, "second": s, "microsecond": us}
//! ```
//!
//! A date is the same dict without the time fields (`year`/`month`/`day` only).
//! This is a documented stopgap: a faithful `datetime` needs real host classes
//! with attribute access (`dt.year`), comparison, `+`/`-` with `timedelta`, and a
//! `__str__`. Those require a host-type pass (see the "deferred" note at the end of
//! this header). Until then, callers read fields with subscript syntax
//! (`dt["year"]`) and do arithmetic through `timestamp`/`fromtimestamp`.
//!
//! ## Time zone honesty
//!
//! `std::time` gives only the UTC epoch and there is no bundled tz database, so
//! **every value this module produces or consumes is UTC**. CPython's
//! `datetime.now()`/`.timestamp()` use the system local zone; we cannot reproduce
//! that faithfully without a tz database, so `now()` here is exactly `utcnow()`,
//! and `timestamp()` interprets the dict as UTC. This is called out rather than
//! silently returning wrong local times. Leap seconds are ignored (POSIX/proleptic
//! Gregorian), matching CPython.
//!
//! ## Module surface (flat, a documented deviation)
//!
//! CPython nests the class under the module (`datetime.datetime.now()`). Without a
//! callable class object, the functions are flat on the module:
//!
//! | call                              | returns                                   |
//! |-----------------------------------|-------------------------------------------|
//! | `datetime.now()` / `.utcnow()`    | dict — current UTC datetime               |
//! | `datetime.today()`                | dict — current UTC date (y/m/d)           |
//! | `datetime.datetime(y,m,d,...)`    | dict — constructed datetime (validated)   |
//! | `datetime.date(y,m,d)`            | dict — constructed date (validated)       |
//! | `datetime.fromtimestamp(ts)`      | dict — UTC datetime for epoch seconds     |
//! | `datetime.timestamp(dt)`          | float — epoch seconds (UTC) for a dict    |
//! | `datetime.strftime(dt, fmt)`      | str — `%Y %m %d %H %M %S %f %j %y %w`     |
//! |                                   |       `%A %a %B %b %%` (others pass thru)  |
//! | `datetime.isoformat(dt)`          | str — ISO-8601 `YYYY-MM-DDTHH:MM:SS`      |
//!
//! Date arithmetic is done by round-tripping through the epoch, which is exact for
//! UTC, e.g. `fromtimestamp(timestamp(dt) + 86400)` for "tomorrow". No `timedelta`
//! object is provided (it needs a host type).
//!
//! ## Deferred (needs a host-type pass, not faked here)
//!
//! Real `datetime`/`date`/`time`/`timedelta` classes, attribute access, rich
//! comparison, `+`/`-` operator overloading, `strptime` parsing, `%z`/`%Z` tz
//! codes, and local-time conversion. These are intentionally NOT implemented as
//! dict hacks — see the parent's report.
//!
//! Wiring (done by the parent): an `import_module` arm for `"datetime"` calling
//! [`entries`], a `call_builtin_function` arm routing `datetime.*` to [`call`],
//! and a `datetime.` prefix in `is_builtin_function`.

use crate::host::{type_error, PKey, PyHost, PyObj};
use fusevm::Value;
use indexmap::IndexMap;

// ── module surface ───────────────────────────────────────────────────────────

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for name in [
        "now",
        "utcnow",
        "today",
        "datetime",
        "date",
        "fromtimestamp",
        "timestamp",
        "strftime",
        "isoformat",
    ] {
        out.push((
            name.to_string(),
            h.alloc(PyObj::Builtin(format!("datetime.{name}"))),
        ));
    }
    out
}

/// Dispatch a `datetime.*` builtin. `fname` arrives already stripped of the
/// `datetime.` prefix. Returns `None` for names this module does not own.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let r = match fname {
        "now" | "utcnow" => now(h),
        "today" => today(h),
        "datetime" => construct_datetime(h, args),
        "date" => construct_date(h, args),
        "fromtimestamp" => fromtimestamp(h, args),
        "timestamp" => timestamp(h, args),
        "strftime" => strftime(h, args),
        "isoformat" => isoformat(h, args),
        _ => return None,
    };
    Some(r)
}

// ── civil-date arithmetic (Howard Hinnant's algorithms, integer-exact) ────────
//
// days_from_civil / civil_from_days convert between a proleptic-Gregorian
// (year, month, day) and a day count relative to 1970-01-01 (day 0). Correct for
// the full i64 range with no leap-second fudging, matching POSIX/CPython.

/// Days since 1970-01-01 for a valid civil (year, month, day).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Civil (year, month, day) for a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days in a given month (1-12) of a given year.
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// ── field struct + dict <-> fields ────────────────────────────────────────────

/// The seven datetime fields. Time components default to 0 for a bare date.
#[derive(Clone, Copy)]
struct Dt {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    microsecond: i64,
}

/// Build the dict representation from fields (all seven keys, integer values).
fn dt_to_dict(h: &mut PyHost, dt: Dt) -> Value {
    let fields = [
        ("year", dt.year),
        ("month", dt.month),
        ("day", dt.day),
        ("hour", dt.hour),
        ("minute", dt.minute),
        ("second", dt.second),
        ("microsecond", dt.microsecond),
    ];
    let mut map: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    for (k, v) in fields {
        let kv = h.new_str(k);
        map.insert(PKey::Str(k.to_string()), (kv, Value::Int(v)));
    }
    h.new_dict(map)
}

/// Build the date dict (year/month/day only).
fn date_to_dict(h: &mut PyHost, y: i64, m: i64, d: i64) -> Value {
    let fields = [("year", y), ("month", m), ("day", d)];
    let mut map: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    for (k, v) in fields {
        let kv = h.new_str(k);
        map.insert(PKey::Str(k.to_string()), (kv, Value::Int(v)));
    }
    h.new_dict(map)
}

/// Read one integer field from a datetime/date dict, or `None` if absent.
fn field(h: &PyHost, dt: &Value, name: &str) -> Option<i64> {
    let v = match h.get(dt) {
        Some(PyObj::Dict(m)) => m.get(&PKey::Str(name.to_string())).map(|(_, v)| v.clone()),
        _ => None,
    }?;
    h.as_int(&v)
}

/// Read a full `Dt` from a dict argument (time fields default to 0). The dict must
/// carry at least `year`/`month`/`day`; missing required fields are an error.
fn dt_from_dict(h: &PyHost, dt: &Value) -> Result<Dt, String> {
    if !matches!(h.get(dt), Some(PyObj::Dict(_))) {
        return Err(type_error(&format!(
            "expected a datetime/date dict, got '{}'",
            h.type_name(dt)
        )));
    }
    let req = |name: &str| -> Result<i64, String> {
        field(h, dt, name)
            .ok_or_else(|| type_error(&format!("datetime dict missing integer field '{name}'")))
    };
    Ok(Dt {
        year: req("year")?,
        month: req("month")?,
        day: req("day")?,
        hour: field(h, dt, "hour").unwrap_or(0),
        minute: field(h, dt, "minute").unwrap_or(0),
        second: field(h, dt, "second").unwrap_or(0),
        microsecond: field(h, dt, "microsecond").unwrap_or(0),
    })
}

// ── epoch <-> fields ──────────────────────────────────────────────────────────

/// Seconds (UTC) + sub-second microseconds since the Unix epoch, from the wall
/// clock. Pre-1970 wall clocks (unusual) fold to a negative epoch cleanly.
fn wall_clock() -> (i64, i64) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_micros() as i64),
        Err(e) => {
            // Clock is before the epoch: `e.duration()` is how far before.
            let d = e.duration();
            let secs = -(d.as_secs() as i64);
            let us = d.subsec_micros() as i64;
            if us == 0 {
                (secs, 0)
            } else {
                // Represent as (secs-1) whole seconds plus the fractional remainder.
                (secs - 1, 1_000_000 - us)
            }
        }
    }
}

/// Split UTC epoch seconds + microseconds into calendar fields.
fn from_epoch(secs: i64, micros: i64) -> Dt {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Dt {
        year,
        month,
        day,
        hour: tod / 3600,
        minute: (tod % 3600) / 60,
        second: tod % 60,
        microsecond: micros,
    }
}

/// Epoch seconds (UTC, fractional) for a `Dt`.
fn to_epoch_secs(dt: Dt) -> f64 {
    let days = days_from_civil(dt.year, dt.month, dt.day);
    let whole = days * 86_400 + dt.hour * 3600 + dt.minute * 60 + dt.second;
    whole as f64 + dt.microsecond as f64 / 1_000_000.0
}

// ── functions ─────────────────────────────────────────────────────────────────

fn now(h: &mut PyHost) -> Result<Value, String> {
    let (secs, micros) = wall_clock();
    let dt = from_epoch(secs, micros);
    Ok(dt_to_dict(h, dt))
}

fn today(h: &mut PyHost) -> Result<Value, String> {
    let (secs, _) = wall_clock();
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    Ok(date_to_dict(h, y, m, d))
}

/// Validate and construct a datetime dict.
/// `datetime(year, month, day[, hour[, minute[, second[, microsecond]]]])`.
fn construct_datetime(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let year = req_int(h, args, 0, "year")?;
    let month = req_int(h, args, 1, "month")?;
    let day = req_int(h, args, 2, "day")?;
    let hour = opt_int(h, args, 3)?;
    let minute = opt_int(h, args, 4)?;
    let second = opt_int(h, args, 5)?;
    let microsecond = opt_int(h, args, 6)?;
    let dt = Dt {
        year,
        month,
        day,
        hour,
        minute,
        second,
        microsecond,
    };
    validate(dt)?;
    Ok(dt_to_dict(h, dt))
}

/// Validate and construct a date dict. `date(year, month, day)`.
fn construct_date(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let year = req_int(h, args, 0, "year")?;
    let month = req_int(h, args, 1, "month")?;
    let day = req_int(h, args, 2, "day")?;
    validate(Dt {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        microsecond: 0,
    })?;
    Ok(date_to_dict(h, year, month, day))
}

/// CPython-style range checks (raise `ValueError` on out-of-range fields).
fn validate(dt: Dt) -> Result<(), String> {
    if !(1..=9999).contains(&dt.year) {
        return Err("ValueError: year is out of range".into());
    }
    if !(1..=12).contains(&dt.month) {
        return Err("ValueError: month must be in 1..12".into());
    }
    let dim = days_in_month(dt.year, dt.month);
    if !(1..=dim).contains(&dt.day) {
        return Err("ValueError: day is out of range for month".into());
    }
    if !(0..=23).contains(&dt.hour) {
        return Err("ValueError: hour must be in 0..23".into());
    }
    if !(0..=59).contains(&dt.minute) {
        return Err("ValueError: minute must be in 0..59".into());
    }
    if !(0..=59).contains(&dt.second) {
        return Err("ValueError: second must be in 0..59".into());
    }
    if !(0..=999_999).contains(&dt.microsecond) {
        return Err("ValueError: microsecond must be in 0..999999".into());
    }
    Ok(())
}

/// `fromtimestamp(ts)` — a UTC datetime dict for epoch seconds (int or float).
fn fromtimestamp(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let ts = match args.first() {
        Some(Value::Int(n)) => *n as f64,
        Some(Value::Float(f)) => *f,
        Some(Value::Bool(b)) => *b as i64 as f64,
        _ => {
            return Err(type_error(
                "fromtimestamp() argument must be a number (epoch seconds)",
            ))
        }
    };
    // Floor to whole seconds; carry the fractional part into microseconds.
    let secs = ts.floor() as i64;
    let micros = ((ts - ts.floor()) * 1_000_000.0).round() as i64;
    // A rounded microsecond of 1e6 rolls into the next second.
    let (secs, micros) = if micros >= 1_000_000 {
        (secs + 1, micros - 1_000_000)
    } else {
        (secs, micros)
    };
    let dt = from_epoch(secs, micros);
    Ok(dt_to_dict(h, dt))
}

/// `timestamp(dt)` — epoch seconds (float, UTC) for a datetime/date dict.
fn timestamp(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let dt = match args.first() {
        Some(v) => dt_from_dict(h, v)?,
        None => return Err(type_error("timestamp() missing required argument: 'dt'")),
    };
    Ok(Value::Float(to_epoch_secs(dt)))
}

/// `isoformat(dt)` — `YYYY-MM-DDTHH:MM:SS`, plus `.ffffff` when microseconds are
/// non-zero (matching CPython's `datetime.isoformat`).
fn isoformat(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let dt = match args.first() {
        Some(v) => dt_from_dict(h, v)?,
        None => return Err(type_error("isoformat() missing required argument: 'dt'")),
    };
    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    );
    if dt.microsecond != 0 {
        s.push_str(&format!(".{:06}", dt.microsecond));
    }
    Ok(h.new_str(s))
}

// ── strftime ──────────────────────────────────────────────────────────────────

const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// Weekday names indexed Monday=0 .. Sunday=6 (Python's `weekday()` order).
const WEEKDAY_FULL: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const WEEKDAY_ABBR: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Weekday with Monday=0 (Python `weekday()`); 1970-01-01 (day 0) is Thursday=3.
fn weekday_mon0(dt: Dt) -> i64 {
    (days_from_civil(dt.year, dt.month, dt.day) + 3).rem_euclid(7)
}

/// 1-based day of the year.
fn day_of_year(dt: Dt) -> i64 {
    days_from_civil(dt.year, dt.month, dt.day) - days_from_civil(dt.year, 1, 1) + 1
}

/// `strftime(dt, fmt)` for the documented code subset. Unsupported `%X` codes are
/// emitted verbatim (`%` + the char) rather than guessed at.
fn strftime(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let dt = match args.first() {
        Some(v) => dt_from_dict(h, v)?,
        None => return Err(type_error("strftime() missing required argument: 'dt'")),
    };
    let fmt = match args.get(1) {
        Some(Value::Str(s)) => (**s).clone(),
        Some(v) => h
            .as_str(v)
            .ok_or_else(|| type_error("strftime() format must be a str"))?,
        None => return Err(type_error("strftime() missing required argument: 'format'")),
    };

    let wd = weekday_mon0(dt) as usize;
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let code = chars[i + 1];
        i += 2;
        match code {
            'Y' => out.push_str(&format!("{:04}", dt.year)),
            'y' => out.push_str(&format!("{:02}", dt.year.rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", dt.month)),
            'd' => out.push_str(&format!("{:02}", dt.day)),
            'H' => out.push_str(&format!("{:02}", dt.hour)),
            'M' => out.push_str(&format!("{:02}", dt.minute)),
            'S' => out.push_str(&format!("{:02}", dt.second)),
            'f' => out.push_str(&format!("{:06}", dt.microsecond)),
            'j' => out.push_str(&format!("{:03}", day_of_year(dt))),
            'w' => out.push_str(&(((wd + 1) % 7).to_string())), // %w: Sunday=0
            'A' => out.push_str(WEEKDAY_FULL[wd]),
            'a' => out.push_str(WEEKDAY_ABBR[wd]),
            'B' => out.push_str(MONTH_FULL[(dt.month - 1) as usize]),
            'b' => out.push_str(MONTH_ABBR[(dt.month - 1) as usize]),
            '%' => out.push('%'),
            other => {
                // Unsupported code: pass through unchanged, do not guess.
                out.push('%');
                out.push(other);
            }
        }
    }
    Ok(h.new_str(out))
}

// ── arg helpers ────────────────────────────────────────────────────────────────

fn req_int(h: &PyHost, args: &[Value], i: usize, name: &str) -> Result<i64, String> {
    args.get(i)
        .and_then(|v| h.as_int(v))
        .ok_or_else(|| type_error(&format!("an integer is required for '{name}'")))
}

/// Optional positional int argument; absent or `None` -> 0.
fn opt_int(h: &PyHost, args: &[Value], i: usize) -> Result<i64, String> {
    match args.get(i) {
        None | Some(Value::Undef) => Ok(0),
        Some(v) => h
            .as_int(v)
            .ok_or_else(|| type_error("an integer is required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_epoch_zero() {
        // 1970-01-01 is day 0 and a Thursday.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        let dt = Dt {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        };
        assert_eq!(weekday_mon0(dt), 3); // Thursday
    }

    #[test]
    fn known_timestamp() {
        // 2000-01-01T00:00:00Z == 946684800 (well-known epoch value).
        let dt = Dt {
            year: 2000,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        };
        assert_eq!(to_epoch_secs(dt), 946_684_800.0);
        assert_eq!(civil_from_days(946_684_800 / 86_400), (2000, 1, 1));
    }

    #[test]
    fn leap_day_valid_and_invalid() {
        // 2020 is a leap year (Feb 29 valid); 2021 is not.
        assert_eq!(days_in_month(2020, 2), 29);
        assert_eq!(days_in_month(2021, 2), 28);
        assert!(validate(Dt {
            year: 2020,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        })
        .is_ok());
        assert!(validate(Dt {
            year: 2021,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        })
        .is_err());
    }

    #[test]
    fn day_of_year_leap() {
        // 2020-12-31 is day 366 in a leap year.
        let dt = Dt {
            year: 2020,
            month: 12,
            day: 31,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        };
        assert_eq!(day_of_year(dt), 366);
    }

    #[test]
    fn from_epoch_fields() {
        // 2001-09-09T01:46:40Z == 1_000_000_000 (a well-known epoch landmark).
        let dt = from_epoch(1_000_000_000, 0);
        assert_eq!(
            (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second),
            (2001, 9, 9, 1, 46, 40)
        );
    }
}

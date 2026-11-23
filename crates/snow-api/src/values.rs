//! Converting JSON result cells into Python values (`SnowflakeConverter`).

use crate::output::{Output, Pv, Row, uniquify_columns};
use chrono::{DateTime, Datelike, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use snow_connector::{Column, Cursor};

/// `str(decimal.Decimal(s))`.
pub fn decimal_str(s: &str) -> String {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (mant, exp_part) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int_part, frac_part) = mant.split_once('.').unwrap_or((mant, ""));
    let mut digits: String = format!("{int_part}{frac_part}");
    let mut exp = exp_part - frac_part.len() as i64;
    let trimmed = digits.trim_start_matches('0');
    digits = if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.to_string()
    };
    if digits.chars().any(|c| !c.is_ascii_digit()) {
        return s.to_string();
    }
    let sign = if neg { "-" } else { "" };
    let n = digits.len() as i64;
    let adjusted = exp + n - 1;
    if exp <= 0 && adjusted >= -6 {
        if exp == 0 {
            return format!("{sign}{digits}");
        }
        let point = n + exp;
        if point > 0 {
            let p = point as usize;
            return format!("{sign}{}.{}", &digits[..p], &digits[p..]);
        }
        return format!("{sign}0.{}{digits}", "0".repeat((-point) as usize));
    }
    // Scientific notation.
    let mut m = digits[..1].to_string();
    if n > 1 {
        m.push('.');
        m.push_str(&digits[1..]);
    }
    let _ = &mut exp;
    format!(
        "{sign}{m}E{}{}",
        if adjusted >= 0 { "+" } else { "-" },
        adjusted.abs()
    )
}

fn seconds_micros(value: &str, scale: i64) -> (i64, i64) {
    let negative = value.starts_with('-');
    let (lhs, rhs) = value.split_once('.').unwrap_or((value, ""));
    let seconds: i64 = lhs.parse().unwrap_or(0);
    let mut micro: i64 = if rhs.is_empty() {
        0
    } else {
        rhs.parse().unwrap_or(0)
    };
    if scale < 6 {
        micro *= 10i64.pow((6 - scale) as u32);
    } else if scale > 6 {
        let d = 10i64.pow((scale - 6) as u32);
        micro = if negative {
            (micro + d - 1) / d
        } else {
            micro / d
        };
    }
    if negative {
        micro = -micro;
    }
    (seconds, micro)
}

fn fmt_time(h: u32, m: u32, s: u32, us: u32) -> String {
    if us == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{us:06}")
    }
}

fn fmt_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

fn naive(seconds: i64, micros: i64) -> Option<NaiveDateTime> {
    let base = DateTime::<Utc>::from_timestamp(seconds, 0)?.naive_utc();
    base.checked_add_signed(chrono::Duration::microseconds(micros))
}

fn dt_pv(n: NaiveDateTime, tz: String) -> Pv {
    Pv::DateTime {
        date: format!("{:04}-{:02}-{:02}", n.year(), n.month(), n.day()),
        time: fmt_time(n.hour(), n.minute(), n.second(), n.nanosecond() / 1000),
        tz,
    }
}

/// Convert one cell. `tz_name` is the session `TIMEZONE`.
pub fn convert(col: &Column, cell: Option<&str>, tz_name: &str) -> Pv {
    let Some(v) = cell else { return Pv::None };
    let scale = col.scale.unwrap_or(0);
    match col.type_name.to_lowercase().as_str() {
        "fixed" => {
            // `int if ctx["scale"] == 0 else decimal.Decimal` (a null scale is Decimal).
            if col.scale == Some(0) {
                v.parse::<i128>()
                    .map(Pv::Int)
                    .unwrap_or_else(|_| Pv::Decimal(decimal_str(v)))
            } else {
                Pv::Decimal(decimal_str(v))
            }
        }
        "decfloat" => Pv::Decimal(decimal_str(v)),
        "real" => Pv::Float(match v.to_lowercase().as_str() {
            "nan" => f64::NAN,
            "inf" | "infinity" => f64::INFINITY,
            "-inf" | "-infinity" => f64::NEG_INFINITY,
            _ => v.parse().unwrap_or(0.0),
        }),
        "boolean" => Pv::Bool(v == "1" || v == "TRUE"),
        "date" => {
            let days: i64 = v.parse().unwrap_or(0);
            match naive(days * 86400, 0) {
                Some(n) => Pv::Date(format!("{:04}-{:02}-{:02}", n.year(), n.month(), n.day())),
                None => Pv::Str(v.to_string()),
            }
        }
        "time" => {
            let (s, us) = seconds_micros(v, scale);
            let n = naive(s, us).unwrap_or_default();
            Pv::Time(fmt_time(
                n.hour(),
                n.minute(),
                n.second(),
                n.nanosecond() / 1000,
            ))
        }
        "timestamp_ntz" => {
            let (s, us) = seconds_micros(v, scale);
            match naive(s, us) {
                Some(n) => dt_pv(n, String::new()),
                None => Pv::Str(v.to_string()),
            }
        }
        "timestamp_tz" => {
            let (val, tz) = v.split_once(' ').unwrap_or((v, "1440"));
            let offset_min: i32 = tz.parse::<i32>().unwrap_or(1440) - 1440;
            let (s, us) = seconds_micros(val, scale);
            match naive(s, us) {
                Some(n) => {
                    let local = n + chrono::Duration::minutes(offset_min as i64);
                    dt_pv(local, fmt_offset(offset_min * 60))
                }
                None => Pv::Str(v.to_string()),
            }
        }
        "timestamp_ltz" | "timestamp" => {
            let (s, us) = seconds_micros(v, scale);
            match naive(s, us) {
                Some(n) => {
                    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
                    let utc = Utc.from_utc_datetime(
                        &DateTime::<Utc>::from_timestamp(s, 0)
                            .map(|d| d.naive_utc())
                            .unwrap_or(n),
                    );
                    let off = tz
                        .offset_from_utc_datetime(&utc.naive_utc())
                        .fix()
                        .local_minus_utc();
                    let local = n + chrono::Duration::seconds(off as i64);
                    dt_pv(local, fmt_offset(off))
                }
                None => Pv::Str(v.to_string()),
            }
        }
        "binary" => Pv::Bytes(
            (0..v.len() / 2)
                .filter_map(|i| u8::from_str_radix(&v[2 * i..2 * i + 2], 16).ok())
                .collect(),
        ),
        "vector" => serde_json::from_str::<serde_json::Value>(v)
            .map(|j| Pv::from_json(&j))
            .unwrap_or_else(|_| Pv::Str(v.to_string())),
        _ => Pv::Str(v.to_string()),
    }
}

pub fn session_timezone(conn: &snow_connector::Connection) -> String {
    conn.parameter("TIMEZONE")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("UTC")
        .to_string()
}

/// Rows of a cursor as dictionaries keyed by de-duplicated column names.
pub fn rows(cursor: &Cursor, tz: &str, json_ext: bool) -> Vec<Row> {
    let names = uniquify_columns(cursor.columns.iter().map(|c| c.name.clone()).collect());
    cursor
        .rows
        .iter()
        .map(|r| {
            names
                .iter()
                .zip(&cursor.columns)
                .enumerate()
                .map(|(i, (n, c))| {
                    let cell = r.get(i).and_then(|x| x.as_deref());
                    let mut v = convert(c, cell, tz);
                    if json_ext {
                        let code = c.type_code();
                        if matches!(code, 5 | 9 | 10)
                            && let Pv::Str(s) = &v
                            && let Ok(j) = serde_json::from_str::<serde_json::Value>(s)
                        {
                            v = Pv::from_json(&j);
                        }
                    }
                    (n.clone(), v)
                })
                .collect()
        })
        .collect()
}

/// `QueryResult(cursor)`.
pub fn query_result(cursor: &Cursor, tz: &str, json_ext: bool) -> Output {
    Output::Collection(rows(cursor, tz, json_ext))
}

/// `SingleQueryResult(cursor)`: the first row, or None.
pub fn single_query_result(cursor: &Cursor, tz: &str, json_ext: bool) -> Output {
    Output::Object(rows(cursor, tz, json_ext).into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_str_matches_python() {
        for (input, want) in [
            ("1.50", "1.50"),
            ("-0.10", "-0.10"),
            ("123", "123"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1E-7"),
            ("0.00", "0.00"),
            ("0.0000000", "0E-7"),
            ("1E+5", "1E+5"),
            ("12345678901234567890.123", "12345678901234567890.123"),
        ] {
            assert_eq!(decimal_str(input), want, "{input}");
        }
    }

    #[test]
    fn timestamp_tz_offset() {
        let col = Column {
            name: "T".into(),
            type_name: "timestamp_tz".into(),
            scale: Some(9),
            precision: None,
            length: None,
            nullable: true,
        };
        let v = convert(&col, Some("1675265104.105000000 960"), "UTC");
        assert_eq!(v.py_str(), "2023-02-01 07:25:04.105000-08:00");
    }
}

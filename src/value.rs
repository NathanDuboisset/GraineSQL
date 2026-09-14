//! Canonical value model.
//!
//! Every column is read as text over a session pinned to deterministic output
//! settings (UTC, ISO intervals, hex bytea), parsed per the column's
//! [`TypeClass`], and re-emitted canonically. Writing runs the same path in
//! reverse.
//!
//! The invariant the tests enforce: `parse(class, encode(v)) == v` for every
//! type. Export and load must be exact inverses, or committed files drift on
//! their own.

use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

use crate::schema::TypeClass;

/// Fractional-second digits emitted for time-bearing values. Both Postgres and
/// MySQL top out at microsecond precision.
const FRAC_DIGITS: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Exact decimal, held as its canonical digit string. Never routed through
    /// `f64`, that would silently corrupt money columns.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Uuid(uuid::Uuid),
    /// Raw JSON text as the engine returned it. Parsed lazily, only when the
    /// output mode asks for nested (unrolled) JSON.
    Json(String),
    Date(NaiveDate),
    Time(NaiveTime),
    Timestamp(NaiveDateTime),
    TimestampTz(DateTime<Utc>),
    /// Types we deliberately pass through verbatim: enums, arrays, intervals,
    /// and anything the introspector did not recognise. Lossless because the
    /// write path casts the same text back to the same type.
    Raw(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Parse a value from the engine's canonical *text* output.
    ///
    /// `None` is SQL NULL. This is the only entry point from a database row.
    pub fn parse(class: &TypeClass, text: Option<&str>) -> Result<Value> {
        let Some(s) = text else {
            return Ok(Value::Null);
        };
        Ok(match class {
            TypeClass::Bool => Value::Bool(parse_bool(s)?),
            TypeClass::Int { .. } => Value::Int(
                s.trim()
                    .parse::<i64>()
                    .with_context(|| format!("expected an integer, got {s:?}"))?,
            ),
            TypeClass::Float { .. } => Value::Float(parse_float(s)?),
            TypeClass::Decimal { .. } => {
                validate_decimal(s)?;
                Value::Decimal(s.trim().to_string())
            }
            TypeClass::Text { .. } => Value::Text(s.to_string()),
            TypeClass::Bytes => Value::Bytes(parse_bytes(s)?),
            TypeClass::Uuid => Value::Uuid(
                s.trim()
                    .parse()
                    .with_context(|| format!("expected a uuid, got {s:?}"))?,
            ),
            TypeClass::Json { .. } => {
                // Validate now so a malformed blob fails at export, not at load.
                serde_json::from_str::<serde_json::Value>(s)
                    .with_context(|| format!("expected json, got {}", truncate(s, 60)))?;
                Value::Json(s.to_string())
            }
            TypeClass::Date => Value::Date(parse_date(s)?),
            TypeClass::Time { .. } => Value::Time(parse_time(s)?),
            TypeClass::Timestamp { tz: false } => Value::Timestamp(parse_naive_dt(s)?),
            TypeClass::Timestamp { tz: true } => Value::TimestampTz(parse_dt_tz(s)?),
            TypeClass::Interval
            | TypeClass::Enum { .. }
            | TypeClass::Array { .. }
            | TypeClass::Other { .. } => Value::Raw(s.to_string()),
        })
    }

    /// Canonical single-line text form.
    ///
    /// Used for CSV fields, per-row filename slugs, and as the bind text handed
    /// back to the database. Never quoted or escaped, that is the caller's job.
    pub fn to_text(&self) -> Option<String> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Decimal(d) => d.clone(),
            Value::Text(t) => t.clone(),
            Value::Bytes(b) => format!("\\x{}", hex_encode(b)),
            Value::Uuid(u) => u.to_string(),
            Value::Json(j) => j.clone(),
            Value::Date(d) => d.format("%Y-%m-%d").to_string(),
            Value::Time(t) => format_time(*t),
            Value::Timestamp(t) => {
                format!("{}T{}", t.date().format("%Y-%m-%d"), format_time(t.time()))
            }
            Value::TimestampTz(t) => {
                format!(
                    "{}T{}Z",
                    t.date_naive().format("%Y-%m-%d"),
                    format_time(t.time())
                )
            }
            Value::Raw(r) => r.clone(),
        })
    }

    /// Stable, filesystem-safe fragment identifying this value, for per-row
    /// filenames. Lossy by design; uniqueness is checked by the caller.
    pub fn to_slug(&self) -> String {
        let text = match self.to_text() {
            None => return "null".to_string(),
            Some(t) => t,
        };
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            match ch {
                'a'..='z' | '0'..='9' | '-' | '_' => out.push(ch),
                'A'..='Z' => out.extend(ch.to_lowercase()),
                _ => out.push('-'),
            }
        }
        // Collapse runs of '-' so `2024-01-01 12:00:00` does not become a mess.
        let mut collapsed = String::with_capacity(out.len());
        let mut prev_dash = false;
        for ch in out.chars() {
            if ch == '-' {
                if !prev_dash {
                    collapsed.push(ch);
                }
                prev_dash = true;
            } else {
                collapsed.push(ch);
                prev_dash = false;
            }
        }
        let trimmed = collapsed.trim_matches('-');
        if trimmed.is_empty() {
            "empty".to_string()
        } else if trimmed.len() > 48 {
            // Keep the head, then a short hash so long keys stay unique.
            let hash = blake3::hash(text.as_bytes());
            let head: String = trimmed.chars().take(40).collect();
            format!("{head}-{}", &hash.to_hex()[..8])
        } else {
            trimmed.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_bool(s: &str) -> Result<bool> {
    // Postgres emits t/f, MySQL emits 1/0, our own files emit true/false.
    match s.trim() {
        "t" | "true" | "TRUE" | "True" | "1" | "y" | "yes" | "on" => Ok(true),
        "f" | "false" | "FALSE" | "False" | "0" | "n" | "no" | "off" => Ok(false),
        other => bail!("expected a boolean, got {other:?}"),
    }
}

fn parse_float(s: &str) -> Result<f64> {
    match s.trim() {
        "NaN" | "nan" => Ok(f64::NAN),
        "Infinity" | "inf" | "Inf" => Ok(f64::INFINITY),
        "-Infinity" | "-inf" | "-Inf" => Ok(f64::NEG_INFINITY),
        other => other
            .parse::<f64>()
            .with_context(|| format!("expected a float, got {other:?}")),
    }
}

/// Reject anything that is not a plain decimal literal, so a `Decimal` string is
/// always safe to splice into generated SQL and always re-parses identically.
fn validate_decimal(s: &str) -> Result<()> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty decimal");
    }
    if matches!(t, "NaN" | "nan") {
        // Postgres numeric genuinely supports NaN.
        return Ok(());
    }
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut seen_exp = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot && !seen_exp => seen_dot = true,
            'e' | 'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                if matches!(chars.peek(), Some('-' | '+')) {
                    chars.next();
                }
                if chars.peek().is_none() {
                    bail!("decimal {t:?} has an empty exponent");
                }
            }
            _ => bail!("expected a decimal, got {t:?}"),
        }
    }
    if !seen_digit {
        bail!("expected a decimal, got {t:?}");
    }
    Ok(())
}

fn parse_bytes(s: &str) -> Result<Vec<u8>> {
    let t = s.trim();
    // Postgres hex format, our canonical form, and bare hex (MySQL HEX()).
    let hex = t
        .strip_prefix("\\x")
        .or_else(|| t.strip_prefix("0x"))
        .unwrap_or(t);
    if hex.is_empty() {
        return Ok(Vec::new());
    }
    hex_decode(hex).with_context(|| format!("expected hex-encoded bytes, got {}", truncate(t, 40)))
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .with_context(|| format!("expected a date as YYYY-MM-DD, got {s:?}"))
}

fn parse_time(s: &str) -> Result<NaiveTime> {
    let t = s.trim();
    // Strip a timezone suffix: `timetz` carries one, but an offset on a bare
    // time of day has no meaningful anchor, so we keep the local time.
    let t = strip_offset(t).0;
    NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M"))
        .with_context(|| format!("expected a time, got {s:?}"))
}

/// Parse a timestamp with no time zone.
///
/// A trailing offset is folded to UTC rather than refused: SQLite and MySQL
/// have no tz-aware type to receive a Postgres `timestamptz`, so refusing it
/// fails every cross-engine load of such a column. Sessions are pinned to UTC,
/// so the instant survives and only the designator goes.
fn parse_naive_dt(s: &str) -> Result<NaiveDateTime> {
    let t = s.trim();
    naive_dt_exact(t)
        .or_else(|| {
            let (naive, offset) = strip_offset(t);
            let offset = offset?;
            let naive = naive_dt_exact(naive)?;
            let seconds = parse_offset_seconds(offset).ok()?;
            Some(naive - chrono::Duration::seconds(seconds as i64))
        })
        .with_context(|| format!("expected a timestamp, got {s:?}"))
}

fn naive_dt_exact(t: &str) -> Option<NaiveDateTime> {
    // Accept both the SQL space separator and the ISO `T`.
    NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| {
            NaiveDate::parse_from_str(t, "%Y-%m-%d").map(|d| {
                d.and_time(NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time"))
            })
        })
        .ok()
}

fn parse_dt_tz(s: &str) -> Result<DateTime<Utc>> {
    let t = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Postgres text output: `2024-01-01 12:00:00+00`, offset without minutes.
    let (naive_part, offset) = strip_offset(t);
    let naive = parse_naive_dt(naive_part)?;
    let Some(offset) = offset else {
        // No offset at all, the session is pinned to UTC, so read it as UTC.
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    };
    let seconds = parse_offset_seconds(offset)
        .with_context(|| format!("expected a UTC offset, got {offset:?} in {s:?}"))?;
    let utc = naive - chrono::Duration::seconds(seconds as i64);
    Ok(DateTime::from_naive_utc_and_offset(utc, Utc))
}

/// Split a trailing timezone designator off a date/time string.
fn strip_offset(s: &str) -> (&str, Option<&str>) {
    if let Some(rest) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return (rest, Some("+00"));
    }
    // Scan from the right for a sign that starts an offset, but do not mistake
    // the '-' separators inside the date for one.
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate().rev() {
        if *b == b'+' {
            return (&s[..i], Some(&s[i..]));
        }
        if *b == b'-' {
            // An offset '-' comes after the time part, which contains ':'.
            if s[..i].contains(':') {
                return (&s[..i], Some(&s[i..]));
            }
            break;
        }
        if !b.is_ascii_digit() && *b != b':' {
            break;
        }
    }
    (s, None)
}

fn parse_offset_seconds(off: &str) -> Result<i32> {
    let (sign, rest) = match off.as_bytes().first() {
        Some(b'+') => (1, &off[1..]),
        Some(b'-') => (-1, &off[1..]),
        _ => bail!("offset {off:?} has no sign"),
    };
    let mut parts = rest.split(':');
    let hours: i32 = parts.next().unwrap_or("").parse().context("offset hours")?;
    let minutes: i32 = match parts.next() {
        Some(m) => m.parse().context("offset minutes")?,
        None => 0,
    };
    let seconds: i32 = match parts.next() {
        Some(s) => s.parse().context("offset seconds")?,
        None => 0,
    };
    Ok(sign * (hours * 3600 + minutes * 60 + seconds))
}

// ---------------------------------------------------------------------------
// Canonical formatting
// ---------------------------------------------------------------------------

/// Shortest representation that round-trips back to the same `f64`.
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(f);
    // ryu always emits a fractional part; trim `1.0` to `1` so integral floats
    // match how every other integral value is written.
    s.strip_suffix(".0").unwrap_or(s).to_string()
}

/// `HH:MM:SS` with exactly six fractional digits when the value has any, none
/// when it does not. Fixed precision keeps output stable across engines.
fn format_time(t: NaiveTime) -> String {
    use chrono::Timelike;
    let micros = t.nanosecond() / 1_000;
    if micros == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        format!(
            "{}.{:0width$}",
            t.format("%H:%M:%S"),
            micros,
            width = FRAC_DIGITS
        )
    }
}

/// Lowercase hex, the form GraineSQL writes.
pub fn hex_encode(bytes: &[u8]) -> String {
    hex(bytes, false)
}

/// Uppercase hex, which `UNHEX` and `X'..'` literals conventionally take.
pub fn hex_encode_upper(bytes: &[u8]) -> String {
    hex(bytes, true)
}

fn hex(bytes: &[u8], upper: bool) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = if upper {
            write!(out, "{b:02X}")
        } else {
            write!(out, "{b:02x}")
        };
    }
    out
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("hex string has an odd length ({} chars)", s.len());
    }
    let bytes = s.as_bytes();
    (0..bytes.len())
        .step_by(2)
        .map(|i| {
            let hi = hex_nibble(bytes[i])?;
            let lo = hex_nibble(bytes[i + 1])?;
            Ok(hi << 4 | lo)
        })
        .collect()
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => bail!("{:?} is not a hex digit", other as char),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every value must survive text -> Value -> text unchanged.
    fn round_trip(class: &TypeClass, text: &str) -> String {
        let v = Value::parse(class, Some(text)).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
        let out = v.to_text().expect("non-null value has text");
        // A second pass must be a fixed point.
        let again = Value::parse(class, Some(&out)).unwrap();
        assert_eq!(
            again.to_text().unwrap(),
            out,
            "encoding is not idempotent for {text:?}"
        );
        out
    }

    #[test]
    fn null_round_trips() {
        for class in [
            TypeClass::Bool,
            TypeClass::Text { max_len: None },
            TypeClass::Uuid,
        ] {
            let v = Value::parse(&class, None).unwrap();
            assert!(v.is_null());
            assert_eq!(v.to_text(), None);
        }
    }

    #[test]
    fn bool_canonicalises_engine_spellings() {
        let c = TypeClass::Bool;
        assert_eq!(round_trip(&c, "t"), "true");
        assert_eq!(round_trip(&c, "f"), "false");
        assert_eq!(round_trip(&c, "1"), "true");
        assert_eq!(round_trip(&c, "0"), "false");
        assert!(Value::parse(&c, Some("maybe")).is_err());
    }

    #[test]
    fn int_round_trips_at_the_extremes() {
        let c = TypeClass::Int { bits: 64 };
        assert_eq!(round_trip(&c, "0"), "0");
        assert_eq!(round_trip(&c, "-1"), "-1");
        assert_eq!(round_trip(&c, &i64::MAX.to_string()), i64::MAX.to_string());
        assert_eq!(round_trip(&c, &i64::MIN.to_string()), i64::MIN.to_string());
    }

    #[test]
    fn float_uses_shortest_round_trip_form() {
        let c = TypeClass::Float { bits: 64 };
        assert_eq!(round_trip(&c, "1"), "1");
        assert_eq!(round_trip(&c, "1.0"), "1");
        assert_eq!(round_trip(&c, "0.1"), "0.1");
        assert_eq!(round_trip(&c, "3.141592653589793"), "3.141592653589793");
        assert_eq!(round_trip(&c, "-0.0"), "-0");
        assert_eq!(round_trip(&c, "1e100"), "1e100");
    }

    #[test]
    fn float_specials_survive() {
        let c = TypeClass::Float { bits: 64 };
        assert_eq!(round_trip(&c, "NaN"), "NaN");
        assert_eq!(round_trip(&c, "Infinity"), "Infinity");
        assert_eq!(round_trip(&c, "-Infinity"), "-Infinity");
    }

    #[test]
    fn float_values_all_round_trip_bitwise() {
        for f in [
            0.1_f64,
            1.0 / 3.0,
            f64::MIN_POSITIVE,
            f64::MAX,
            -f64::MAX,
            1e-308,
            2.225_073_858_507_201e-308,
        ] {
            let s = format_float(f);
            let back: f64 = s.parse().unwrap();
            assert_eq!(f.to_bits(), back.to_bits(), "{s} did not round-trip");
        }
    }

    #[test]
    fn decimal_preserves_exact_digits_including_trailing_zeros() {
        let c = TypeClass::Decimal {
            precision: Some(20),
            scale: Some(4),
        };
        // Trailing zeros are semantically meaningful in SQL numeric output and
        // would be destroyed by a trip through f64.
        assert_eq!(round_trip(&c, "1.5000"), "1.5000");
        assert_eq!(round_trip(&c, "0.0000"), "0.0000");
        assert_eq!(
            round_trip(&c, "12345678901234567890.1234567890"),
            "12345678901234567890.1234567890"
        );
        assert_eq!(round_trip(&c, "-0.0001"), "-0.0001");
        assert_eq!(round_trip(&c, "NaN"), "NaN");
    }

    #[test]
    fn decimal_rejects_non_numeric_text() {
        let c = TypeClass::Decimal {
            precision: None,
            scale: None,
        };
        for bad in ["", "1.2.3", "abc", "1e", "--1", "0x10", "1;DROP TABLE t"] {
            assert!(
                Value::parse(&c, Some(bad)).is_err(),
                "{bad:?} should be rejected, decimals are spliced into SQL literals"
            );
        }
    }

    #[test]
    fn bytes_round_trip_via_hex() {
        let c = TypeClass::Bytes;
        assert_eq!(round_trip(&c, "\\x48656c6c6f"), "\\x48656c6c6f");
        assert_eq!(round_trip(&c, "\\x"), "\\x");
        // Uppercase hex and MySQL's bare form normalise to lowercase `\x`.
        assert_eq!(round_trip(&c, "\\xDEADBEEF"), "\\xdeadbeef");
        assert_eq!(round_trip(&c, "deadbeef"), "\\xdeadbeef");
        assert!(Value::parse(&c, Some("\\xabc")).is_err(), "odd length");
        assert!(Value::parse(&c, Some("\\xzz")).is_err(), "not hex");
    }

    #[test]
    fn all_byte_values_survive() {
        let all: Vec<u8> = (0..=255).collect();
        let encoded = Value::Bytes(all.clone()).to_text().unwrap();
        let back = Value::parse(&TypeClass::Bytes, Some(&encoded)).unwrap();
        assert_eq!(back, Value::Bytes(all));
    }

    #[test]
    fn uuid_normalises_to_lowercase_hyphenated() {
        let c = TypeClass::Uuid;
        assert_eq!(
            round_trip(&c, "9F2C4B1E-7A3D-4E5F-8B9C-0D1E2F3A4B5C"),
            "9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c"
        );
        assert!(Value::parse(&c, Some("not-a-uuid")).is_err());
    }

    #[test]
    fn date_round_trips() {
        let c = TypeClass::Date;
        assert_eq!(round_trip(&c, "2024-02-29"), "2024-02-29");
        assert_eq!(round_trip(&c, "0001-01-01"), "0001-01-01");
        assert!(Value::parse(&c, Some("2024-02-30")).is_err());
    }

    #[test]
    fn time_uses_fixed_fractional_precision() {
        let c = TypeClass::Time { tz: false };
        assert_eq!(round_trip(&c, "12:00:00"), "12:00:00");
        // A partial fraction is padded to six digits, so two engines reporting
        // `.5` and `.500000` produce identical files.
        assert_eq!(round_trip(&c, "12:00:00.5"), "12:00:00.500000");
        assert_eq!(round_trip(&c, "12:00:00.123456"), "12:00:00.123456");
        assert_eq!(round_trip(&c, "23:59:59.000001"), "23:59:59.000001");
    }

    #[test]
    fn timestamp_normalises_separator_to_iso() {
        let c = TypeClass::Timestamp { tz: false };
        assert_eq!(round_trip(&c, "2024-01-01 12:00:00"), "2024-01-01T12:00:00");
        assert_eq!(round_trip(&c, "2024-01-01T12:00:00"), "2024-01-01T12:00:00");
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00.25"),
            "2024-01-01T12:00:00.250000"
        );
    }

    #[test]
    fn a_tz_aware_value_folds_into_a_naive_column() {
        // What a Postgres timestamptz export looks like arriving at a SQLite
        // DATETIME or MySQL DATETIME column, which have no tz-aware type.
        let c = TypeClass::Timestamp { tz: false };
        assert_eq!(
            round_trip(&c, "2024-01-01T12:00:00Z"),
            "2024-01-01T12:00:00"
        );
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00+02"),
            "2024-01-01T10:00:00"
        );
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00-05:30"),
            "2024-01-01T17:30:00"
        );
        // Still rejects what is genuinely not a timestamp.
        assert!(Value::parse(&c, Some("not a time")).is_err());
    }

    #[test]
    fn timestamptz_normalises_to_utc() {
        let c = TypeClass::Timestamp { tz: true };
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00+00"),
            "2024-01-01T12:00:00Z"
        );
        assert_eq!(
            round_trip(&c, "2024-01-01T12:00:00Z"),
            "2024-01-01T12:00:00Z"
        );
        // Offsets are folded into UTC so the same instant always writes the same
        // bytes regardless of the session that produced it.
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00+02"),
            "2024-01-01T10:00:00Z"
        );
        assert_eq!(
            round_trip(&c, "2024-01-01 12:00:00-05:30"),
            "2024-01-01T17:30:00Z"
        );
        assert_eq!(
            round_trip(&c, "2024-07-01 12:00:00+02:00"),
            "2024-07-01T10:00:00Z"
        );
    }

    #[test]
    fn dst_boundary_instants_are_unambiguous_once_in_utc() {
        let c = TypeClass::Timestamp { tz: true };
        // 02:30 local on the US "fall back" night happens twice; the offset in
        // the source text is what disambiguates it, and UTC preserves that.
        let first = round_trip(&c, "2024-11-03 01:30:00-04");
        let second = round_trip(&c, "2024-11-03 01:30:00-05");
        assert_eq!(first, "2024-11-03T05:30:00Z");
        assert_eq!(second, "2024-11-03T06:30:00Z");
        assert_ne!(first, second, "DST repeat must not collapse to one instant");
    }

    #[test]
    fn offset_stripping_does_not_eat_date_hyphens() {
        assert_eq!(
            strip_offset("2024-01-01 12:00:00"),
            ("2024-01-01 12:00:00", None)
        );
        assert_eq!(
            strip_offset("2024-01-01 12:00:00-05"),
            ("2024-01-01 12:00:00", Some("-05"))
        );
        assert_eq!(strip_offset("2024-01-01"), ("2024-01-01", None));
    }

    #[test]
    fn text_is_preserved_byte_for_byte() {
        let c = TypeClass::Text { max_len: None };
        for s in [
            "",
            " leading and trailing ",
            "line\nbreak",
            "tab\there",
            "quote\"and'apostrophe",
            "back\\slash",
            "emoji 🌱 and accents éàü",
            "null-ish \\N",
            "\u{1}\u{7f}",
        ] {
            let v = Value::parse(&c, Some(s)).unwrap();
            assert_eq!(v.to_text().unwrap(), s, "text must not be normalised");
        }
    }

    #[test]
    fn json_is_validated_but_passed_through() {
        let c = TypeClass::Json { binary: true };
        assert_eq!(round_trip(&c, r#"{"a": 1}"#), r#"{"a": 1}"#);
        assert_eq!(round_trip(&c, "null"), "null");
        assert_eq!(round_trip(&c, "[]"), "[]");
        assert!(
            Value::parse(&c, Some("{not json")).is_err(),
            "malformed json must fail at export, not at load"
        );
    }

    #[test]
    fn exotic_types_pass_through_verbatim() {
        for c in [
            TypeClass::Enum {
                name: "tier".into(),
            },
            TypeClass::Interval,
            TypeClass::Array {
                of: Box::new(TypeClass::Int { bits: 32 }),
            },
            TypeClass::Other {
                name: "tsvector".into(),
            },
        ] {
            assert_eq!(round_trip(&c, "{1,2,3}"), "{1,2,3}");
            assert_eq!(round_trip(&c, "PT1H30M"), "PT1H30M");
            assert_eq!(round_trip(&c, r#"{"a b","c,d"}"#), r#"{"a b","c,d"}"#);
        }
    }

    #[test]
    fn slugs_are_stable_and_filesystem_safe() {
        assert_eq!(Value::Int(42).to_slug(), "42");
        assert_eq!(Value::Text("Hello World".into()).to_slug(), "hello-world");
        assert_eq!(Value::Text("a//b\\c".into()).to_slug(), "a-b-c");
        assert_eq!(Value::Text("...".into()).to_slug(), "empty");
        assert_eq!(Value::Null.to_slug(), "null");
        assert_eq!(
            Value::Uuid("9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c".parse().unwrap()).to_slug(),
            "9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c"
        );
    }

    #[test]
    fn long_slugs_are_truncated_but_stay_distinct() {
        let a = Value::Text("x".repeat(200)).to_slug();
        let b = Value::Text(format!("{}y", "x".repeat(199))).to_slug();
        assert!(a.len() <= 49, "slug too long: {}", a.len());
        assert_ne!(a, b, "truncation must not collide");
        // And it must be deterministic.
        assert_eq!(a, Value::Text("x".repeat(200)).to_slug());
    }

    #[test]
    fn hex_helpers_are_inverses() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&data)).unwrap(), data);
        assert_eq!(hex_encode(&[]), "");
        assert!(hex_decode("f").is_err());
    }
}

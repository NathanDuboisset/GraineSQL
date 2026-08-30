//! Canonical JSON encoding and decoding for rows.
//!
//! Hand-rolled rather than delegated to `serde_json::to_string` so that number
//! formatting, key order, and escaping are all decided here, every one of them
//! is a determinism requirement, and a serializer that "helpfully" normalises a
//! decimal would silently corrupt money columns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::JsonMode;
use crate::format::{Output, RowWriter};
use crate::io::LineBuffer;
use crate::schema::{Column, Table, TypeClass};
use crate::value::Value;

/// Encode one row as a JSON object.
///
/// Keys are emitted in `columns` order, which is the lock's column order, never
/// map iteration order.
pub fn encode_row(
    out: &mut String,
    columns: &[&Column],
    row: &[Value],
    mode: JsonMode,
    pretty: bool,
) -> Result<()> {
    debug_assert_eq!(columns.len(), row.len());
    let indent = if pretty { 1 } else { 0 };
    out.push('{');
    for (i, (col, value)) in columns.iter().zip(row).enumerate() {
        if i > 0 {
            out.push(',');
        }
        if pretty {
            out.push('\n');
            push_indent(out, indent);
        }
        encode_string(out, &col.name);
        out.push(':');
        if pretty {
            out.push(' ');
        }
        encode_value(out, col, value, mode, pretty, indent)?;
    }
    if pretty && !columns.is_empty() {
        out.push('\n');
    }
    out.push('}');
    Ok(())
}

fn encode_value(
    out: &mut String,
    col: &Column,
    value: &Value,
    mode: JsonMode,
    pretty: bool,
    depth: usize,
) -> Result<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        // A decimal is emitted as a bare JSON number token holding its exact
        // source digits, trailing zeros and all. Routing it through f64, which
        // is what a naive encoder does, would lose precision.
        //
        // Anything a JSON parser would re-render differently is quoted instead,
        // so the exact digits always survive. That covers Postgres `numeric`
        // NaN, which is not a JSON number at all, and exponent forms, which
        // JSON parsers normalise (`1e10` comes back as `1e+10`).
        Value::Decimal(d) if is_lossless_json_number(d) => out.push_str(d),
        Value::Decimal(d) => encode_string(out, d),
        Value::Float(f) if f.is_finite() => out.push_str(&crate::value::format_float(*f)),
        // JSON has no NaN or Infinity literal, so the non-finite floats become
        // strings. The column's type class tells the decoder to expect that.
        Value::Float(f) => encode_string(out, &crate::value::format_float(*f)),
        Value::Json(raw) => match mode {
            JsonMode::String => encode_string(out, raw),
            JsonMode::Unroll => {
                let parsed: serde_json::Value = serde_json::from_str(raw)
                    .with_context(|| format!("re-reading json from column {}", col.name))?;
                // Only objects and arrays are unrolled. A scalar at the root
                // would become ambiguous: an unrolled JSON `null` is
                // indistinguishable from SQL NULL, and an unrolled JSON string
                // is indistinguishable from `json: string` mode. Since those
                // payloads gain nothing from nesting anyway, they stay quoted,
                // which keeps every case decodable.
                if parsed.is_object() || parsed.is_array() {
                    // Always sorted, never conditional on `binary`. jsonb is
                    // key-normalised server-side and plain json is not, so
                    // keying the decision off the column class would make the
                    // same document encode differently depending on which
                    // engine held it, and a Postgres `json` column loaded into
                    // a MySQL `JSON` one would re-export with reordered keys.
                    write_json(out, &parsed, pretty, depth, true);
                } else {
                    encode_string(out, raw);
                }
            }
        },
        other => {
            let text = other.to_text().expect("null was handled above");
            encode_string(out, &text);
        }
    }
    Ok(())
}

/// Emit a parsed JSON value canonically.
fn write_json(out: &mut String, v: &serde_json::Value, pretty: bool, depth: usize, sort: bool) {
    match v {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        // `Number::to_string` preserves the source digits because the
        // `arbitrary_precision` feature stores them verbatim.
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => encode_string(out, s),
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if pretty {
                    out.push('\n');
                    push_indent(out, depth + 1);
                }
                write_json(out, item, pretty, depth + 1, sort);
            }
            if pretty {
                out.push('\n');
                push_indent(out, depth);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            // Collect first so sorting is a decision, not an accident of the map
            // type the `preserve_order` feature happens to select.
            let entries: Vec<(&String, &serde_json::Value)> = if sort {
                let sorted: BTreeMap<&String, &serde_json::Value> = map.iter().collect();
                sorted.into_iter().collect()
            } else {
                map.iter().collect()
            };
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if pretty {
                    out.push('\n');
                    push_indent(out, depth + 1);
                }
                encode_string(out, k);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                write_json(out, val, pretty, depth + 1, sort);
            }
            if pretty {
                out.push('\n');
                push_indent(out, depth);
            }
            out.push('}');
        }
    }
}

/// Whether this decimal can be written as a bare JSON number and read back with
/// byte-identical digits.
///
/// Plain digits with at most one decimal point qualify, which is every value
/// Postgres `numeric` and MySQL `DECIMAL` actually produce. Exponent forms and
/// `NaN` do not, and are quoted instead.
fn is_lossless_json_number(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for b in body.bytes() {
        match b {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// RFC 8259 string escaping.
///
/// Only the characters that must be escaped are escaped: non-ASCII is emitted as
/// UTF-8 so accented text and emoji stay readable in a diff.
pub fn encode_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Decode one JSON object back into a row, in `columns` order.
///
/// A column missing from the object decodes as NULL, so a seed file written
/// before a nullable column was added still loads.
pub fn decode_row(columns: &[&Column], line: &str, where_: &str) -> Result<Vec<Value>> {
    let parsed: serde_json::Value =
        serde_json::from_str(line).with_context(|| format!("parsing json in {where_}"))?;
    let serde_json::Value::Object(map) = parsed else {
        bail!(
            "{where_}: expected a json object, found {}",
            kind_of(&parsed)
        );
    };

    for key in map.keys() {
        if !columns.iter().any(|c| c.name == *key) {
            bail!(
                "{where_}: column {key:?} is in the seed file but not in the table; \
                 re-export after a schema change, or run `graine lock`"
            );
        }
    }

    columns
        .iter()
        .map(|col| match map.get(&col.name) {
            None => Ok(Value::Null),
            Some(v) => {
                decode_value(col, v).with_context(|| format!("{where_}: column {:?}", col.name))
            }
        })
        .collect()
}

fn decode_value(col: &Column, v: &serde_json::Value) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match &col.class {
        // A json column may hold any shape, including a bare string or number,
        // so re-serialize whatever is there rather than expecting a string.
        TypeClass::Json { binary } => {
            let mut raw = String::new();
            match v {
                // `json: string` mode wrote the payload as a JSON string, so a
                // string here is that payload, unless it does not itself parse
                // as JSON, in which case it really is a string document.
                serde_json::Value::String(s) => {
                    match serde_json::from_str::<serde_json::Value>(s) {
                        Ok(_) => raw.push_str(s),
                        Err(_) => write_json(&mut raw, v, false, 0, *binary),
                    }
                }
                other => write_json(&mut raw, other, false, 0, *binary),
            }
            Ok(Value::Json(raw))
        }
        _ => {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                // SQLite and MySQL have no boolean, so a Postgres `boolean`
                // idiomatically becomes an integer column on the way across.
                serde_json::Value::Bool(b) => match col.class {
                    TypeClass::Int { .. } | TypeClass::Decimal { .. } | TypeClass::Float { .. } => {
                        if *b { "1" } else { "0" }.to_string()
                    }
                    _ => b.to_string(),
                },
                serde_json::Value::Number(n) => match col.class {
                    TypeClass::Bool => match n.as_i64() {
                        Some(0) => "false".to_string(),
                        Some(1) => "true".to_string(),
                        _ => n.to_string(),
                    },
                    _ => n.to_string(),
                },
                other => bail!(
                    "expected a {} value, found a json {}",
                    col.class.label(),
                    kind_of(other)
                ),
            };
            Value::parse(&col.class, Some(&text))
        }
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// One JSON object per line, the default format, and the one with the cleanest
/// git diffs, since a changed row is a changed line.
pub struct JsonlWriter<'a> {
    path: String,
    columns: Vec<&'a Column>,
    mode: JsonMode,
    compress: bool,
    buf: LineBuffer,
}

impl<'a> JsonlWriter<'a> {
    pub fn new(path: String, columns: Vec<&'a Column>, mode: JsonMode, compress: bool) -> Self {
        Self {
            path,
            columns,
            mode,
            compress,
            buf: LineBuffer::new(),
        }
    }
}

impl RowWriter for JsonlWriter<'_> {
    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        let mut line = String::new();
        // Never pretty: jsonl is one row per line by definition.
        encode_row(&mut line, &self.columns, row, self.mode, false)?;
        self.buf.push_line(&line);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vec<Output>> {
        let bytes = self.buf.finish();
        let bytes = if self.compress {
            crate::io::gzip(&bytes)?
        } else {
            bytes
        };
        Ok(vec![Output {
            path: self.path,
            bytes,
        }])
    }
}

/// One file per row, in a directory named after the table.
///
/// For small, human-edited tables (templates, config rows) where a one-line
/// diff is unreadable. This is where pretty-printing actually applies.
pub struct PerRowWriter<'a> {
    dir: String,
    columns: Vec<&'a Column>,
    key: Vec<usize>,
    mode: JsonMode,
    pretty: bool,
    outputs: Vec<Output>,
    /// Slugs already used, so a collision becomes a suffix rather than a file
    /// silently overwriting another.
    used: BTreeSet<String>,
}

impl<'a> PerRowWriter<'a> {
    pub fn new(
        dir: String,
        table: &Table,
        columns: Vec<&'a Column>,
        mode: JsonMode,
        pretty: bool,
    ) -> Self {
        // Name files by the primary key when the exported columns include it;
        // otherwise fall back to the row index.
        let key = table
            .primary_key
            .iter()
            .filter_map(|k| columns.iter().position(|c| c.name == *k))
            .collect::<Vec<_>>();
        let key = if key.len() == table.primary_key.len() {
            key
        } else {
            Vec::new()
        };
        Self {
            dir,
            columns,
            key,
            mode,
            pretty,
            outputs: Vec::new(),
            used: BTreeSet::new(),
        }
    }

    fn slug_for(&mut self, row: &[Value]) -> String {
        let index = self.outputs.len();
        let base = if self.key.is_empty() {
            format!("{:06}", index)
        } else {
            self.key
                .iter()
                .map(|i| row[*i].to_slug())
                .collect::<Vec<_>>()
                .join("-")
        };
        if self.used.insert(base.clone()) {
            return base;
        }
        // Two rows slugged the same (different keys, same sanitised form).
        // Disambiguate with the row index, which is stable because the query is
        // totally ordered.
        let mut n = 2;
        loop {
            let candidate = format!("{base}-{n}");
            if self.used.insert(candidate.clone()) {
                return candidate;
            }
            n += 1;
        }
    }
}

impl RowWriter for PerRowWriter<'_> {
    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        let mut text = String::new();
        encode_row(&mut text, &self.columns, row, self.mode, self.pretty)?;
        let slug = self.slug_for(row);
        let mut buf = LineBuffer::new();
        buf.push_str(&text);
        self.outputs.push(Output {
            path: format!("{}/{slug}.json", self.dir),
            bytes: buf.finish(),
        });
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vec<Output>> {
        Ok(self.outputs)
    }
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

pub fn read_jsonl(path: &Path, columns: &[&Column], compressed: bool) -> Result<Vec<Value2D>> {
    let raw =
        std::fs::read(path).with_context(|| format!("reading seed file {}", path.display()))?;
    let raw = if compressed {
        crate::io::gunzip(&raw).with_context(|| format!("decompressing {}", path.display()))?
    } else {
        raw
    };
    let text =
        String::from_utf8(raw).with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, line)| decode_row(columns, line, &format!("{name}:{}", i + 1)))
        .collect()
}

pub fn read_per_row(dir: &Path, columns: &[&Column]) -> Result<Vec<Value2D>> {
    if !dir.is_dir() {
        bail!(
            "seed directory {} does not exist (the table is configured as `layout: per_row`)",
            dir.display()
        );
    }
    // Sort by filename so load order is deterministic and matches export order.
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", dir.display()))?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();

    paths
        .iter()
        .map(|p| {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            decode_row(columns, text.trim(), &name)
        })
        .collect()
}

/// One decoded row.
pub type Value2D = Vec<Value>;

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, class: TypeClass) -> Column {
        Column {
            name: name.into(),
            sql_type: class.label(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn encode(cols: &[Column], row: &[Value], mode: JsonMode, pretty: bool) -> String {
        let refs: Vec<&Column> = cols.iter().collect();
        let mut s = String::new();
        encode_row(&mut s, &refs, row, mode, pretty).unwrap();
        s
    }

    fn round_trip(cols: &[Column], row: &[Value], mode: JsonMode) -> Vec<Value> {
        let text = encode(cols, row, mode, false);
        let refs: Vec<&Column> = cols.iter().collect();
        decode_row(&refs, &text, "test").unwrap()
    }

    #[test]
    fn encodes_a_flat_row_in_column_order() {
        let cols = vec![
            col("id", TypeClass::Int { bits: 64 }),
            col("email", TypeClass::Text { max_len: None }),
            col("active", TypeClass::Bool),
        ];
        let row = vec![
            Value::Int(1),
            Value::Text("a@b.c".into()),
            Value::Bool(true),
        ];
        assert_eq!(
            encode(&cols, &row, JsonMode::Unroll, false),
            r#"{"id":1,"email":"a@b.c","active":true}"#
        );
    }

    #[test]
    fn key_order_follows_the_columns_not_the_alphabet() {
        let cols = vec![
            col("zeta", TypeClass::Int { bits: 32 }),
            col("alpha", TypeClass::Int { bits: 32 }),
        ];
        let out = encode(
            &cols,
            &[Value::Int(1), Value::Int(2)],
            JsonMode::Unroll,
            false,
        );
        assert_eq!(out, r#"{"zeta":1,"alpha":2}"#);
    }

    #[test]
    fn nulls_encode_as_json_null() {
        let cols = vec![col("x", TypeClass::Text { max_len: None })];
        assert_eq!(
            encode(&cols, &[Value::Null], JsonMode::Unroll, false),
            r#"{"x":null}"#
        );
    }

    #[test]
    fn decimals_keep_their_exact_digits_as_bare_number_tokens() {
        let cols = vec![col(
            "amount",
            TypeClass::Decimal {
                precision: Some(20),
                scale: Some(4),
            },
        )];
        let out = encode(
            &cols,
            &[Value::Decimal("1.5000".into())],
            JsonMode::Unroll,
            false,
        );
        // Not 1.5, and not a quoted string: the exact source digits, unquoted.
        assert_eq!(out, r#"{"amount":1.5000}"#);

        let back = round_trip(&cols, &[Value::Decimal("1.5000".into())], JsonMode::Unroll);
        assert_eq!(back, vec![Value::Decimal("1.5000".into())]);
    }

    #[test]
    fn plain_decimals_stay_bare_numbers_and_exotic_ones_get_quoted() {
        let cols = vec![col(
            "n",
            TypeClass::Decimal {
                precision: None,
                scale: None,
            },
        )];
        let enc = |d: &str| encode(&cols, &[Value::Decimal(d.into())], JsonMode::Unroll, false);
        // The shapes a database actually emits stay readable as numbers.
        assert_eq!(enc("1.5000"), r#"{"n":1.5000}"#);
        assert_eq!(enc("-0.0001"), r#"{"n":-0.0001}"#);
        // The shapes a JSON parser would rewrite are quoted to protect them.
        assert_eq!(enc("1e10"), r#"{"n":"1e10"}"#);
        assert_eq!(enc("NaN"), r#"{"n":"NaN"}"#);
    }

    #[test]
    fn a_numeric_nan_is_quoted_so_the_line_stays_valid_json() {
        // Postgres numeric admits NaN; JSON does not. Emitting it bare would
        // produce a file no JSON parser can read.
        let cols = vec![col(
            "n",
            TypeClass::Decimal {
                precision: None,
                scale: None,
            },
        )];
        let out = encode(
            &cols,
            &[Value::Decimal("NaN".into())],
            JsonMode::Unroll,
            false,
        );
        assert_eq!(out, r#"{"n":"NaN"}"#);
        serde_json::from_str::<serde_json::Value>(&out).expect("must be valid json");

        assert_eq!(
            round_trip(&cols, &[Value::Decimal("NaN".into())], JsonMode::Unroll),
            vec![Value::Decimal("NaN".into())]
        );
    }

    #[test]
    fn every_decimal_shape_encodes_to_valid_json() {
        let cols = vec![col(
            "n",
            TypeClass::Decimal {
                precision: None,
                scale: None,
            },
        )];
        for d in [
            "0",
            "-0.0001",
            "1.5000",
            "0.0000000000",
            "1e10",
            "-1E-10",
            "NaN",
            "12345678901234567890.5",
        ] {
            let out = encode(&cols, &[Value::Decimal(d.into())], JsonMode::Unroll, false);
            serde_json::from_str::<serde_json::Value>(&out)
                .unwrap_or_else(|e| panic!("{d} produced invalid json {out}: {e}"));
            assert_eq!(
                round_trip(&cols, &[Value::Decimal(d.into())], JsonMode::Unroll),
                vec![Value::Decimal(d.into())],
                "{d} did not survive"
            );
        }
    }

    #[test]
    fn huge_decimals_survive_the_round_trip() {
        let cols = vec![col(
            "n",
            TypeClass::Decimal {
                precision: None,
                scale: None,
            },
        )];
        let big = "123456789012345678901234567890.123456789012345678901234567890";
        let back = round_trip(&cols, &[Value::Decimal(big.into())], JsonMode::Unroll);
        assert_eq!(back, vec![Value::Decimal(big.into())]);
    }

    #[test]
    fn non_finite_floats_become_strings_and_come_back_as_floats() {
        let cols = vec![col("f", TypeClass::Float { bits: 64 })];
        let out = encode(&cols, &[Value::Float(f64::NAN)], JsonMode::Unroll, false);
        assert_eq!(out, r#"{"f":"NaN"}"#, "json has no NaN literal");

        let back = round_trip(&cols, &[Value::Float(f64::INFINITY)], JsonMode::Unroll);
        assert_eq!(back, vec![Value::Float(f64::INFINITY)]);
    }

    #[test]
    fn unrolled_json_is_nested_not_escaped() {
        let cols = vec![col("prefs", TypeClass::Json { binary: true })];
        let row = vec![Value::Json(r#"{"theme": "dark", "n": 1}"#.into())];
        let out = encode(&cols, &row, JsonMode::Unroll, false);
        assert_eq!(out, r#"{"prefs":{"n":1,"theme":"dark"}}"#);
        assert!(
            !out.contains("\\\""),
            "unrolled json must not be escaped: {out}"
        );
    }

    #[test]
    fn string_mode_escapes_json_instead_of_nesting_it() {
        let cols = vec![col("prefs", TypeClass::Json { binary: true })];
        let row = vec![Value::Json(r#"{"a": 1}"#.into())];
        let out = encode(&cols, &row, JsonMode::String, false);
        assert_eq!(out, r#"{"prefs":"{\"a\": 1}"}"#);
    }

    #[test]
    fn both_json_modes_round_trip() {
        let cols = vec![col("prefs", TypeClass::Json { binary: true })];
        for mode in [JsonMode::Unroll, JsonMode::String] {
            let back = round_trip(&cols, &[Value::Json(r#"{"a":1}"#.into())], mode);
            let Value::Json(raw) = &back[0] else {
                panic!("expected json, got {:?}", back[0])
            };
            let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                parsed["a"],
                serde_json::json!(1),
                "mode {mode:?} lost the payload"
            );
        }
    }

    #[test]
    fn key_order_is_canonical_whatever_the_column_class() {
        let binary = vec![col("j", TypeClass::Json { binary: true })];
        let plain = vec![col("j", TypeClass::Json { binary: false })];
        let row = vec![Value::Json(r#"{"z":1,"a":2}"#.into())];

        // Both sort. Key order must not depend on which engine held the
        // column, or the same document re-exports differently after a
        // cross-engine load.
        assert_eq!(
            encode(&binary, &row, JsonMode::Unroll, false),
            r#"{"j":{"a":2,"z":1}}"#
        );
        assert_eq!(
            encode(&plain, &row, JsonMode::Unroll, false),
            r#"{"j":{"a":2,"z":1}}"#
        );
    }

    #[test]
    fn json_containing_a_bare_scalar_round_trips() {
        let cols = vec![col("j", TypeClass::Json { binary: true })];
        for raw in [
            "1",
            "true",
            "false",
            "null",
            r#""a string""#,
            "[]",
            "{}",
            "0.10",
        ] {
            let back = round_trip(&cols, &[Value::Json(raw.into())], JsonMode::Unroll);
            let Value::Json(out) = &back[0] else {
                panic!("{raw} came back as {:?}, not json", back[0])
            };
            let a: serde_json::Value = serde_json::from_str(raw).unwrap();
            let b: serde_json::Value = serde_json::from_str(out).unwrap();
            assert_eq!(a, b, "{raw} did not survive");
        }
    }

    #[test]
    fn a_json_null_payload_stays_distinct_from_sql_null() {
        // `'null'::jsonb` is not the same as a NULL jsonb column, and conflating
        // them on reload would silently rewrite the row. Unrolling a bare `null`
        // is what would cause that, so it must not happen.
        let cols = vec![col("j", TypeClass::Json { binary: true })];

        let sql_null = encode(&cols, &[Value::Null], JsonMode::Unroll, false);
        let json_null = encode(
            &cols,
            &[Value::Json("null".into())],
            JsonMode::Unroll,
            false,
        );
        assert_eq!(sql_null, r#"{"j":null}"#);
        assert_eq!(json_null, r#"{"j":"null"}"#);
        assert_ne!(sql_null, json_null, "the two nulls must not encode alike");

        assert_eq!(
            round_trip(&cols, &[Value::Null], JsonMode::Unroll),
            vec![Value::Null]
        );
        assert_eq!(
            round_trip(&cols, &[Value::Json("null".into())], JsonMode::Unroll),
            vec![Value::Json("null".into())]
        );
    }

    #[test]
    fn a_json_string_payload_stays_distinct_from_the_string_it_contains() {
        // `'"null"'::jsonb` (a JSON string) must not collide with `'null'::jsonb`
        // (the JSON null literal) either.
        let cols = vec![col("j", TypeClass::Json { binary: true })];
        let a = encode(
            &cols,
            &[Value::Json("null".into())],
            JsonMode::Unroll,
            false,
        );
        let b = encode(
            &cols,
            &[Value::Json(r#""null""#.into())],
            JsonMode::Unroll,
            false,
        );
        assert_ne!(a, b, "{a} collided with {b}");
        assert_eq!(
            round_trip(&cols, &[Value::Json(r#""null""#.into())], JsonMode::Unroll),
            vec![Value::Json(r#""null""#.into())]
        );
    }

    #[test]
    fn composite_payloads_still_unroll() {
        // The whole point of unroll mode: objects and arrays stay readable.
        let cols = vec![col("j", TypeClass::Json { binary: true })];
        for (raw, expected) in [
            (r#"{"a":1}"#, r#"{"j":{"a":1}}"#),
            ("[1,2]", r#"{"j":[1,2]}"#),
            (r#"[{"a":1}]"#, r#"{"j":[{"a":1}]}"#),
        ] {
            let out = encode(&cols, &[Value::Json(raw.into())], JsonMode::Unroll, false);
            // Nested, so no escaped quotes anywhere in the payload.
            assert_eq!(out, expected, "{raw} should be nested");
            assert!(!out.contains('\\'), "{raw} came out escaped: {out}");
        }
    }

    #[test]
    fn pretty_output_is_indented_and_nested() {
        let cols = vec![
            col("id", TypeClass::Int { bits: 32 }),
            col("prefs", TypeClass::Json { binary: true }),
        ];
        let row = vec![Value::Int(1), Value::Json(r#"{"a":[1,2]}"#.into())];
        let out = encode(&cols, &row, JsonMode::Unroll, true);
        assert_eq!(
            out,
            "{\n  \"id\": 1,\n  \"prefs\": {\n    \"a\": [\n      1,\n      2\n    ]\n  }\n}"
        );
    }

    #[test]
    fn string_escaping_covers_the_required_characters_only() {
        let mut s = String::new();
        encode_string(&mut s, "a\"b\\c\nd\te");
        assert_eq!(s, r#""a\"b\\c\nd\te""#);

        // Non-ASCII stays literal so diffs remain readable.
        let mut s = String::new();
        encode_string(&mut s, "héllo 🌱");
        assert_eq!(s, "\"héllo 🌱\"");

        // Other control characters take the \u form.
        let mut s = String::new();
        encode_string(&mut s, "\u{1}");
        assert_eq!(s, "\"\\u0001\"");
    }

    #[test]
    fn text_survives_a_round_trip_byte_for_byte() {
        let cols = vec![col("t", TypeClass::Text { max_len: None })];
        for s in [
            "",
            "a\nb",
            "quote\"",
            "back\\slash",
            "🌱",
            "\u{1}",
            "  spaced  ",
        ] {
            let back = round_trip(&cols, &[Value::Text(s.into())], JsonMode::Unroll);
            assert_eq!(back, vec![Value::Text(s.into())], "{s:?} was mangled");
        }
    }

    #[test]
    fn a_missing_column_decodes_as_null() {
        // A file written before a nullable column existed must still load.
        let cols = [
            col("id", TypeClass::Int { bits: 32 }),
            col("added_later", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        let row = decode_row(&refs, r#"{"id":1}"#, "test").unwrap();
        assert_eq!(row, vec![Value::Int(1), Value::Null]);
    }

    #[test]
    fn a_bool_and_an_integer_column_decode_each_other() {
        // A Postgres `boolean` exported as `true` loading into the SQLite or
        // MySQL integer column that stands in for it, and back again.
        let ints = [col("flag", TypeClass::Int { bits: 64 })];
        let refs: Vec<&Column> = ints.iter().collect();
        assert_eq!(
            decode_row(&refs, r#"{"flag":true}"#, "test").unwrap(),
            vec![Value::Int(1)]
        );
        assert_eq!(
            decode_row(&refs, r#"{"flag":false}"#, "test").unwrap(),
            vec![Value::Int(0)]
        );

        let bools = [col("flag", TypeClass::Bool)];
        let refs: Vec<&Column> = bools.iter().collect();
        assert_eq!(
            decode_row(&refs, r#"{"flag":1}"#, "test").unwrap(),
            vec![Value::Bool(true)]
        );
        assert_eq!(
            decode_row(&refs, r#"{"flag":0}"#, "test").unwrap(),
            vec![Value::Bool(false)]
        );
        // A number that is not 0 or 1 is not a boolean in disguise.
        assert!(decode_row(&refs, r#"{"flag":7}"#, "test").is_err());
    }

    #[test]
    fn an_unknown_column_is_a_clear_error_not_a_silent_drop() {
        let cols = [col("id", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = decode_row(&refs, r#"{"id":1,"ghost":2}"#, "users.jsonl:1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");
        assert!(
            err.contains("users.jsonl:1"),
            "the location must be named: {err}"
        );
    }

    #[test]
    fn a_non_object_line_is_rejected() {
        let cols = [col("id", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = decode_row(&refs, "[1,2]", "test").unwrap_err().to_string();
        assert!(err.contains("expected a json object"), "{err}");
    }

    #[test]
    fn every_scalar_class_round_trips_through_json() {
        let cases: Vec<(TypeClass, Value)> = vec![
            (TypeClass::Bool, Value::Bool(false)),
            (TypeClass::Int { bits: 64 }, Value::Int(i64::MIN)),
            (TypeClass::Float { bits: 64 }, Value::Float(0.1)),
            (
                TypeClass::Decimal {
                    precision: None,
                    scale: None,
                },
                Value::Decimal("0.0000".into()),
            ),
            (TypeClass::Text { max_len: None }, Value::Text("x".into())),
            (TypeClass::Bytes, Value::Bytes(vec![0, 255])),
            (
                TypeClass::Uuid,
                Value::Uuid("9f2c4b1e-7a3d-4e5f-8b9c-0d1e2f3a4b5c".parse().unwrap()),
            ),
            (
                TypeClass::Date,
                Value::parse(&TypeClass::Date, Some("2024-02-29")).unwrap(),
            ),
            (
                TypeClass::Timestamp { tz: true },
                Value::parse(
                    &TypeClass::Timestamp { tz: true },
                    Some("2024-01-01T00:00:00Z"),
                )
                .unwrap(),
            ),
            (TypeClass::Interval, Value::Raw("PT1H".into())),
            (
                TypeClass::Array {
                    of: Box::new(TypeClass::Int { bits: 32 }),
                },
                Value::Raw("{1,2}".into()),
            ),
            (
                TypeClass::Enum {
                    name: "tier".into(),
                },
                Value::Raw("pro".into()),
            ),
            (TypeClass::Text { max_len: None }, Value::Null),
        ];
        for (class, value) in cases {
            let cols = vec![col("c", class.clone())];
            let back = round_trip(&cols, std::slice::from_ref(&value), JsonMode::Unroll);
            assert_eq!(
                back,
                vec![value.clone()],
                "{} lost {value:?}",
                class.label()
            );
        }
    }

    // -- writers ------------------------------------------------------------

    fn table_with_pk(pk: &[&str], cols: &[Column]) -> Table {
        Table {
            id: crate::schema::TableId::new("public", "t"),
            columns: cols.to_vec(),
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    #[test]
    fn jsonl_writer_emits_one_line_per_row_with_a_trailing_newline() {
        let cols = [col("id", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(JsonlWriter::new(
            "t.jsonl".into(),
            refs,
            JsonMode::Unroll,
            false,
        ));
        w.write_row(&[Value::Int(1)]).unwrap();
        w.write_row(&[Value::Int(2)]).unwrap();
        let out = w.finish().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "t.jsonl");
        assert_eq!(out[0].bytes, b"{\"id\":1}\n{\"id\":2}\n");
    }

    #[test]
    fn a_table_with_no_rows_produces_an_empty_file() {
        let cols = [col("id", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let w = Box::new(JsonlWriter::new(
            "t.jsonl".into(),
            refs,
            JsonMode::Unroll,
            false,
        ));
        let out = w.finish().unwrap();
        assert!(
            out[0].bytes.is_empty(),
            "an empty table must not emit a blank line"
        );
    }

    #[test]
    fn per_row_writer_names_files_from_the_primary_key() {
        let cols = vec![
            col("id", TypeClass::Text { max_len: None }),
            col("body", TypeClass::Text { max_len: None }),
        ];
        let table = table_with_pk(&["id"], &cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(PerRowWriter::new(
            "t".into(),
            &table,
            refs,
            JsonMode::Unroll,
            true,
        ));
        w.write_row(&[
            Value::Text("Welcome Email".into()),
            Value::Text("hi".into()),
        ])
        .unwrap();
        let out = w.finish().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "t/welcome-email.json");
        assert_eq!(
            String::from_utf8(out[0].bytes.clone()).unwrap(),
            "{\n  \"id\": \"Welcome Email\",\n  \"body\": \"hi\"\n}\n"
        );
    }

    #[test]
    fn per_row_writer_falls_back_to_an_index_without_a_primary_key() {
        let cols = vec![col("x", TypeClass::Int { bits: 32 })];
        let table = table_with_pk(&[], &cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(PerRowWriter::new(
            "t".into(),
            &table,
            refs,
            JsonMode::Unroll,
            false,
        ));
        w.write_row(&[Value::Int(7)]).unwrap();
        w.write_row(&[Value::Int(8)]).unwrap();
        let out = w.finish().unwrap();
        assert_eq!(out[0].path, "t/000000.json");
        assert_eq!(out[1].path, "t/000001.json");
    }

    #[test]
    fn per_row_slug_collisions_get_a_suffix_rather_than_overwriting() {
        // Two distinct keys can sanitise to the same slug; losing a row to a
        // silent overwrite would be the worst possible outcome.
        let cols = vec![col("id", TypeClass::Text { max_len: None })];
        let table = table_with_pk(&["id"], &cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(PerRowWriter::new(
            "t".into(),
            &table,
            refs,
            JsonMode::Unroll,
            false,
        ));
        w.write_row(&[Value::Text("a/b".into())]).unwrap();
        w.write_row(&[Value::Text("a b".into())]).unwrap();
        w.write_row(&[Value::Text("a-b".into())]).unwrap();
        let out = w.finish().unwrap();
        let paths: Vec<&str> = out.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, ["t/a-b.json", "t/a-b-2.json", "t/a-b-3.json"]);
    }

    #[test]
    fn per_row_composite_keys_join_their_parts() {
        let cols = vec![
            col("a", TypeClass::Int { bits: 32 }),
            col("b", TypeClass::Int { bits: 32 }),
        ];
        let table = table_with_pk(&["a", "b"], &cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(PerRowWriter::new(
            "t".into(),
            &table,
            refs,
            JsonMode::Unroll,
            false,
        ));
        w.write_row(&[Value::Int(1), Value::Int(2)]).unwrap();
        assert_eq!(w.finish().unwrap()[0].path, "t/1-2.json");
    }
}

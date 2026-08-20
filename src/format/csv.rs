//! CSV with the Postgres `COPY ... CSV` NULL convention.
//!
//! CSV cannot natively distinguish `NULL` from the empty string, so an unquoted
//! empty field is `NULL` and a quoted one (`""`) is the empty string. That
//! distinction is invisible to a parser returning only decoded text, which is
//! why the writer and reader here are hand-rolled.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::format::{Output, RowWriter};
use crate::io::LineBuffer;
use crate::schema::Column;
use crate::value::Value;

const DELIMITER: char = ',';
const QUOTE: char = '"';

/// Render one field.
///
/// Quoting is applied only where it is required, plus the one case that carries
/// meaning: an empty string, which must be quoted to distinguish it from NULL.
pub fn encode_field(v: &Value) -> String {
    let Some(text) = v.to_text() else {
        // NULL: nothing at all, not even quotes.
        return String::new();
    };
    if needs_quoting(&text) {
        format!("{QUOTE}{}{QUOTE}", text.replace(QUOTE, "\"\""))
    } else {
        text
    }
}

fn needs_quoting(text: &str) -> bool {
    text.is_empty()
        || text.contains(DELIMITER)
        || text.contains(QUOTE)
        || text.contains('\n')
        || text.contains('\r')
        // Leading or trailing whitespace would be ambiguous to a reader that
        // trims, so pin it down explicitly.
        || text.starts_with(' ')
        || text.ends_with(' ')
}

/// Encode a whole record.
pub fn encode_record(fields: &[String]) -> String {
    fields.join(",")
}

/// Split one CSV line into `(text, was_quoted)` pairs.
///
/// `was_quoted` is what carries the NULL distinction, and is the reason this is
/// not a call into a CSV library.
pub fn parse_line(line: &str) -> Result<Vec<(String, bool)>> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();

    loop {
        let mut text = String::new();
        let mut quoted = false;

        if chars.peek() == Some(&QUOTE) {
            quoted = true;
            chars.next();
            loop {
                match chars.next() {
                    None => bail!("unterminated quoted field"),
                    Some(QUOTE) => {
                        if chars.peek() == Some(&QUOTE) {
                            chars.next();
                            text.push(QUOTE);
                        } else {
                            break;
                        }
                    }
                    Some(c) => text.push(c),
                }
            }
            match chars.peek() {
                None => {
                    fields.push((text, quoted));
                    break;
                }
                Some(&DELIMITER) => {
                    chars.next();
                    fields.push((text, quoted));
                    continue;
                }
                Some(c) => bail!("unexpected {c:?} after a closing quote"),
            }
        }

        let mut ended = true;
        for c in chars.by_ref() {
            if c == DELIMITER {
                ended = false;
                break;
            }
            if c == QUOTE {
                bail!("a bare quote may not appear in an unquoted field");
            }
            text.push(c);
        }
        fields.push((text, quoted));
        if ended {
            break;
        }
    }

    Ok(fields)
}

/// Turn a parsed field back into a value for `col`.
pub fn decode_field(col: &Column, text: &str, quoted: bool) -> Result<Value> {
    if text.is_empty() && !quoted {
        return Ok(Value::Null);
    }
    Value::parse(&col.class, Some(text))
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub struct CsvWriter<'a> {
    path: String,
    columns: Vec<&'a Column>,
    buf: LineBuffer,
}

impl<'a> CsvWriter<'a> {
    pub fn new(path: String, columns: Vec<&'a Column>) -> Self {
        let mut buf = LineBuffer::new();
        // Header from the lock's column order, so a reader can map by name.
        let header: Vec<String> = columns
            .iter()
            .map(|c| encode_field(&Value::Text(c.name.clone())))
            .collect();
        buf.push_line(&encode_record(&header));
        Self { path, columns, buf }
    }
}

impl RowWriter for CsvWriter<'_> {
    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        let fields: Vec<String> = row.iter().map(encode_field).collect();
        self.buf.push_line(&encode_record(&fields));
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vec<Output>> {
        let _ = &self.columns;
        Ok(vec![Output {
            path: self.path,
            bytes: self.buf.finish(),
        }])
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub fn read_csv(path: &Path, columns: &[&Column]) -> Result<Vec<Vec<Value>>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading seed file {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Records can span lines when a quoted field contains a newline.
    let records =
        split_records(&text).with_context(|| format!("splitting records in {}", path.display()))?;
    let mut iter = records.into_iter().enumerate();

    let Some((_, header_line)) = iter.next() else {
        // No header at all means no rows either; an empty table is legitimate.
        return Ok(Vec::new());
    };
    let header: Vec<String> = parse_line(&header_line)
        .with_context(|| format!("{name}:1: parsing the header"))?
        .into_iter()
        .map(|(t, _)| t)
        .collect();

    for h in &header {
        if !columns.iter().any(|c| c.name == *h) {
            bail!(
                "{name}:1: column {h:?} is in the seed file but not in the table; \
                 re-export after a schema change, or run `seedle lock`"
            );
        }
    }
    // Map each table column to its position in the file, if present. A column
    // absent from the header loads as NULL, matching the jsonl reader.
    let positions: Vec<Option<usize>> = columns
        .iter()
        .map(|c| header.iter().position(|h| *h == c.name))
        .collect();

    let mut rows = Vec::new();
    for (i, record) in iter {
        if record.trim().is_empty() {
            continue;
        }
        let line_no = i + 1;
        let fields = parse_line(&record).with_context(|| format!("{name}:{line_no}"))?;
        if fields.len() != header.len() {
            bail!(
                "{name}:{line_no}: row has {} fields but the header has {}",
                fields.len(),
                header.len()
            );
        }
        let row = columns
            .iter()
            .zip(&positions)
            .map(|(col, pos)| match pos {
                None => Ok(Value::Null),
                Some(p) => {
                    let (text, quoted) = &fields[*p];
                    decode_field(col, text, *quoted)
                        .with_context(|| format!("{name}:{line_no}: column {:?}", col.name))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        rows.push(row);
    }
    Ok(rows)
}

/// Split text into records, respecting newlines inside quoted fields.
fn split_records(text: &str) -> Result<Vec<String>> {
    let mut records = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            QUOTE => {
                if in_quotes && chars.peek() == Some(&QUOTE) {
                    current.push(QUOTE);
                    current.push(QUOTE);
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                    current.push(QUOTE);
                }
            }
            '\n' if !in_quotes => {
                records.push(std::mem::take(&mut current));
            }
            // A lone CR outside quotes is a line ending from another platform.
            '\r' if !in_quotes => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                records.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    if in_quotes {
        bail!("file ends inside a quoted field");
    }
    if !current.is_empty() {
        records.push(current);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::TypeClass;

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

    fn text_col() -> Column {
        col("t", TypeClass::Text { max_len: None })
    }

    /// Field -> text -> field, the property the NULL convention exists to give.
    fn round_trip(col: &Column, v: &Value) -> Value {
        let encoded = encode_field(v);
        let parsed = parse_line(&encoded).unwrap();
        assert_eq!(
            parsed.len(),
            1,
            "one field encoded into {} fields",
            parsed.len()
        );
        decode_field(col, &parsed[0].0, parsed[0].1).unwrap()
    }

    #[test]
    fn null_and_empty_string_are_distinguishable() {
        // This is the single property that makes CSV export non-lossy.
        assert_eq!(encode_field(&Value::Null), "");
        assert_eq!(encode_field(&Value::Text(String::new())), "\"\"");

        assert_eq!(round_trip(&text_col(), &Value::Null), Value::Null);
        assert_eq!(
            round_trip(&text_col(), &Value::Text(String::new())),
            Value::Text(String::new())
        );
    }

    #[test]
    fn fields_are_quoted_only_when_needed() {
        assert_eq!(encode_field(&Value::Text("plain".into())), "plain");
        assert_eq!(encode_field(&Value::Int(42)), "42");
        assert_eq!(encode_field(&Value::Text("a,b".into())), "\"a,b\"");
        assert_eq!(encode_field(&Value::Text("a\"b".into())), "\"a\"\"b\"");
        assert_eq!(encode_field(&Value::Text("a\nb".into())), "\"a\nb\"");
        // Surrounding spaces are meaningful, so they are pinned down by quotes.
        assert_eq!(encode_field(&Value::Text(" pad ".into())), "\" pad \"");
    }

    #[test]
    fn awkward_text_round_trips() {
        for s in [
            "plain",
            "",
            "a,b",
            "a\"b",
            "\"leading quote",
            "a\nb",
            "a\r\nb",
            " spaced ",
            "comma, and \"quote\"",
            "🌱",
            "\\N",
        ] {
            assert_eq!(
                round_trip(&text_col(), &Value::Text(s.into())),
                Value::Text(s.into()),
                "{s:?} did not survive"
            );
        }
    }

    #[test]
    fn the_literal_backslash_n_stays_text_not_null() {
        // Postgres COPY's *text* format uses \N for NULL; the CSV format does
        // not, so this must remain ordinary data.
        assert_eq!(
            round_trip(&text_col(), &Value::Text("\\N".into())),
            Value::Text("\\N".into())
        );
    }

    #[test]
    fn parses_records_into_fields() {
        assert_eq!(
            parse_line("a,b,c").unwrap(),
            vec![
                ("a".into(), false),
                ("b".into(), false),
                ("c".into(), false)
            ]
        );
        assert_eq!(
            parse_line("\"a\",,\"\"").unwrap(),
            vec![("a".into(), true), ("".into(), false), ("".into(), true)]
        );
        // A trailing delimiter means a final empty (NULL) field.
        assert_eq!(
            parse_line("a,").unwrap(),
            vec![("a".into(), false), ("".into(), false)]
        );
    }

    #[test]
    fn malformed_records_are_rejected_rather_than_guessed_at() {
        assert!(parse_line("\"unterminated").is_err());
        assert!(parse_line("a\"b").is_err(), "a bare quote is ambiguous");
        assert!(parse_line("\"a\"x").is_err(), "text after a closing quote");
    }

    #[test]
    fn records_may_span_lines_inside_quotes() {
        let text = "id,body\n1,\"line one\nline two\"\n2,\"plain\"\n";
        let records = split_records(text).unwrap();
        assert_eq!(records.len(), 3, "got {records:?}");
        assert_eq!(records[1], "1,\"line one\nline two\"");
    }

    #[test]
    fn crlf_line_endings_are_accepted_on_read() {
        let records = split_records("a\r\nb\r\n").unwrap();
        assert_eq!(records, vec!["a", "b"]);
    }

    #[test]
    fn unterminated_quote_at_eof_is_an_error() {
        assert!(split_records("a,\"oops\n").is_err());
    }

    #[test]
    fn writer_emits_a_header_from_the_column_order() {
        let cols = [
            col("id", TypeClass::Int { bits: 32 }),
            col("name", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(CsvWriter::new("t.csv".into(), refs));
        w.write_row(&[Value::Int(1), Value::Text("a".into())])
            .unwrap();
        w.write_row(&[Value::Int(2), Value::Null]).unwrap();
        let out = w.finish().unwrap();
        assert_eq!(
            String::from_utf8(out[0].bytes.clone()).unwrap(),
            "id,name\n1,a\n2,\n"
        );
    }

    #[test]
    fn file_round_trips_through_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");

        let cols = [
            col("id", TypeClass::Int { bits: 32 }),
            col("name", TypeClass::Text { max_len: None }),
            col("note", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        let rows = vec![
            vec![Value::Int(1), Value::Text("a".into()), Value::Null],
            vec![
                Value::Int(2),
                Value::Text(String::new()),
                Value::Text("x,y".into()),
            ],
            vec![
                Value::Int(3),
                Value::Text("multi\nline".into()),
                Value::Text("\"q\"".into()),
            ],
        ];

        let mut w = Box::new(CsvWriter::new("t.csv".into(), refs.clone()));
        for r in &rows {
            w.write_row(r).unwrap();
        }
        std::fs::write(&path, w.finish().unwrap()[0].bytes.clone()).unwrap();

        assert_eq!(read_csv(&path, &refs).unwrap(), rows);
    }

    #[test]
    fn reader_maps_by_header_name_not_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        // The file's columns are in a different order than the table's.
        std::fs::write(&path, "name,id\na,1\n").unwrap();

        let cols = [
            col("id", TypeClass::Int { bits: 32 }),
            col("name", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_csv(&path, &refs).unwrap(),
            vec![vec![Value::Int(1), Value::Text("a".into())]]
        );
    }

    #[test]
    fn a_column_missing_from_the_header_reads_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "id\n1\n").unwrap();

        let cols = [
            col("id", TypeClass::Int { bits: 32 }),
            col("added_later", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_csv(&path, &refs).unwrap(),
            vec![vec![Value::Int(1), Value::Null]]
        );
    }

    #[test]
    fn an_unknown_header_column_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "id,ghost\n1,2\n").unwrap();

        let cols = [col("id", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = read_csv(&path, &refs).unwrap_err().to_string();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn a_short_row_is_an_error_not_a_silent_null_fill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "a,b\n1\n").unwrap();

        let cols = [
            col("a", TypeClass::Int { bits: 32 }),
            col("b", TypeClass::Int { bits: 32 }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = read_csv(&path, &refs).unwrap_err().to_string();
        assert!(
            err.contains("1 fields") && err.contains("header has 2"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_file_yields_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "").unwrap();
        let cols = [col("a", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        assert!(read_csv(&path, &refs).unwrap().is_empty());
    }

    #[test]
    fn header_only_file_yields_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "a\n").unwrap();
        let cols = [col("a", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        assert!(read_csv(&path, &refs).unwrap().is_empty());
    }
}

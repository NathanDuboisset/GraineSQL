//! Batched `INSERT` statements, runnable through `psql` or `mysql`.
//!
//! The reader accepts the shape this writer emits and rejects anything else,
//! rather than being a general SQL parser. Escaping rules come from the engine
//! recorded in the lock, never from sniffing the file: real payloads contain
//! backticks and backslashes.

use anyhow::{Context, Result, bail};

use crate::config::{Engine, ResolvedTable};
use crate::dialect::Dialect;
use crate::format::{Output, RowWriter};
use crate::io::LineBuffer;
use crate::schema::{Column, Table};
use crate::value::Value;

pub struct SqlWriter<'a> {
    path: String,
    columns: Vec<&'a Column>,
    dialect: &'a dyn Dialect,
    /// `INSERT INTO "t" ("a", "b") VALUES`, computed once.
    header: String,
    /// `ON CONFLICT ...`, appended after the last tuple of each batch.
    conflict: String,
    batch_size: usize,
    batch: Vec<String>,
    buf: LineBuffer,
}

impl<'a> SqlWriter<'a> {
    pub fn new(
        path: String,
        table: &Table,
        columns: Vec<&'a Column>,
        cfg: &ResolvedTable,
        dialect: &'a dyn Dialect,
        batch_size: usize,
    ) -> Result<Self> {
        let key: Vec<String> = cfg
            .key
            .clone()
            .or_else(|| table.upsert_key().map(|k| k.to_vec()))
            .unwrap_or_default();
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let header = format!(
            "{} {} ({}) VALUES",
            dialect.insert_verb(cfg.load_mode),
            dialect.quote_table(&table.id),
            columns
                .iter()
                .map(|c| dialect.quote_ident(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let conflict = dialect.conflict_clause(table, &key, &names, cfg.load_mode)?;

        Ok(Self {
            path,
            columns,
            dialect,
            header,
            conflict,
            batch_size: batch_size.max(1),
            batch: Vec::new(),
            buf: LineBuffer::new(),
        })
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        self.buf.push_line(&self.header);
        let last = self.batch.len() - 1;
        for (i, tuple) in self.batch.iter().enumerate() {
            let terminator = if i == last {
                format!("{};", self.conflict)
            } else {
                ",".into()
            };
            self.buf.push_line(&format!("  {tuple}{terminator}"));
        }
        self.batch.clear();
    }
}

impl RowWriter for SqlWriter<'_> {
    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        let literals: Vec<String> = self
            .columns
            .iter()
            .zip(row)
            .map(|(col, v)| self.dialect.literal(col, v))
            .collect::<Result<Vec<_>>>()?;
        self.batch.push(format!("({})", literals.join(", ")));
        if self.batch.len() >= self.batch_size {
            self.flush();
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<Output>> {
        self.flush();
        Ok(vec![Output {
            path: self.path,
            bytes: self.buf.finish(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Format, Layout, LoadMode};
    use crate::dialect::{Mysql, Postgres};
    use crate::schema::{TableId, TypeClass};

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

    fn table(cols: &[Column]) -> Table {
        Table {
            id: TableId::new("public", "users"),
            columns: cols.to_vec(),
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    fn cfg(mode: LoadMode) -> ResolvedTable {
        ResolvedTable {
            id: TableId::new("public", "users"),
            config_key: "users".into(),
            filter: None,
            order_by: vec![],
            limit: None,
            columns: None,
            exclude_columns: vec![],
            format: Format::Sql,
            layout: Layout::Single,
            pretty: false,
            json: crate::config::JsonMode::Unroll,
            load_mode: mode,
            key: None,
        }
    }

    fn render(dialect: &dyn Dialect, mode: LoadMode, batch: usize, rows: &[Vec<Value>]) -> String {
        let cols = vec![
            col("id", TypeClass::Int { bits: 32 }),
            col("name", TypeClass::Text { max_len: None }),
        ];
        let t = table(&cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(
            SqlWriter::new("users.sql".into(), &t, refs, &cfg(mode), dialect, batch).unwrap(),
        );
        for r in rows {
            w.write_row(r).unwrap();
        }
        String::from_utf8(w.finish().unwrap()[0].bytes.clone()).unwrap()
    }

    fn two_rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Null],
        ]
    }

    #[test]
    fn emits_one_statement_per_batch() {
        let out = render(&Postgres, LoadMode::Insert, 100, &two_rows());
        assert_eq!(
            out,
            "INSERT INTO \"public\".\"users\" (\"id\", \"name\") VALUES\n  \
             (1, 'a'::text),\n  (2, NULL);\n"
        );
    }

    #[test]
    fn batching_splits_into_separate_statements() {
        let out = render(&Postgres, LoadMode::Insert, 1, &two_rows());
        assert_eq!(out.matches("INSERT INTO").count(), 2);
        assert_eq!(out.matches(';').count(), 2);
    }

    #[test]
    fn the_conflict_clause_lands_once_per_statement_at_the_end() {
        let out = render(&Postgres, LoadMode::Upsert, 100, &two_rows());
        assert_eq!(out.matches("ON CONFLICT").count(), 1);
        assert!(
            out.contains(
                "(2, NULL) ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\";"
            ),
            "{out}"
        );
        // And it must not appear after a non-final tuple.
        assert!(!out.contains("'a'::text) ON CONFLICT"), "{out}");
    }

    #[test]
    fn each_batch_gets_its_own_conflict_clause() {
        let out = render(&Postgres, LoadMode::Upsert, 1, &two_rows());
        assert_eq!(out.matches("ON CONFLICT").count(), 2);
    }

    #[test]
    fn mysql_uses_its_own_verb_and_clause() {
        let out = render(&Mysql, LoadMode::Upsert, 100, &two_rows());
        assert!(
            out.starts_with("INSERT INTO `public`.`users` (`id`, `name`) VALUES"),
            "{out}"
        );
        assert!(
            out.contains("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"),
            "{out}"
        );

        let out = render(&Mysql, LoadMode::SkipExisting, 100, &two_rows());
        assert!(out.starts_with("INSERT IGNORE INTO"), "{out}");
        assert!(!out.contains("ON DUPLICATE"), "{out}");
    }

    #[test]
    fn hostile_text_is_escaped_inside_the_literal() {
        let rows = vec![vec![
            Value::Int(1),
            Value::Text("'); DROP TABLE users; --".into()),
        ]];
        let out = render(&Postgres, LoadMode::Insert, 10, &rows);
        assert!(out.contains("'''); DROP TABLE users; --'::text"), "{out}");
        // The two semicolons inside the payload plus the statement's own, and a
        // single INSERT: nothing escaped the literal to become new SQL.
        assert_eq!(out.matches(';').count(), 3, "{out}");
        assert_eq!(out.matches("INSERT INTO").count(), 1);
        assert_eq!(
            out.matches("DROP TABLE").count(),
            1,
            "still just data: {out}"
        );
    }

    #[test]
    fn no_rows_produces_an_empty_file_not_a_dangling_insert() {
        let out = render(&Postgres, LoadMode::Insert, 10, &[]);
        assert!(out.is_empty(), "expected nothing, got {out:?}");
    }

    #[test]
    fn upsert_without_a_key_fails_at_construction_not_mid_file() {
        let cols = vec![col("a", TypeClass::Int { bits: 32 })];
        let mut t = table(&cols);
        t.primary_key.clear();
        let refs: Vec<&Column> = cols.iter().collect();
        let err = match SqlWriter::new(
            "t.sql".into(),
            &t,
            refs,
            &cfg(LoadMode::Upsert),
            &Postgres,
            10,
        ) {
            Ok(_) => panic!("an upsert with no key must not construct"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no primary key"), "{err}");
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Read back the `INSERT` statements this module writes.
///
/// Not a SQL parser: it accepts the shape [`SqlWriter`] emits and rejects
/// anything else rather than guessing. That is enough to round-trip GraineSQL's own
/// output, and it fails loudly on a hand-written file instead of silently
/// misreading it.
pub fn read_sql(
    path: &std::path::Path,
    columns: &[&Column],
    engine: Engine,
) -> Result<Vec<Vec<Value>>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading seed file {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // The dialects disagree about the backslash: MySQL escapes with it, Postgres
    // treats it as an ordinary character, so `'a\\b'` means different things.
    // The engine comes from the lock rather than from sniffing the file, whose
    // data can contain anything.
    let esc = match engine.dialect() {
        Engine::Mysql => Escaping::Backslash,
        _ => Escaping::QuoteOnly,
    };

    let mut rows = Vec::new();
    for stmt in split_statements(&text, esc) {
        let Some((header, values)) = split_insert(&stmt.text) else {
            bail!(
                "{name}:{}: expected an INSERT ... VALUES statement, which is all GraineSQL \
                 writes and all it reads back",
                stmt.line
            );
        };
        let names = parse_column_list(&header)
            .with_context(|| format!("{name}:{}: reading the column list", stmt.line))?;

        // Map file columns onto table columns by name, as the other readers do.
        for n in &names {
            if !columns.iter().any(|c| c.name == *n) {
                bail!(
                    "{name}:{}: column {n:?} is in the seed file but not in the table",
                    stmt.line
                );
            }
        }

        for tuple in split_tuples(&values, &name, stmt.line, esc)? {
            let literals = split_literals(&tuple, &name, stmt.line, esc)?;
            if literals.len() != names.len() {
                bail!(
                    "{name}:{}: a row has {} values but the column list has {}",
                    stmt.line,
                    literals.len(),
                    names.len()
                );
            }
            let row = columns
                .iter()
                .map(|col| match names.iter().position(|n| n == &col.name) {
                    None => Ok(Value::Null),
                    Some(i) => parse_literal(col, &literals[i], esc)
                        .with_context(|| format!("{name}:{}: column {:?}", stmt.line, col.name)),
                })
                .collect::<Result<Vec<_>>>()?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// How the file that wrote these literals escapes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escaping {
    /// Postgres: only a doubled quote escapes; a backslash is literal.
    QuoteOnly,
    /// MySQL: a doubled quote or a backslash escape.
    Backslash,
}

struct Statement {
    text: String,
    line: usize,
}

/// Advance past a quoted literal's contents, honouring the escaping rules.
///
/// `chars` must be positioned just after the opening quote. Returns the raw
/// body, still escaped.
fn take_literal<I>(chars: &mut std::iter::Peekable<I>, esc: Escaping) -> (String, bool)
where
    I: Iterator<Item = char>,
{
    let mut body = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                if chars.peek() == Some(&'\'') {
                    body.push_str("''");
                    chars.next();
                } else {
                    return (body, true);
                }
            }
            '\\' if esc == Escaping::Backslash => {
                body.push('\\');
                if let Some(next) = chars.next() {
                    body.push(next);
                }
            }
            c => body.push(c),
        }
    }
    (body, false)
}

/// Split on semicolons that are not inside a quoted literal or a comment.
fn split_statements(text: &str, esc: Escaping) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut line = 1usize;
    let mut start_line = 1usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\n' {
            line += 1;
        }
        // A line comment runs to the end of the line.
        if c == '-' && chars.peek() == Some(&'-') {
            for skipped in chars.by_ref() {
                if skipped == '\n' {
                    line += 1;
                    break;
                }
            }
            current.push(' ');
            continue;
        }
        match c {
            '\'' => {
                let (body, _) = take_literal(&mut chars, esc);
                line += body.matches('\n').count();
                current.push('\'');
                current.push_str(&body);
                current.push('\'');
            }
            ';' => {
                if !current.trim().is_empty() {
                    out.push(Statement {
                        text: current.trim().to_string(),
                        line: start_line,
                    });
                }
                current.clear();
                start_line = line;
            }
            c => {
                if current.trim().is_empty() && !c.is_whitespace() {
                    start_line = line;
                }
                current.push(c);
            }
        }
    }
    if !current.trim().is_empty() {
        out.push(Statement {
            text: current.trim().to_string(),
            line: start_line,
        });
    }
    out
}

/// Split `INSERT INTO t (a, b) VALUES (...)...` into the column list and the
/// tuples that follow.
fn split_insert(stmt: &str) -> Option<(String, String)> {
    let upper = stmt.to_ascii_uppercase();
    if !upper.trim_start().starts_with("INSERT") {
        return None;
    }
    let values_at = find_keyword(&upper, "VALUES")?;
    let head = &stmt[..values_at];
    let open = head.rfind('(')?;
    let close = head[open..].find(')')? + open;

    // A conflict clause carries its own parentheses, which would otherwise be
    // read as another row.
    let tail = &stmt[values_at + "VALUES".len()..];
    let tail_upper = &upper[values_at + "VALUES".len()..];
    let end = ["ON CONFLICT", "ON DUPLICATE", "RETURNING"]
        .iter()
        .filter_map(|kw| find_keyword(tail_upper, kw))
        .min()
        .unwrap_or(tail.len());

    Some((head[open + 1..close].to_string(), tail[..end].to_string()))
}

/// Find a keyword outside any quoted literal.
fn find_keyword(upper: &str, word: &str) -> Option<usize> {
    let bytes = upper.as_bytes();
    let mut in_quotes = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => in_quotes = !in_quotes,
            _ if !in_quotes && upper[i..].starts_with(word) => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_column_list(list: &str) -> Result<Vec<String>> {
    list.split(',')
        .map(|part| {
            let p = part.trim();
            let unquoted = p
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .map(|inner| inner.replace("\"\"", "\""))
                .or_else(|| {
                    p.strip_prefix('`')
                        .and_then(|r| r.strip_suffix('`'))
                        .map(|inner| inner.replace("``", "`"))
                })
                .unwrap_or_else(|| p.to_string());
            if unquoted.is_empty() {
                bail!("empty column name in ({list})");
            }
            Ok(unquoted)
        })
        .collect()
}

/// Split the `(..), (..)` tail of a VALUES clause into its tuples.
fn split_tuples(values: &str, file: &str, line: usize, esc: Escaping) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut chars = values.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let (body, closed) = take_literal(&mut chars, esc);
                if !closed {
                    bail!("{file}:{line}: a literal is not closed");
                }
                current.push('\'');
                current.push_str(&body);
                current.push('\'');
            }
            '(' => {
                depth += 1;
                if depth > 1 {
                    current.push(c);
                }
            }
            ')' => {
                depth -= 1;
                match depth {
                    0 => out.push(std::mem::take(&mut current)),
                    d if d < 0 => bail!("{file}:{line}: unbalanced parentheses"),
                    _ => current.push(c),
                }
            }
            c if depth > 0 => current.push(c),
            // Between tuples: the separator, or a conflict clause.
            _ => {}
        }
    }
    if depth != 0 {
        bail!("{file}:{line}: statement ends inside a parenthesis");
    }
    Ok(out)
}

/// Split one tuple into its literals, respecting quotes and nesting.
fn split_literals(tuple: &str, file: &str, line: usize, esc: Escaping) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut chars = tuple.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let (body, closed) = take_literal(&mut chars, esc);
                if !closed {
                    bail!("{file}:{line}: a literal is not closed");
                }
                current.push('\'');
                current.push_str(&body);
                current.push('\'');
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    out.push(current);
    Ok(out.into_iter().map(|s| s.trim().to_string()).collect())
}

/// Turn one SQL literal back into a value.
fn parse_literal(col: &Column, literal: &str, esc: Escaping) -> Result<Value> {
    let lit = literal.trim();
    if lit.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if lit.eq_ignore_ascii_case("true") {
        return Ok(Value::Bool(true));
    }
    if lit.eq_ignore_ascii_case("false") {
        return Ok(Value::Bool(false));
    }

    // MySQL binary literal.
    if let Some(hex) = lit
        .strip_prefix("X'")
        .or_else(|| lit.strip_prefix("x'"))
        .and_then(|r| r.strip_suffix('\''))
    {
        return Value::parse(&col.class, Some(hex));
    }

    if lit.starts_with('\'') {
        // A quoted literal, optionally followed by ::type.
        let mut chars = lit.chars().skip(1).peekable();
        let (body, closed) = take_literal(&mut chars, esc);
        if !closed {
            bail!("unterminated literal {}", truncate(lit));
        }
        return Value::parse(&col.class, Some(&unescape(&body, esc)));
    }

    // A bare number, or something with a cast we can strip.
    let bare = lit.split("::").next().unwrap_or(lit).trim();
    Value::parse(&col.class, Some(bare))
}

/// Undo the escaping applied inside a literal.
fn unescape(body: &str, esc: Escaping) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if chars.peek() == Some(&'\'') => {
                out.push('\'');
                chars.next();
            }
            '\\' if esc == Escaping::Backslash => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('0') => out.push('\0'),
                Some('Z') => out.push('\u{1a}'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= 60 {
        return s.to_string();
    }
    s.chars().take(60).collect()
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use crate::config::{Format, JsonMode, Layout, LoadMode};
    use crate::dialect::{Mysql, Postgres};
    use crate::schema::{TableId, TypeClass};

    fn col(name: &str, sql_type: &str, class: TypeClass) -> Column {
        Column {
            name: name.into(),
            sql_type: sql_type.into(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn wide() -> Vec<Column> {
        vec![
            col("id", "integer", TypeClass::Int { bits: 32 }),
            col("name", "text", TypeClass::Text { max_len: None }),
            col(
                "amount",
                "numeric(14,4)",
                TypeClass::Decimal {
                    precision: Some(14),
                    scale: Some(4),
                },
            ),
            col("ok", "boolean", TypeClass::Bool),
            col("blob", "bytea", TypeClass::Bytes),
            col(
                "tags",
                "text[]",
                TypeClass::Array {
                    of: Box::new(TypeClass::Text { max_len: None }),
                },
            ),
            col("at", "timestamptz", TypeClass::Timestamp { tz: true }),
        ]
    }

    fn cfg(mode: LoadMode) -> ResolvedTable {
        ResolvedTable {
            id: TableId::new("public", "t"),
            config_key: "t".into(),
            filter: None,
            order_by: vec![],
            limit: None,
            columns: None,
            exclude_columns: vec![],
            format: Format::Sql,
            layout: Layout::Single,
            pretty: false,
            json: JsonMode::Unroll,
            load_mode: mode,
            key: None,
        }
    }

    /// Write rows out and read them straight back.
    fn round_trip(
        dialect: &dyn Dialect,
        mode: LoadMode,
        batch: usize,
        rows: &[Vec<Value>],
    ) -> Vec<Vec<Value>> {
        let engine = dialect.engine();
        let cols = wide();
        let t = Table {
            id: TableId::new("public", "t"),
            columns: cols.clone(),
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        };
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(
            SqlWriter::new("t.sql".into(), &t, refs.clone(), &cfg(mode), dialect, batch).unwrap(),
        );
        for r in rows {
            w.write_row(r).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, w.finish().unwrap()[0].bytes.clone()).unwrap();
        read_sql(&path, &refs, engine).unwrap()
    }

    fn sample() -> Vec<Vec<Value>> {
        vec![
            vec![
                Value::Int(1),
                Value::Text("plain".into()),
                Value::Decimal("19.9900".into()),
                Value::Bool(true),
                Value::Bytes(vec![0xde, 0xad, 0x00, 0xff]),
                Value::Raw("{a,b}".into()),
                Value::parse(
                    &TypeClass::Timestamp { tz: true },
                    Some("2024-01-01T12:00:00Z"),
                )
                .unwrap(),
            ],
            vec![
                Value::Int(2),
                Value::Text("it's got, a comma; and 'quotes'".into()),
                Value::Decimal("-0.0001".into()),
                Value::Bool(false),
                Value::Bytes(vec![]),
                Value::Raw("{}".into()),
                Value::Null,
            ],
            vec![
                Value::Int(3),
                Value::Text("multi\nline\twith \\backslash".into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    }

    #[test]
    fn postgres_sql_round_trips() {
        assert_eq!(
            round_trip(&Postgres, LoadMode::Insert, 100, &sample()),
            sample()
        );
    }

    #[test]
    fn mysql_sql_round_trips() {
        // MySQL escapes backslashes, which is the case most likely to break.
        assert_eq!(
            round_trip(&Mysql, LoadMode::Insert, 100, &sample()),
            sample()
        );
    }

    #[test]
    fn round_trip_survives_batching_and_conflict_clauses() {
        for batch in [1, 2, 100] {
            for mode in [LoadMode::Insert, LoadMode::Upsert, LoadMode::SkipExisting] {
                assert_eq!(
                    round_trip(&Postgres, mode, batch, &sample()),
                    sample(),
                    "batch {batch}, mode {mode:?}"
                );
            }
        }
    }

    /// The escaping rule must come from the engine, never from the file's
    /// content: real payloads contain backticks and backslashes, and sniffing
    /// for them picks the wrong dialect and mangles every escape.
    #[test]
    fn content_that_looks_like_another_dialect_does_not_change_the_escaping() {
        let cols = vec![
            col("id", "integer", TypeClass::Int { bits: 32 }),
            col("doc", "jsonb", TypeClass::Json { binary: true }),
        ];
        let t = Table {
            id: TableId::new("public", "t"),
            columns: cols.clone(),
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        };
        let refs: Vec<&Column> = cols.iter().collect();

        // Backticks and backslash escapes inside the payload, as markdown-ish
        // prose in a jsonb column produces.
        let payload = r#"{"note":"see `field_name`","text":"line\nbreak \\ done"}"#;
        let rows = [vec![Value::Int(1), Value::Json(payload.into())]];

        let mut w = Box::new(
            SqlWriter::new(
                "t.sql".into(),
                &t,
                refs.clone(),
                &cfg(LoadMode::Insert),
                &Postgres,
                10,
            )
            .unwrap(),
        );
        w.write_row(&rows[0]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, w.finish().unwrap()[0].bytes.clone()).unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains('`'),
            "the fixture must contain a backtick to be meaningful"
        );

        let back = read_sql(&path, &refs, Engine::Postgres).unwrap();
        let Value::Json(got) = &back[0][1] else {
            panic!("expected json, got {:?}", back[0][1])
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(got).unwrap(),
            serde_json::from_str::<serde_json::Value>(payload).unwrap()
        );
    }

    #[test]
    fn an_empty_file_yields_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, "").unwrap();
        let cols = wide();
        let refs: Vec<&Column> = cols.iter().collect();
        assert!(read_sql(&path, &refs, Engine::Postgres).unwrap().is_empty());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(
            &path,
            "-- a comment with a ; semicolon and an 'apostrophe\n\n\
             INSERT INTO \"t\" (\"id\") VALUES\n  (1),\n  (2);\n\
             -- trailing comment\n",
        )
        .unwrap();
        let cols = [col("id", "integer", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_sql(&path, &refs, Engine::Postgres).unwrap(),
            vec![vec![Value::Int(1)], vec![Value::Int(2)]]
        );
    }

    #[test]
    fn a_semicolon_inside_a_literal_does_not_split_the_statement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(
            &path,
            "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'a;b'), (2, 'c;d');\n",
        )
        .unwrap();
        let cols = [
            col("id", "integer", TypeClass::Int { bits: 32 }),
            col("name", "text", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_sql(&path, &refs, Engine::Postgres).unwrap(),
            vec![
                vec![Value::Int(1), Value::Text("a;b".into())],
                vec![Value::Int(2), Value::Text("c;d".into())],
            ]
        );
    }

    #[test]
    fn a_column_missing_from_the_file_reads_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, "INSERT INTO \"t\" (\"id\") VALUES (7);\n").unwrap();
        let cols = [
            col("id", "integer", TypeClass::Int { bits: 32 }),
            col("later", "text", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_sql(&path, &refs, Engine::Postgres).unwrap(),
            vec![vec![Value::Int(7), Value::Null]]
        );
    }

    #[test]
    fn a_non_insert_statement_is_refused_rather_than_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, "DELETE FROM \"t\";\n").unwrap();
        let cols = [col("id", "integer", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = read_sql(&path, &refs, Engine::Postgres)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected an INSERT"), "{err}");
    }

    #[test]
    fn an_unknown_column_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, "INSERT INTO \"t\" (\"ghost\") VALUES (1);\n").unwrap();
        let cols = [col("id", "integer", TypeClass::Int { bits: 32 })];
        let refs: Vec<&Column> = cols.iter().collect();
        assert!(
            read_sql(&path, &refs, Engine::Postgres)
                .unwrap_err()
                .to_string()
                .contains("ghost")
        );
    }

    #[test]
    fn a_wrong_value_count_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sql");
        std::fs::write(&path, "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1);\n").unwrap();
        let cols = [
            col("id", "integer", TypeClass::Int { bits: 32 }),
            col("name", "text", TypeClass::Text { max_len: None }),
        ];
        let refs: Vec<&Column> = cols.iter().collect();
        let err = read_sql(&path, &refs, Engine::Postgres)
            .unwrap_err()
            .to_string();
        assert!(err.contains("1 values") && err.contains("has 2"), "{err}");
    }
}

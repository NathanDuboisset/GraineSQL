//! Per-engine SQL generation.
//!
//! Everything in this module is synchronous and pure, so the quoting, escaping,
//! and clause-building rules, the parts most likely to be subtly wrong, are
//! unit-testable without a database.

use anyhow::{Result, bail};

use crate::config::{Engine, LoadMode};
use crate::schema::{Column, Table, TableId, TypeClass};
use crate::value::Value;

pub trait Dialect: Send + Sync {
    fn engine(&self) -> Engine;

    /// Quote an identifier, escaping the quote character by doubling it.
    fn quote_ident(&self, name: &str) -> String;

    /// Bind-parameter placeholder. `n` is 1-based.
    fn placeholder(&self, n: usize) -> String;

    /// Session settings applied on connect to make text output deterministic.
    fn session_setup(&self) -> &'static [&'static str];

    /// Query returning the server version, for `seedle sources`.
    fn version_query(&self) -> &'static str {
        "SELECT version()"
    }

    /// Expression that reads `col` as canonical text.
    fn read_expr(&self, col: &Column) -> String;

    /// Expression to sort by.
    ///
    /// Text columns get an explicit binary collation: the default collation is a
    /// per-database property, so without pinning it two databases holding
    /// identical rows would order them differently and produce different files.
    fn order_expr(&self, col: &Column) -> String;

    /// Placeholder wrapped so the engine parses our text back into the column's
    /// type. `n` is 1-based.
    fn write_expr(&self, col: &Column, n: usize) -> String;

    /// The text handed to the driver for a bind parameter. `None` is NULL.
    fn bind_text(&self, col: &Column, v: &Value) -> Result<Option<String>>;

    /// A self-contained SQL literal for `v`, for the `.sql` export format.
    fn literal(&self, col: &Column, v: &Value) -> Result<String>;

    /// `INSERT` keyword sequence, which MySQL varies to express "skip existing".
    fn insert_verb(&self, mode: LoadMode) -> &'static str;

    /// Trailing conflict clause, empty when the mode needs none.
    fn conflict_clause(
        &self,
        table: &Table,
        key: &[String],
        insert_cols: &[String],
        mode: LoadMode,
    ) -> Result<String>;

    fn quote_table(&self, id: &TableId) -> String {
        match &id.schema {
            Some(s) => format!("{}.{}", self.quote_ident(s), self.quote_ident(&id.name)),
            None => self.quote_ident(&id.name),
        }
    }

    /// Session statement that suspends foreign-key checking, when the engine
    /// can, for loading a set of tables whose keys form a cycle.
    ///
    /// `all_deferrable` says whether every constraint in the cycle is
    /// `DEFERRABLE`, which is the only case Postgres can satisfy.
    fn defer_constraints(&self, all_deferrable: bool) -> Option<&'static str> {
        let _ = all_deferrable;
        None
    }

    /// Undo [`Dialect::defer_constraints`], where it outlives the transaction.
    fn restore_constraints(&self) -> Option<&'static str> {
        None
    }

    /// Whether sequence fixup has to wait until after the commit.
    fn fixup_after_commit(&self) -> bool {
        false
    }

    /// Statements that advance identity sequences past the loaded keys.
    fn sequence_fixups(&self, table: &Table) -> Vec<String> {
        let _ = table;
        Vec::new()
    }

    /// Bulk-load statement taking a CSV stream on stdin, when the engine has
    /// one. `None` means fall back to one INSERT per row.
    ///
    /// Only usable for modes with no conflict handling, since a bulk load has
    /// nowhere to put an ON CONFLICT clause.
    fn copy_in_statement(&self, table: &Table, columns: &[&Column]) -> Option<String> {
        let _ = (table, columns);
        None
    }

    /// Statement that empties a table.
    ///
    /// Deliberately `DELETE FROM` rather than `TRUNCATE` on both engines:
    /// Postgres refuses to truncate a table that any foreign key references,
    /// even when the referencing table is empty, and MySQL's `TRUNCATE` performs
    /// an implicit commit that would silently break the load's atomicity.
    fn delete_all(&self, id: &TableId) -> String {
        format!("DELETE FROM {}", self.quote_table(id))
    }
}

pub fn for_engine(engine: Engine) -> Box<dyn Dialect> {
    match engine.dialect() {
        Engine::Mysql => Box::new(Mysql),
        Engine::Sqlite => Box::new(Sqlite),
        _ => Box::new(Postgres),
    }
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

pub struct Postgres;

impl Dialect for Postgres {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn session_setup(&self) -> &'static [&'static str] {
        // Every one of these pins a setting that would otherwise make the text
        // representation of a value depend on server or role configuration.
        &[
            "SET TimeZone = 'UTC'",
            "SET DateStyle = 'ISO, MDY'",
            "SET IntervalStyle = 'iso_8601'",
            "SET bytea_output = 'hex'",
            "SET extra_float_digits = 3",
        ]
    }

    fn read_expr(&self, col: &Column) -> String {
        // A uniform `::text` cast works for every Postgres type and, with the
        // session above, yields a canonical representation for all of them.
        format!("{}::text", self.quote_ident(&col.name))
    }

    fn order_expr(&self, col: &Column) -> String {
        let q = self.quote_ident(&col.name);
        match &col.class {
            // "C" collation is byte order: available in every Postgres install
            // and identical everywhere, unlike the database default.
            TypeClass::Text { .. } | TypeClass::Enum { .. } => format!("{q} COLLATE \"C\""),
            _ => q,
        }
    }

    fn write_expr(&self, col: &Column, n: usize) -> String {
        // `sql_type` comes from `format_type()`, which is exactly a castable
        // type expression, including quoting for enums that need it.
        format!("CAST(${n} AS {})", col.sql_type)
    }

    fn bind_text(&self, _col: &Column, v: &Value) -> Result<Option<String>> {
        // Postgres accepts our canonical text for every type verbatim: `\x...`
        // is its hex bytea input format, `true`/`false` are valid booleans, and
        // ISO timestamps parse unambiguously.
        Ok(v.to_text())
    }

    fn literal(&self, col: &Column, v: &Value) -> Result<String> {
        Ok(match v {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Decimal(d) if is_bare_numeric_literal(d) => d.clone(),
            // Postgres `numeric` accepts NaN, but `NaN` is not a numeric
            // literal, it has to be quoted and cast like a non-finite float.
            Value::Decimal(d) => format!("{}::{}", pg_quote(d), col.sql_type),
            Value::Float(f) if f.is_finite() => crate::value::format_float(*f),
            // NaN and the infinities are valid Postgres floats but are not
            // numeric literals, so they need a quoted cast.
            Value::Float(f) => format!(
                "{}::{}",
                pg_quote(&crate::value::format_float(*f)),
                col.sql_type
            ),
            Value::Bytes(_) => {
                let text = v.to_text().expect("bytes are never null here");
                format!("{}::bytea", pg_quote(&text))
            }
            // Everything else is a quoted literal cast to the column's type,
            // which covers uuid, json, temporal types, enums, and arrays with
            // one code path.
            _ => {
                let text = v.to_text().expect("non-null value has text");
                format!("{}::{}", pg_quote(&text), col.sql_type)
            }
        })
    }

    fn insert_verb(&self, _mode: LoadMode) -> &'static str {
        "INSERT INTO"
    }

    fn defer_constraints(&self, all_deferrable: bool) -> Option<&'static str> {
        // Postgres errors rather than ignoring a request to defer a constraint
        // that is not DEFERRABLE.
        all_deferrable.then_some("SET CONSTRAINTS ALL DEFERRED")
    }

    fn copy_in_statement(&self, table: &Table, columns: &[&Column]) -> Option<String> {
        // COPY's CSV dialect is the one seedle already writes: an unquoted
        // empty field is NULL and a quoted one is the empty string.
        Some(format!(
            "COPY {} ({}) FROM STDIN WITH (FORMAT csv)",
            self.quote_table(&table.id),
            columns
                .iter()
                .map(|c| self.quote_ident(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    fn sequence_fixups(&self, table: &Table) -> Vec<String> {
        table
            .identity_columns()
            .map(|c| crate::db::postgres::fix_sequence_sql(&table.id, &c.name))
            .collect()
    }

    fn conflict_clause(
        &self,
        table: &Table,
        key: &[String],
        insert_cols: &[String],
        mode: LoadMode,
    ) -> Result<String> {
        match mode {
            LoadMode::Insert | LoadMode::TruncateFirst => Ok(String::new()),
            LoadMode::SkipExisting => Ok(" ON CONFLICT DO NOTHING".to_string()),
            LoadMode::Upsert => {
                if key.is_empty() {
                    bail!(
                        "table {} has no primary key or unique constraint, so `upsert` has \
                         nothing to conflict on; set `load: insert` or `key: [...]` for it",
                        table.id
                    );
                }
                let target = key
                    .iter()
                    .map(|c| self.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let updates: Vec<String> = insert_cols
                    .iter()
                    .filter(|c| !key.contains(c))
                    .map(|c| {
                        let q = self.quote_ident(c);
                        format!("{q} = EXCLUDED.{q}")
                    })
                    .collect();
                if updates.is_empty() {
                    // A key-only table has nothing to update; upsert degenerates
                    // to "ensure present".
                    Ok(format!(" ON CONFLICT ({target}) DO NOTHING"))
                } else {
                    Ok(format!(
                        " ON CONFLICT ({target}) DO UPDATE SET {}",
                        updates.join(", ")
                    ))
                }
            }
        }
    }
}

/// Single-quoted SQL string literal, escaping only the quote.
///
/// Correct for Postgres under `standard_conforming_strings = on` (the default
/// since 9.1) and for SQLite, in both of which a backslash inside a plain
/// literal is an ordinary character. MySQL escapes with backslashes too and
/// needs [`mysql_quote`].
pub(crate) fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn pg_quote(s: &str) -> String {
    quote_literal(s)
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

pub struct Mysql;

impl Dialect for Mysql {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }

    fn quote_ident(&self, name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }

    fn placeholder(&self, _n: usize) -> String {
        "?".to_string()
    }

    fn session_setup(&self) -> &'static [&'static str] {
        &[
            "SET time_zone = '+00:00'",
            // Reject silent truncation and out-of-range coercion rather than
            // writing a corrupted value.
            "SET sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION'",
        ]
    }

    fn read_expr(&self, col: &Column) -> String {
        let q = self.quote_ident(&col.name);
        match &col.class {
            // CAST(... AS CHAR) on a binary column would mangle non-UTF-8 bytes.
            TypeClass::Bytes => format!("HEX({q})"),
            _ => format!("CAST({q} AS CHAR)"),
        }
    }

    fn write_expr(&self, col: &Column, _n: usize) -> String {
        match &col.class {
            TypeClass::Bytes => "UNHEX(?)".to_string(),
            _ => "?".to_string(),
        }
    }

    fn order_expr(&self, col: &Column) -> String {
        let q = self.quote_ident(&col.name);
        match &col.class {
            // MySQL's default collation is case- and accent-insensitive, which
            // leaves ties between rows that differ only in case.
            TypeClass::Text { .. } | TypeClass::Enum { .. } => {
                format!("{q} COLLATE utf8mb4_bin")
            }
            _ => q,
        }
    }

    fn bind_text(&self, col: &Column, v: &Value) -> Result<Option<String>> {
        Ok(match v {
            Value::Null => None,
            // MySQL has no boolean type; BOOLEAN is TINYINT(1) and will not
            // coerce the strings "true"/"false".
            Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            // UNHEX() wants bare hex, without our canonical `\x` marker.
            Value::Bytes(b) => Some(crate::value::hex_encode_upper(b)),
            // MySQL's DATETIME parser wants a space separator and no zone
            // suffix; the session is pinned to UTC so dropping `Z` is safe.
            Value::Timestamp(_) | Value::TimestampTz(_) => {
                let text = v.to_text().expect("non-null");
                Some(text.trim_end_matches('Z').replace('T', " "))
            }
            Value::Float(f) if !f.is_finite() => bail!(
                "column {} holds {} but MySQL cannot store non-finite floats",
                col.name,
                crate::value::format_float(*f)
            ),
            _ => v.to_text(),
        })
    }

    fn literal(&self, col: &Column, v: &Value) -> Result<String> {
        Ok(match v {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Decimal(d) if is_bare_numeric_literal(d) => d.clone(),
            Value::Decimal(d) => bail!(
                "column {} holds the numeric value {d}, which MySQL cannot represent",
                col.name
            ),
            Value::Float(f) if f.is_finite() => crate::value::format_float(*f),
            Value::Float(f) => bail!(
                "column {} holds {} but MySQL cannot store non-finite floats",
                col.name,
                crate::value::format_float(*f)
            ),
            Value::Bytes(b) if b.is_empty() => "''".to_string(),
            Value::Bytes(b) => format!("X'{}'", crate::value::hex_encode_upper(b)),
            _ => {
                let text = self
                    .bind_text(col, v)?
                    .expect("non-null value has bind text");
                mysql_quote(&text)
            }
        })
    }

    fn insert_verb(&self, mode: LoadMode) -> &'static str {
        match mode {
            // MySQL expresses "skip existing" on the verb, not in a clause.
            LoadMode::SkipExisting => "INSERT IGNORE INTO",
            _ => "INSERT INTO",
        }
    }

    fn defer_constraints(&self, _all_deferrable: bool) -> Option<&'static str> {
        // Transaction-scoped, and works regardless of how the keys were declared.
        Some("SET FOREIGN_KEY_CHECKS = 0")
    }

    fn restore_constraints(&self) -> Option<&'static str> {
        Some("SET FOREIGN_KEY_CHECKS = 1")
    }

    fn fixup_after_commit(&self) -> bool {
        // ALTER TABLE commits implicitly, which would split the load in two.
        true
    }

    fn conflict_clause(
        &self,
        table: &Table,
        key: &[String],
        insert_cols: &[String],
        mode: LoadMode,
    ) -> Result<String> {
        match mode {
            LoadMode::Insert | LoadMode::TruncateFirst | LoadMode::SkipExisting => {
                Ok(String::new())
            }
            LoadMode::Upsert => {
                if key.is_empty() {
                    bail!(
                        "table {} has no primary key or unique constraint, so `upsert` has \
                         nothing to conflict on; set `load: insert` or `key: [...]` for it",
                        table.id
                    );
                }
                let updates: Vec<String> = insert_cols
                    .iter()
                    .filter(|c| !key.contains(c))
                    .map(|c| {
                        let q = self.quote_ident(c);
                        format!("{q} = VALUES({q})")
                    })
                    .collect();
                if updates.is_empty() {
                    // Nothing to update, so a no-op assignment is the only way
                    // to express "ignore the duplicate" in this clause form.
                    let q = self.quote_ident(&key[0]);
                    return Ok(format!(" ON DUPLICATE KEY UPDATE {q} = {q}"));
                }
                Ok(format!(" ON DUPLICATE KEY UPDATE {}", updates.join(", ")))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

pub struct Sqlite;

impl Dialect for Sqlite {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn placeholder(&self, n: usize) -> String {
        format!("?{n}")
    }

    fn session_setup(&self) -> &'static [&'static str] {
        // Foreign keys are off by default, so a load would silently accept
        // orphans without this.
        &["PRAGMA foreign_keys = ON"]
    }

    fn version_query(&self) -> &'static str {
        "SELECT sqlite_version()"
    }

    fn read_expr(&self, col: &Column) -> String {
        let q = self.quote_ident(&col.name);
        match &col.class {
            // CAST(... AS TEXT) on a blob reinterprets the bytes, and hex(NULL)
            // returns an empty string rather than NULL, which would turn a null
            // blob into an empty one.
            TypeClass::Bytes => format!("CASE WHEN {q} IS NULL THEN NULL ELSE hex({q}) END"),
            _ => format!("CAST({q} AS TEXT)"),
        }
    }

    fn write_expr(&self, col: &Column, n: usize) -> String {
        match &col.class {
            TypeClass::Bytes => format!("unhex(?{n})"),
            _ => format!("?{n}"),
        }
    }

    fn bind_text(&self, col: &Column, v: &Value) -> Result<Option<String>> {
        Ok(match v {
            Value::Null => None,
            // No boolean type: 0 and 1, as every SQLite client expects.
            Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            Value::Bytes(b) => Some(crate::value::hex_encode_upper(b)),
            Value::Float(f) if !f.is_finite() => bail!(
                "column {} holds {} but SQLite cannot store non-finite floats",
                col.name,
                crate::value::format_float(*f)
            ),
            _ => v.to_text(),
        })
    }

    fn literal(&self, col: &Column, v: &Value) -> Result<String> {
        Ok(match v {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Decimal(d) if is_bare_numeric_literal(d) => d.clone(),
            Value::Float(f) if f.is_finite() => crate::value::format_float(*f),
            Value::Float(f) => bail!(
                "column {} holds {} but SQLite cannot store non-finite floats",
                col.name,
                crate::value::format_float(*f)
            ),
            Value::Bytes(b) if b.is_empty() => "x''".to_string(),
            Value::Bytes(b) => format!("x'{}'", crate::value::hex_encode_upper(b)),
            other => {
                let text = other.to_text().expect("null was handled above");
                quote_literal(&text)
            }
        })
    }

    fn insert_verb(&self, mode: LoadMode) -> &'static str {
        match mode {
            LoadMode::SkipExisting => "INSERT OR IGNORE INTO",
            _ => "INSERT INTO",
        }
    }

    fn defer_constraints(&self, _all_deferrable: bool) -> Option<&'static str> {
        Some("PRAGMA defer_foreign_keys = ON")
    }

    fn sequence_fixups(&self, table: &Table) -> Vec<String> {
        table
            .identity_columns()
            .map(|c| crate::db::sqlite::fix_sequence_sql(&table.id, &c.name))
            .collect()
    }

    fn conflict_clause(
        &self,
        table: &Table,
        key: &[String],
        insert_cols: &[String],
        mode: LoadMode,
    ) -> Result<String> {
        match mode {
            LoadMode::Insert | LoadMode::TruncateFirst | LoadMode::SkipExisting => {
                Ok(String::new())
            }
            LoadMode::Upsert => {
                if key.is_empty() {
                    bail!(
                        "table {} has no primary key or unique constraint, so `upsert` has \
                         nothing to conflict on; set `load: insert` or `key: [...]` for it",
                        table.id
                    );
                }
                let target = key
                    .iter()
                    .map(|c| self.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let updates: Vec<String> = insert_cols
                    .iter()
                    .filter(|c| !key.contains(c))
                    .map(|c| {
                        let q = self.quote_ident(c);
                        format!("{q} = excluded.{q}")
                    })
                    .collect();
                if updates.is_empty() {
                    return Ok(format!(" ON CONFLICT ({target}) DO NOTHING"));
                }
                Ok(format!(
                    " ON CONFLICT ({target}) DO UPDATE SET {}",
                    updates.join(", ")
                ))
            }
        }
    }

    fn order_expr(&self, col: &Column) -> String {
        let q = self.quote_ident(&col.name);
        match &col.class {
            // BINARY is the default, but a column declared COLLATE NOCASE would
            // otherwise order case-insensitively and leave ties.
            TypeClass::Text { .. } => format!("{q} COLLATE BINARY"),
            _ => q,
        }
    }
}

/// Single-quoted MySQL string literal.
///
/// MySQL treats backslash as an escape character in string literals (unless
/// `NO_BACKSLASH_ESCAPES` is set), so every backslash must be doubled, the
/// opposite of the Postgres rule.
fn mysql_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            // Ctrl-Z terminates input on Windows clients if left raw.
            '\u{1a}' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Whether a decimal's digits form a bare SQL numeric literal.
///
/// Everything a database emits for a decimal does, except `NaN`, which Postgres
/// accepts as a `numeric` value but not as a literal token.
fn is_bare_numeric_literal(s: &str) -> bool {
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    !body.is_empty()
        && body.bytes().any(|b| b.is_ascii_digit())
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::TypeClass;

    fn col(name: &str, sql_type: &str, class: TypeClass) -> Column {
        Column {
            name: name.to_string(),
            sql_type: sql_type.to_string(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn text_col() -> Column {
        col("c", "text", TypeClass::Text { max_len: None })
    }

    fn table(pk: &[&str], cols: &[&str]) -> Table {
        Table {
            id: TableId::new("public", "users"),
            columns: cols.iter().map(|c| text_col_named(c)).collect(),
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    fn text_col_named(name: &str) -> Column {
        col(name, "text", TypeClass::Text { max_len: None })
    }

    // -- identifier quoting -------------------------------------------------

    #[test]
    fn postgres_quotes_and_escapes_identifiers() {
        let d = Postgres;
        assert_eq!(d.quote_ident("users"), "\"users\"");
        assert_eq!(d.quote_ident("MixedCase"), "\"MixedCase\"");
        // A quote inside an identifier must be doubled, not dropped.
        assert_eq!(d.quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            d.quote_table(&TableId::new("audit", "log")),
            "\"audit\".\"log\""
        );
        assert_eq!(d.quote_table(&TableId::bare("log")), "\"log\"");
    }

    #[test]
    fn mysql_quotes_and_escapes_identifiers() {
        let d = Mysql;
        assert_eq!(d.quote_ident("users"), "`users`");
        assert_eq!(d.quote_ident("we`ird"), "`we``ird`");
        assert_eq!(d.quote_table(&TableId::new("app", "log")), "`app`.`log`");
    }

    #[test]
    fn identifier_quoting_neutralises_injection_attempts() {
        let hostile = "users\"; DROP TABLE users; --";
        let pg = Postgres.quote_ident(hostile);
        // The injected quote is doubled, so the whole thing stays one identifier.
        assert_eq!(pg, "\"users\"\"; DROP TABLE users; --\"");
        assert_eq!(pg.matches('"').count() % 2, 0, "quotes must stay balanced");

        let my = Mysql.quote_ident("users`; DROP TABLE users; --");
        assert_eq!(my, "`users``; DROP TABLE users; --`");
    }

    // -- literal escaping ---------------------------------------------------

    #[test]
    fn postgres_literals_escape_quotes_and_leave_backslashes_alone() {
        // Under standard_conforming_strings a backslash is a literal character.
        assert_eq!(pg_quote("plain"), "'plain'");
        assert_eq!(pg_quote("it's"), "'it''s'");
        assert_eq!(pg_quote("back\\slash"), "'back\\slash'");
        assert_eq!(
            pg_quote("'; DROP TABLE users; --"),
            "'''; DROP TABLE users; --'"
        );
    }

    #[test]
    fn mysql_literals_escape_backslashes_too() {
        assert_eq!(mysql_quote("plain"), "'plain'");
        assert_eq!(mysql_quote("it's"), "'it\\'s'");
        // The critical difference from Postgres: a lone backslash before the
        // closing quote would otherwise escape it and break out of the literal.
        assert_eq!(mysql_quote("trail\\"), "'trail\\\\'");
        assert_eq!(mysql_quote("a\nb"), "'a\\nb'");
        assert_eq!(mysql_quote("\u{1a}"), "'\\Z'");
    }

    #[test]
    fn injection_shaped_text_stays_inside_the_literal() {
        for hostile in [
            "'; DROP TABLE users; --",
            "\\'; DROP TABLE users; --",
            "') ; DELETE FROM users WHERE ('1'='1",
            "\u{0}',1)--",
        ] {
            let d = Postgres;
            let lit = d
                .literal(&text_col(), &Value::Text(hostile.to_string()))
                .unwrap();
            assert!(lit.starts_with("'") && lit.contains("::text"), "{lit}");
            // Count unescaped quotes: every interior quote must be doubled.
            let body = &lit[1..lit.rfind("'").unwrap()];
            assert_eq!(
                body.matches("''").count() * 2 + body.matches('\'').count() % 2,
                body.matches('\'').count(),
                "unbalanced quoting in {lit}"
            );

            let my = Mysql
                .literal(&text_col(), &Value::Text(hostile.to_string()))
                .unwrap();
            assert!(my.starts_with('\'') && my.ends_with('\''), "{my}");
        }
    }

    // -- typed literals -----------------------------------------------------

    #[test]
    fn postgres_typed_literals() {
        let d = Postgres;
        assert_eq!(d.literal(&text_col(), &Value::Null).unwrap(), "NULL");
        assert_eq!(
            d.literal(&col("b", "boolean", TypeClass::Bool), &Value::Bool(true))
                .unwrap(),
            "TRUE"
        );
        assert_eq!(
            d.literal(
                &col("i", "integer", TypeClass::Int { bits: 32 }),
                &Value::Int(-5)
            )
            .unwrap(),
            "-5"
        );
        // Decimals go in bare so no precision is lost to a float round trip.
        assert_eq!(
            d.literal(
                &col(
                    "n",
                    "numeric",
                    TypeClass::Decimal {
                        precision: None,
                        scale: None
                    }
                ),
                &Value::Decimal("1.5000".into())
            )
            .unwrap(),
            "1.5000"
        );
        assert_eq!(
            d.literal(
                &col("z", "bytea", TypeClass::Bytes),
                &Value::Bytes(vec![0xde, 0xad])
            )
            .unwrap(),
            "'\\xdead'::bytea"
        );
        // An enum literal carries the cast that makes it typed.
        assert_eq!(
            d.literal(
                &col(
                    "t",
                    "tier",
                    TypeClass::Enum {
                        name: "tier".into()
                    }
                ),
                &Value::Raw("pro".into())
            )
            .unwrap(),
            "'pro'::tier"
        );
        assert_eq!(
            d.literal(
                &col(
                    "a",
                    "integer[]",
                    TypeClass::Array {
                        of: Box::new(TypeClass::Int { bits: 32 })
                    }
                ),
                &Value::Raw("{1,2}".into())
            )
            .unwrap(),
            "'{1,2}'::integer[]"
        );
    }

    #[test]
    fn a_numeric_nan_is_quoted_and_cast_not_emitted_bare() {
        // `NaN` bare is a syntax error; the generated .sql has to be runnable.
        let c = col(
            "n",
            "numeric(38,10)",
            TypeClass::Decimal {
                precision: Some(38),
                scale: Some(10),
            },
        );
        assert_eq!(
            Postgres.literal(&c, &Value::Decimal("NaN".into())).unwrap(),
            "'NaN'::numeric(38,10)"
        );
        // Ordinary decimals stay bare so no precision is lost to a cast.
        assert_eq!(
            Postgres
                .literal(&c, &Value::Decimal("1.5000".into()))
                .unwrap(),
            "1.5000"
        );
        assert_eq!(
            Postgres
                .literal(&c, &Value::Decimal("-0.0001".into()))
                .unwrap(),
            "-0.0001"
        );

        // MySQL has no numeric NaN at all, so it refuses rather than corrupting.
        let err = Mysql
            .literal(&c, &Value::Decimal("NaN".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot represent"), "{err}");
    }

    #[test]
    fn postgres_non_finite_floats_get_a_quoted_cast() {
        let d = Postgres;
        let c = col("f", "double precision", TypeClass::Float { bits: 64 });
        assert_eq!(
            d.literal(&c, &Value::Float(f64::NAN)).unwrap(),
            "'NaN'::double precision"
        );
        assert_eq!(
            d.literal(&c, &Value::Float(f64::INFINITY)).unwrap(),
            "'Infinity'::double precision"
        );
        assert_eq!(d.literal(&c, &Value::Float(1.5)).unwrap(), "1.5");
    }

    #[test]
    fn mysql_rejects_non_finite_floats_rather_than_corrupting_them() {
        let c = col("f", "double", TypeClass::Float { bits: 64 });
        let err = Mysql
            .literal(&c, &Value::Float(f64::NAN))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot store non-finite"), "{err}");
        assert!(Mysql.bind_text(&c, &Value::Float(f64::INFINITY)).is_err());
    }

    #[test]
    fn mysql_binds_booleans_as_integers() {
        let c = col("b", "tinyint(1)", TypeClass::Bool);
        assert_eq!(
            Mysql.bind_text(&c, &Value::Bool(true)).unwrap().unwrap(),
            "1"
        );
        assert_eq!(
            Mysql.bind_text(&c, &Value::Bool(false)).unwrap().unwrap(),
            "0"
        );
        // Postgres takes the canonical spelling directly.
        assert_eq!(
            Postgres.bind_text(&c, &Value::Bool(true)).unwrap().unwrap(),
            "true"
        );
    }

    #[test]
    fn mysql_strips_the_iso_marker_from_timestamps() {
        let c = col("t", "datetime", TypeClass::Timestamp { tz: true });
        let v = Value::parse(
            &TypeClass::Timestamp { tz: true },
            Some("2024-01-01T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            Mysql.bind_text(&c, &v).unwrap().unwrap(),
            "2024-01-01 12:00:00"
        );
        // Postgres parses the ISO form as-is.
        assert_eq!(
            Postgres.bind_text(&c, &v).unwrap().unwrap(),
            "2024-01-01T12:00:00Z"
        );
    }

    #[test]
    fn mysql_binds_bytes_as_bare_hex_for_unhex() {
        let c = col("z", "blob", TypeClass::Bytes);
        assert_eq!(
            Mysql
                .bind_text(&c, &Value::Bytes(vec![0xde, 0xad]))
                .unwrap()
                .unwrap(),
            "DEAD"
        );
        assert_eq!(Mysql.write_expr(&c, 1), "UNHEX(?)");
        assert_eq!(Mysql.read_expr(&c), "HEX(`z`)");
    }

    // -- read/write expressions --------------------------------------------

    #[test]
    fn postgres_reads_and_writes_through_text() {
        let d = Postgres;
        let c = col(
            "created_at",
            "timestamp with time zone",
            TypeClass::Timestamp { tz: true },
        );
        assert_eq!(d.read_expr(&c), "\"created_at\"::text");
        assert_eq!(d.write_expr(&c, 3), "CAST($3 AS timestamp with time zone)");
    }

    #[test]
    fn text_ordering_pins_the_collation_on_both_engines() {
        // Collation is a per-database property, so leaving it implicit means two
        // databases with the same rows can order them differently.
        let t = col("name", "text", TypeClass::Text { max_len: None });
        assert_eq!(Postgres.order_expr(&t), "\"name\" COLLATE \"C\"");
        assert_eq!(Mysql.order_expr(&t), "`name` COLLATE utf8mb4_bin");

        // Non-text columns must not carry a COLLATE clause; it is a type error.
        let i = col("id", "integer", TypeClass::Int { bits: 32 });
        assert_eq!(Postgres.order_expr(&i), "\"id\"");
        assert_eq!(Mysql.order_expr(&i), "`id`");
        let ts = col("at", "timestamptz", TypeClass::Timestamp { tz: true });
        assert!(!Postgres.order_expr(&ts).contains("COLLATE"));
    }

    #[test]
    fn session_setup_only_sets_session_settable_parameters() {
        // lc_collate and friends are fixed at initdb time; trying to SET one
        // fails the connection.
        for stmt in Postgres.session_setup() {
            assert!(
                !stmt.contains("lc_collate") && !stmt.contains("lc_ctype"),
                "{stmt} is not settable per session"
            );
        }
    }

    #[test]
    fn placeholders_are_numbered_only_where_the_engine_needs_it() {
        assert_eq!(Postgres.placeholder(1), "$1");
        assert_eq!(Postgres.placeholder(12), "$12");
        assert_eq!(Mysql.placeholder(12), "?");
    }

    // -- conflict clauses ---------------------------------------------------

    #[test]
    fn postgres_upsert_updates_every_non_key_column() {
        let t = table(&["id"], &["id", "email", "name"]);
        let cols: Vec<String> = ["id", "email", "name"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let clause = Postgres
            .conflict_clause(&t, &["id".to_string()], &cols, LoadMode::Upsert)
            .unwrap();
        assert_eq!(
            clause,
            " ON CONFLICT (\"id\") DO UPDATE SET \"email\" = EXCLUDED.\"email\", \
             \"name\" = EXCLUDED.\"name\""
        );
    }

    #[test]
    fn postgres_upsert_on_a_key_only_table_degenerates_to_do_nothing() {
        let t = table(&["a", "b"], &["a", "b"]);
        let cols: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let key: Vec<String> = cols.clone();
        let clause = Postgres
            .conflict_clause(&t, &key, &cols, LoadMode::Upsert)
            .unwrap();
        assert_eq!(clause, " ON CONFLICT (\"a\", \"b\") DO NOTHING");
    }

    #[test]
    fn mysql_upsert_uses_on_duplicate_key() {
        let t = table(&["id"], &["id", "email"]);
        let cols: Vec<String> = ["id", "email"].iter().map(|s| s.to_string()).collect();
        let clause = Mysql
            .conflict_clause(&t, &["id".to_string()], &cols, LoadMode::Upsert)
            .unwrap();
        assert_eq!(clause, " ON DUPLICATE KEY UPDATE `email` = VALUES(`email`)");
    }

    #[test]
    fn skip_existing_differs_structurally_between_engines() {
        let t = table(&["id"], &["id"]);
        let cols = vec!["id".to_string()];
        // Postgres puts it in a clause...
        assert_eq!(Postgres.insert_verb(LoadMode::SkipExisting), "INSERT INTO");
        assert_eq!(
            Postgres
                .conflict_clause(&t, &cols, &cols, LoadMode::SkipExisting)
                .unwrap(),
            " ON CONFLICT DO NOTHING"
        );
        // ...MySQL puts it on the verb.
        assert_eq!(
            Mysql.insert_verb(LoadMode::SkipExisting),
            "INSERT IGNORE INTO"
        );
        assert_eq!(
            Mysql
                .conflict_clause(&t, &cols, &cols, LoadMode::SkipExisting)
                .unwrap(),
            ""
        );
    }

    #[test]
    fn plain_insert_has_no_conflict_clause() {
        let t = table(&["id"], &["id"]);
        let cols = vec!["id".to_string()];
        for mode in [LoadMode::Insert, LoadMode::TruncateFirst] {
            assert_eq!(
                Postgres.conflict_clause(&t, &cols, &cols, mode).unwrap(),
                ""
            );
            assert_eq!(Mysql.conflict_clause(&t, &cols, &cols, mode).unwrap(), "");
        }
    }

    #[test]
    fn upsert_without_a_key_is_a_clear_error() {
        let t = table(&[], &["a"]);
        let cols = vec!["a".to_string()];
        for d in [&Postgres as &dyn Dialect, &Mysql] {
            let err = d
                .conflict_clause(&t, &[], &cols, LoadMode::Upsert)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no primary key"), "{err}");
            assert!(
                err.contains("load: insert"),
                "error should name the fix: {err}"
            );
        }
    }

    #[test]
    fn emptying_a_table_avoids_truncate_on_both_engines() {
        // Postgres refuses TRUNCATE on an FK target; MySQL's TRUNCATE commits.
        let id = TableId::new("public", "users");
        assert_eq!(Postgres.delete_all(&id), "DELETE FROM \"public\".\"users\"");
        assert_eq!(
            Mysql.delete_all(&TableId::bare("users")),
            "DELETE FROM `users`"
        );
    }
}

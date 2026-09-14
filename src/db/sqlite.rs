//! SQLite introspection via `PRAGMA`.
//!
//! SQLite differs from the server engines in ways that matter here:
//!
//! - No schemas. Every table lives in `main`, so [`SCHEMA`] stands in for one.
//! - Declared types are advisory. `table_info` reports whatever the DDL said,
//!   and the affinity rules decide what is actually stored, so classification
//!   works on affinity, matching what SQLite itself does.
//! - No native boolean, date, or uuid; those are integers and text.
//! - Foreign keys are off unless `PRAGMA foreign_keys` is on, and are never
//!   deferrable in the Postgres sense.

use anyhow::{Context, Result};
use indexmap::IndexMap;

use crate::db::{Db, Fields};
use crate::dialect::Dialect as _;
use crate::schema::{Column, ForeignKey, Schema, Table, TableId, TypeClass, UniqueKey};

/// Stands in for a schema name, since SQLite has none.
pub const SCHEMA: &str = "main";

/// User tables, excluding SQLite's own bookkeeping.
const TABLES_SQL: &str = "
SELECT name FROM sqlite_master
 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
 ORDER BY name
";

/// Tables declared AUTOINCREMENT, which are the only ones with a
/// `sqlite_sequence` row to correct after a load.
const AUTOINC_SQL: &str = "
SELECT name FROM sqlite_master
 WHERE type = 'table' AND sql LIKE '%AUTOINCREMENT%'
";

pub async fn introspect(db: &Db) -> Result<Schema> {
    let mut tables: IndexMap<TableId, Table> = IndexMap::new();

    let autoinc: std::collections::BTreeSet<String> = db
        .query_text(AUTOINC_SQL)
        .await
        .context("listing AUTOINCREMENT tables")?
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .collect();

    for id in list_tables(db).await? {
        let quoted = crate::dialect::Sqlite.quote_ident(&id.name);
        let mut table = Table {
            id: id.clone(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            unique: Vec::new(),
            foreign_keys: Vec::new(),
        };

        // `pk` is the 1-based position within the primary key, 0 when not part
        // of it, so a composite key keeps its declared order.
        let mut pk: Vec<(i64, String)> = Vec::new();
        for row in db
            .query_text(&format!("PRAGMA table_info({quoted})"))
            .await
            .with_context(|| format!("reading columns of {id}"))?
        {
            let f = Fields::new(&row, 6, "table_info")?;
            let name = f.text(1)?.to_string();
            let declared = f.text(2)?.to_string();
            let notnull = f.text(3)? == "1";
            let default = f.opt(4);
            let pk_pos: i64 = f.text(5)?.parse().unwrap_or(0);

            if pk_pos > 0 {
                pk.push((pk_pos, name.clone()));
            }
            table.columns.push(Column {
                class: classify(&declared),
                sql_type: declared,
                nullable: !notnull,
                has_default: default.is_some(),
                generated: false,
                identity: false,
                name,
            });
        }
        pk.sort_by_key(|(pos, _)| *pos);
        table.primary_key = pk.into_iter().map(|(_, n)| n).collect();

        // An INTEGER PRIMARY KEY aliases the implicit rowid, so it fills itself
        // in. Only an AUTOINCREMENT table keeps a sqlite_sequence row, and only
        // that needs correcting after a load, so `identity` is reserved for it.
        if table.primary_key.len() == 1 {
            let key = table.primary_key[0].clone();
            if let Some(c) = table.columns.iter_mut().find(|c| c.name == key)
                && c.sql_type.trim().eq_ignore_ascii_case("integer")
            {
                c.has_default = true;
                c.identity = autoinc.contains(&id.name);
            }
        }

        for row in db
            .query_text(&format!("PRAGMA index_list({quoted})"))
            .await
            .with_context(|| format!("reading indexes of {id}"))?
        {
            let f = Fields::new(&row, 5, "index_list")?;
            if f.text(2)? != "1" {
                continue;
            }
            let index = f.text(1)?.to_string();
            let cols: Vec<String> = db
                .query_text(&format!(
                    "PRAGMA index_info({})",
                    crate::dialect::Sqlite.quote_ident(&index)
                ))
                .await
                .with_context(|| format!("reading index {index}"))?
                .iter()
                .filter_map(|r| r.get(2).cloned().flatten())
                .collect();

            // An expression index cannot be a conflict target.
            if cols.is_empty() || cols.len() != table_index_width(db, &index).await? {
                continue;
            }
            // Column 4 flags a partial index. Skip it if the predicate cannot
            // be recovered: `ON CONFLICT` without it names nothing the engine
            // will match.
            let predicate = if f.text(4)? == "1" {
                match index_predicate(db, &index).await? {
                    Some(p) => Some(p),
                    None => continue,
                }
            } else {
                None
            };
            let key = UniqueKey {
                columns: cols,
                predicate,
            };
            if key.columns != table.primary_key && !table.unique.contains(&key) {
                table.unique.push(key);
            }
        }

        // `id` groups the columns of one composite key; `seq` orders them.
        let mut fks: IndexMap<String, ForeignKey> = IndexMap::new();
        for row in db
            .query_text(&format!("PRAGMA foreign_key_list({quoted})"))
            .await
            .with_context(|| format!("reading foreign keys of {id}"))?
        {
            let f = Fields::new(&row, 8, "foreign_key_list")?;
            let group = f.text(0)?.to_string();
            let parent = TableId::new(SCHEMA, f.text(2)?);
            let from = f.text(3)?.to_string();
            // A null `to` means the key targets the parent's primary key.
            let to = match f.opt(4) {
                Some(t) => t.to_string(),
                None => tables
                    .get(&parent)
                    .and_then(|p| p.primary_key.first().cloned())
                    .unwrap_or_else(|| "rowid".to_string()),
            };

            let entry = fks.entry(group.clone()).or_insert_with(|| ForeignKey {
                name: format!("{}_fk_{group}", id.name),
                columns: Vec::new(),
                references: parent,
                ref_columns: Vec::new(),
                deferrable: false,
            });
            entry.columns.push(from);
            entry.ref_columns.push(to);
        }
        table.foreign_keys = fks.into_values().collect();
        table.foreign_keys.sort_by(|a, b| a.columns.cmp(&b.columns));

        tables.insert(id, table);
    }

    Ok(Schema {
        default_schema: SCHEMA.to_string(),
        tables,
        enums: IndexMap::new(),
    })
}

/// How many columns an index covers, including expression parts.
///
/// `index_info` omits expression columns from its name output, so a shorter
/// name list than this means the index is not a plain column list.
async fn table_index_width(db: &Db, index: &str) -> Result<usize> {
    Ok(db
        .query_text(&format!(
            "PRAGMA index_info({})",
            crate::dialect::Sqlite.quote_ident(index)
        ))
        .await?
        .len())
}

/// The `WHERE` clause of a partial index. SQLite exposes no catalog view for
/// it, only the `CREATE INDEX` text it stored verbatim.
async fn index_predicate(db: &Db, index: &str) -> Result<Option<String>> {
    let ddl = db
        .query_text(&format!(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = '{}'",
            index.replace('\'', "''")
        ))
        .await
        .with_context(|| format!("reading the definition of index {index}"))?
        .first()
        .and_then(|r| r.first().cloned().flatten());
    Ok(ddl.as_deref().and_then(partial_predicate))
}

/// Split the predicate off a `CREATE INDEX ... WHERE ...`. The column list is
/// parenthesised, so the predicate is the only `WHERE` at depth zero.
fn partial_predicate(ddl: &str) -> Option<String> {
    let b = ddl.as_bytes();
    let (mut depth, mut in_str, mut found) = (0i32, false, None);
    let mut i = 0;
    while i < b.len() {
        if in_str {
            if b[i] == b'\'' {
                if b.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
        } else {
            match b[i] {
                b'\'' => in_str = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0 && is_word_at(b, i, b"WHERE") => {
                    found = Some(i + 5);
                    i += 5;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    found
        .map(|p| ddl[p..].trim().trim_end_matches(';').trim().to_string())
        .filter(|p| !p.is_empty())
}

fn is_word_at(b: &[u8], i: usize, word: &[u8]) -> bool {
    let boundary = |c: Option<&u8>| !c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    b.get(i..i + word.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(word))
        && boundary(i.checked_sub(1).and_then(|p| b.get(p)))
        && boundary(b.get(i + word.len()))
}

pub async fn list_tables(db: &Db) -> Result<Vec<TableId>> {
    Ok(db
        .query_text(TABLES_SQL)
        .await
        .context("listing tables")?
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(|n| TableId::new(SCHEMA, n))
        .collect())
}

/// Advance an `INTEGER PRIMARY KEY` sequence past the loaded rows.
///
/// Only tables declared `AUTOINCREMENT` keep a `sqlite_sequence` row; the rest
/// derive the next id from `max(rowid)` and need nothing.
pub fn fix_sequence_sql(id: &TableId, column: &str) -> String {
    let d = crate::dialect::Sqlite;
    format!(
        "UPDATE sqlite_sequence SET seq = (SELECT COALESCE(MAX({}), 0) FROM {}) \
         WHERE name = {}",
        d.quote_ident(column),
        d.quote_table(id),
        crate::dialect::quote_literal(&id.name)
    )
}

/// Map a declared type onto a [`TypeClass`] using SQLite's affinity rules.
///
/// The rules are applied in the order SQLite documents them, so a column
/// declared `VARCHAR(20)` gets TEXT affinity and `FLOATING POINT` gets INTEGER,
/// which is surprising but is what SQLite actually does.
pub fn classify(declared: &str) -> TypeClass {
    let d = declared.trim().to_ascii_uppercase();

    if d.is_empty() {
        // No declared type is BLOB affinity: anything goes in unconverted.
        return TypeClass::Text { max_len: None };
    }
    if d.contains("INT") {
        // BOOLEAN is INTEGER affinity but is conventionally 0/1, and treating it
        // as an integer round-trips either way.
        return TypeClass::Int { bits: 64 };
    }
    if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        return TypeClass::Text {
            max_len: parse_len(&d),
        };
    }
    if d.contains("BLOB") {
        return TypeClass::Bytes;
    }
    if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        return TypeClass::Float { bits: 64 };
    }
    // NUMERIC affinity. The declared name still says what was intended, and
    // honouring it keeps dates and decimals from being mangled into floats.
    if d.contains("DECIMAL") || d.contains("NUMERIC") {
        let mut args = type_args(&d);
        return TypeClass::Decimal {
            precision: args.next().and_then(|a| a.parse().ok()),
            scale: args.next().and_then(|a| a.parse().ok()),
        };
    }
    if d.contains("BOOL") {
        return TypeClass::Bool;
    }
    if d.starts_with("DATETIME") || d.starts_with("TIMESTAMP") {
        return TypeClass::Timestamp { tz: false };
    }
    if d.starts_with("DATE") {
        return TypeClass::Date;
    }
    if d.starts_with("TIME") {
        return TypeClass::Time { tz: false };
    }
    if d.contains("JSON") {
        return TypeClass::Json { binary: false };
    }
    if d.contains("UUID") || d.contains("GUID") {
        return TypeClass::Uuid;
    }
    TypeClass::Text { max_len: None }
}

fn parse_len(d: &str) -> Option<u32> {
    type_args(d).next().and_then(|a| a.parse().ok())
}

fn type_args(d: &str) -> impl Iterator<Item = &str> {
    let inner = d
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(args, _)| args)
        .unwrap_or("");
    inner.split(',').map(|a| a.trim()).filter(|a| !a.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_affinity_wins_wherever_int_appears() {
        for d in ["INTEGER", "INT", "BIGINT", "TINYINT", "INT8", "MEDIUMINT"] {
            assert_eq!(classify(d), TypeClass::Int { bits: 64 }, "{d}");
        }
    }

    #[test]
    fn text_affinity_covers_the_char_family() {
        assert_eq!(classify("TEXT"), TypeClass::Text { max_len: None });
        assert_eq!(classify("CLOB"), TypeClass::Text { max_len: None });
        assert_eq!(
            classify("VARCHAR(20)"),
            TypeClass::Text { max_len: Some(20) }
        );
        assert_eq!(
            classify("NVARCHAR(255)"),
            TypeClass::Text { max_len: Some(255) }
        );
    }

    #[test]
    fn affinity_order_matches_sqlite_not_intuition() {
        // "POINT" contains INT, so SQLite gives it INTEGER affinity. Following
        // the documented order rather than guessing keeps us consistent with
        // whatever the database actually stored.
        assert_eq!(classify("FLOATING POINT"), TypeClass::Int { bits: 64 });
        // CHAR beats BLOB in the ordering.
        assert_eq!(
            classify("CHARBLOB"),
            TypeClass::Text { max_len: None },
            "CHAR is checked before BLOB"
        );
    }

    #[test]
    fn blob_and_real_families() {
        assert_eq!(classify("BLOB"), TypeClass::Bytes);
        for d in ["REAL", "DOUBLE", "DOUBLE PRECISION", "FLOAT"] {
            assert_eq!(classify(d), TypeClass::Float { bits: 64 }, "{d}");
        }
    }

    #[test]
    fn decimals_keep_precision_and_scale() {
        assert_eq!(
            classify("DECIMAL(14,4)"),
            TypeClass::Decimal {
                precision: Some(14),
                scale: Some(4)
            }
        );
        assert_eq!(
            classify("NUMERIC"),
            TypeClass::Decimal {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn convention_types_are_honoured_where_affinity_does_not_decide() {
        assert_eq!(classify("BOOLEAN"), TypeClass::Bool);
        assert_eq!(classify("DATE"), TypeClass::Date);
        assert_eq!(classify("DATETIME"), TypeClass::Timestamp { tz: false });
        assert_eq!(classify("JSON"), TypeClass::Json { binary: false });
        assert_eq!(classify("UUID"), TypeClass::Uuid);
    }

    #[test]
    fn an_undeclared_type_is_carried_as_text() {
        assert_eq!(classify(""), TypeClass::Text { max_len: None });
        assert_eq!(classify("   "), TypeClass::Text { max_len: None });
    }

    #[test]
    fn sequence_fix_targets_only_the_named_table() {
        let sql = fix_sequence_sql(&TableId::new(SCHEMA, "users"), "id");
        assert!(sql.contains("sqlite_sequence"), "{sql}");
        assert!(sql.contains("'users'"), "{sql}");
        assert!(sql.contains("MAX(\"id\")"), "{sql}");
    }

    #[test]
    fn sequence_fix_escapes_a_hostile_name() {
        let sql = fix_sequence_sql(&TableId::new(SCHEMA, "us'ers"), "i\"d");
        assert!(sql.contains("'us''ers'"), "{sql}");
        assert!(sql.contains("\"i\"\"d\""), "{sql}");
    }
}

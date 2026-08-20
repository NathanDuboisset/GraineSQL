//! MySQL schema introspection via `information_schema`.
//!
//! Where MySQL differs in ways that matter:
//!
//! - No boolean type. `BOOLEAN` is `TINYINT(1)`, so display width is the only
//!   signal, and it is a convention rather than a guarantee.
//! - `BIGINT UNSIGNED` exceeds `i64`, so it is carried as an exact decimal.
//! - Enums have no named type; labels live inline in the column definition.
//! - Foreign keys are never deferrable.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::db::Db;
use crate::dialect::Dialect as _;
use crate::schema::{Column, ForeignKey, Schema, Table, TableId, TypeClass};

/// Schemas that never hold user data.
const SYSTEM_SCHEMAS: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

const COLUMNS_SQL: &str = "
SELECT c.TABLE_SCHEMA                    AS schema_name,
       c.TABLE_NAME                      AS table_name,
       c.COLUMN_NAME                     AS column_name,
       c.COLUMN_TYPE                     AS column_type,
       c.DATA_TYPE                       AS data_type,
       c.IS_NULLABLE                     AS is_nullable,
       CAST(c.CHARACTER_MAXIMUM_LENGTH AS CHAR) AS char_len,
       CAST(c.NUMERIC_PRECISION AS CHAR) AS num_precision,
       CAST(c.NUMERIC_SCALE AS CHAR)     AS num_scale,
       CASE WHEN c.COLUMN_DEFAULT IS NOT NULL THEN '1' ELSE '0' END AS has_default,
       c.EXTRA                           AS extra
  FROM information_schema.COLUMNS c
  JOIN information_schema.TABLES  t
    ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
 WHERE t.TABLE_TYPE = 'BASE TABLE'
   AND c.TABLE_SCHEMA NOT IN SYSTEM_SCHEMAS
 ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION
";

/// Primary keys and unique indexes usable as conflict targets.
///
/// `COLUMN_NAME IS NULL` marks a functional index part, and `SUB_PART IS NOT
/// NULL` marks a prefix index; neither constrains a whole column value, so
/// neither is a usable key. Both filters are written against columns that exist
/// in every supported MySQL version.
const KEYS_SQL: &str = "
SELECT s.TABLE_SCHEMA AS schema_name,
       s.TABLE_NAME   AS table_name,
       s.INDEX_NAME   AS index_name,
       GROUP_CONCAT(s.COLUMN_NAME ORDER BY s.SEQ_IN_INDEX SEPARATOR ',') AS cols
  FROM information_schema.STATISTICS s
  JOIN information_schema.TABLES t
    ON t.TABLE_SCHEMA = s.TABLE_SCHEMA AND t.TABLE_NAME = s.TABLE_NAME
 WHERE t.TABLE_TYPE = 'BASE TABLE'
   AND s.NON_UNIQUE = 0
   AND s.COLUMN_NAME IS NOT NULL
   AND s.SUB_PART IS NULL
   AND s.TABLE_SCHEMA NOT IN SYSTEM_SCHEMAS
 GROUP BY s.TABLE_SCHEMA, s.TABLE_NAME, s.INDEX_NAME
 ORDER BY s.TABLE_SCHEMA, s.TABLE_NAME, (s.INDEX_NAME = 'PRIMARY') DESC, s.INDEX_NAME
";

const FOREIGN_KEYS_SQL: &str = "
SELECT k.TABLE_SCHEMA            AS schema_name,
       k.TABLE_NAME              AS table_name,
       k.CONSTRAINT_NAME         AS fk_name,
       GROUP_CONCAT(k.COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ',') AS cols,
       k.REFERENCED_TABLE_SCHEMA AS ref_schema,
       k.REFERENCED_TABLE_NAME   AS ref_table,
       GROUP_CONCAT(k.REFERENCED_COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ',')
                                 AS ref_cols
  FROM information_schema.KEY_COLUMN_USAGE k
 WHERE k.REFERENCED_TABLE_NAME IS NOT NULL
   AND k.TABLE_SCHEMA NOT IN SYSTEM_SCHEMAS
 GROUP BY k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME,
          k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME
 ORDER BY k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME
";

pub async fn introspect(db: &Db) -> Result<Schema> {
    let default_schema = db
        .query_text("SELECT DATABASE()")
        .await
        .context("reading the current database name")?
        .first()
        .and_then(|r| r.first().cloned().flatten())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the connection has no default database; include one in the URL, as in \
                 mysql://user@host/dbname"
            )
        })?;

    let mut tables: IndexMap<TableId, Table> = IndexMap::new();
    let mut enums: IndexMap<String, Vec<String>> = IndexMap::new();

    for row in db
        .query_text(&COLUMNS_SQL.replace("SYSTEM_SCHEMAS", SYSTEM_SCHEMAS))
        .await
        .context("introspecting columns")?
    {
        let f = Fields::new(&row, 11, "columns")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let column_name = f.text(2)?.to_string();
        let column_type = f.text(3)?.to_string();
        let data_type = f.text(4)?.to_ascii_lowercase();
        let extra = f.opt(10).unwrap_or("").to_ascii_lowercase();

        let class = classify(
            &data_type,
            &column_type,
            f.opt(6).and_then(|v| v.parse().ok()),
            f.opt(7).and_then(|v| v.parse().ok()),
            f.opt(8).and_then(|v| v.parse().ok()),
        );

        // Enum labels are inline in the column type rather than in a named type,
        // so the "type name" we key them by is the column that declares them.
        if let TypeClass::Enum { name } = &class {
            enums.insert(name.clone(), parse_enum_labels(&column_type));
        }

        let column = Column {
            name: column_name,
            sql_type: column_type,
            class,
            nullable: f.text(5)?.eq_ignore_ascii_case("YES"),
            // `DEFAULT_GENERATED` marks an expression default, which still means
            // the database can fill the column in.
            has_default: f.text(9)? == "1"
                || extra.contains("auto_increment")
                || extra.contains("default_generated"),
            generated: extra.contains("generated") && !extra.contains("default_generated"),
            identity: extra.contains("auto_increment"),
        };

        tables
            .entry(id.clone())
            .or_insert_with(|| Table {
                id,
                columns: Vec::new(),
                primary_key: Vec::new(),
                unique: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .columns
            .push(column);
    }

    for row in db
        .query_text(&KEYS_SQL.replace("SYSTEM_SCHEMAS", SYSTEM_SCHEMAS))
        .await
        .context("introspecting primary keys and unique indexes")?
    {
        let f = Fields::new(&row, 4, "keys")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let cols = split_list(f.text(3)?);
        let Some(table) = tables.get_mut(&id) else {
            continue;
        };
        if f.text(2)? == "PRIMARY" {
            table.primary_key = cols;
        } else if !table.unique.contains(&cols) {
            table.unique.push(cols);
        }
    }

    for row in db
        .query_text(&FOREIGN_KEYS_SQL.replace("SYSTEM_SCHEMAS", SYSTEM_SCHEMAS))
        .await
        .context("introspecting foreign keys")?
    {
        let f = Fields::new(&row, 7, "foreign keys")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let fk = ForeignKey {
            name: f.text(2)?.to_string(),
            columns: split_list(f.text(3)?),
            references: TableId::new(f.text(4)?, f.text(5)?),
            ref_columns: split_list(f.text(6)?),
            // MySQL parses DEFERRABLE but never honours it.
            deferrable: false,
        };
        if let Some(table) = tables.get_mut(&id) {
            table.foreign_keys.push(fk);
        }
    }

    let used: BTreeSet<String> = tables
        .values()
        .flat_map(|t| t.columns.iter())
        .filter_map(|c| match &c.class {
            TypeClass::Enum { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    enums.retain(|name, _| used.contains(name));

    Ok(Schema {
        default_schema,
        tables,
        enums,
    })
}

pub async fn list_tables(db: &Db) -> Result<Vec<TableId>> {
    let sql = format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME
           FROM information_schema.TABLES
          WHERE TABLE_TYPE = 'BASE TABLE'
            AND TABLE_SCHEMA NOT IN {SYSTEM_SCHEMAS}
          ORDER BY TABLE_SCHEMA, TABLE_NAME"
    );
    db.query_text(&sql)
        .await
        .context("listing tables")?
        .iter()
        .map(|row| {
            let f = Fields::new(row, 2, "table list")?;
            Ok(TableId::new(f.text(0)?, f.text(1)?))
        })
        .collect()
}

/// Statement that reads the next `AUTO_INCREMENT` value a table needs.
pub fn next_auto_increment_sql(id: &TableId, column: &str) -> String {
    format!(
        "SELECT CAST(COALESCE(MAX({}), 0) + 1 AS CHAR) FROM {}",
        crate::dialect::Mysql.quote_ident(column),
        crate::dialect::Mysql.quote_table(id)
    )
}

/// Statement that sets a table's `AUTO_INCREMENT`.
///
/// `next` must come from [`next_auto_increment_sql`]: this cannot be
/// parameterised, and `ALTER TABLE` performs an implicit commit, so it has to
/// run after the load's transaction rather than inside it.
pub fn set_auto_increment_sql(id: &TableId, next: &str) -> Result<String> {
    if next.is_empty() || !next.bytes().all(|b| b.is_ascii_digit()) {
        bail!("refusing to splice {next:?} into an ALTER TABLE; expected a positive integer");
    }
    Ok(format!(
        "ALTER TABLE {} AUTO_INCREMENT = {next}",
        crate::dialect::Mysql.quote_table(id)
    ))
}

/// Labels of an inline enum or set declaration.
///
/// `enum('free','pro','it''s')` -> `["free", "pro", "it's"]`.
fn parse_enum_labels(column_type: &str) -> Vec<String> {
    let Some(open) = column_type.find('(') else {
        return Vec::new();
    };
    let Some(close) = column_type.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    let body = &column_type[open + 1..close];

    let mut labels = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if in_quotes => {
                if chars.peek() == Some(&'\'') {
                    // Doubled quote inside a label.
                    current.push('\'');
                    chars.next();
                } else {
                    in_quotes = false;
                    labels.push(std::mem::take(&mut current));
                }
            }
            '\'' => in_quotes = true,
            '\\' if in_quotes => {
                // MySQL also accepts backslash escapes in these literals.
                if let Some(next) = chars.next() {
                    current.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        other => other,
                    });
                }
            }
            c if in_quotes => current.push(c),
            _ => {}
        }
    }
    labels
}

/// Map a MySQL type onto a [`TypeClass`].
///
/// `data_type` is the bare name (`varchar`), `column_type` the full declaration
/// (`varchar(120)`, `int unsigned`, `enum('a','b')`).
pub fn classify(
    data_type: &str,
    column_type: &str,
    char_len: Option<u32>,
    precision: Option<u16>,
    scale: Option<i16>,
) -> TypeClass {
    let unsigned = column_type.to_ascii_lowercase().contains("unsigned");

    match data_type {
        // MySQL has no boolean type: BOOLEAN is an alias for TINYINT(1), and the
        // display width is the only thing distinguishing the two intents.
        "tinyint" if column_type.starts_with("tinyint(1)") => TypeClass::Bool,
        "bool" | "boolean" => TypeClass::Bool,

        "tinyint" => TypeClass::Int {
            bits: if unsigned { 16 } else { 8 },
        },
        "smallint" => TypeClass::Int {
            bits: if unsigned { 32 } else { 16 },
        },
        // MEDIUMINT is 24-bit; the next size up covers it either way.
        "mediumint" => TypeClass::Int { bits: 32 },
        "int" | "integer" => TypeClass::Int {
            bits: if unsigned { 64 } else { 32 },
        },
        // BIGINT UNSIGNED runs past i64, so it is carried as an exact decimal
        // rather than an integer that would silently overflow.
        "bigint" if unsigned => TypeClass::Decimal {
            precision: Some(20),
            scale: Some(0),
        },
        "bigint" => TypeClass::Int { bits: 64 },
        "year" => TypeClass::Int { bits: 16 },

        "float" => TypeClass::Float { bits: 32 },
        "double" | "real" => TypeClass::Float { bits: 64 },
        "decimal" | "numeric" => TypeClass::Decimal { precision, scale },

        "char" | "varchar" => TypeClass::Text { max_len: char_len },
        "tinytext" | "text" | "mediumtext" | "longtext" => TypeClass::Text { max_len: None },

        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            TypeClass::Bytes
        }

        // MySQL normalises JSON object keys on storage, the way jsonb does.
        "json" => TypeClass::Json { binary: true },

        "date" => TypeClass::Date,
        "time" => TypeClass::Time { tz: false },
        "datetime" => TypeClass::Timestamp { tz: false },
        // TIMESTAMP is stored as UTC and converted for the session, so it is the
        // zone-aware one of the pair, the opposite of what the names suggest.
        "timestamp" => TypeClass::Timestamp { tz: true },

        // Keyed by the column that declares them, since there is no named type.
        "enum" => TypeClass::Enum {
            name: column_type.to_string(),
        },

        other => TypeClass::Other {
            name: other.to_string(),
        },
    }
}

fn split_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',').map(|p| p.to_string()).collect()
}

struct Fields<'a> {
    row: &'a [Option<String>],
    what: &'static str,
}

impl<'a> Fields<'a> {
    fn new(row: &'a [Option<String>], expected: usize, what: &'static str) -> Result<Self> {
        if row.len() != expected {
            bail!(
                "{what} introspection returned {} columns, expected {expected}",
                row.len()
            );
        }
        Ok(Self { row, what })
    }

    fn text(&self, i: usize) -> Result<&str> {
        self.row[i]
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{} introspection: column {i} was NULL", self.what))
    }

    fn opt(&self, i: usize) -> Option<&str> {
        self.row[i].as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(data_type: &str, column_type: &str) -> TypeClass {
        classify(data_type, column_type, None, None, None)
    }

    #[test]
    fn tinyint_1_is_the_boolean_convention() {
        // MySQL has no boolean type; BOOLEAN is stored as TINYINT(1).
        assert_eq!(c("tinyint", "tinyint(1)"), TypeClass::Bool);
        // A wider tinyint is a real small integer.
        assert_eq!(c("tinyint", "tinyint(4)"), TypeClass::Int { bits: 8 });
        assert_eq!(c("tinyint", "tinyint"), TypeClass::Int { bits: 8 });
    }

    #[test]
    fn unsigned_integers_widen_to_hold_their_range() {
        assert_eq!(c("int", "int"), TypeClass::Int { bits: 32 });
        assert_eq!(c("int", "int unsigned"), TypeClass::Int { bits: 64 });
        assert_eq!(
            c("smallint", "smallint unsigned"),
            TypeClass::Int { bits: 32 }
        );
    }

    #[test]
    fn unsigned_bigint_becomes_a_decimal_rather_than_overflowing() {
        // u64's range does not fit in the i64 that Value::Int holds, so the only
        // lossless carrier is an exact decimal.
        assert_eq!(c("bigint", "bigint"), TypeClass::Int { bits: 64 });
        assert_eq!(
            c("bigint", "bigint unsigned"),
            TypeClass::Decimal {
                precision: Some(20),
                scale: Some(0)
            }
        );
    }

    #[test]
    fn timestamp_is_the_zone_aware_one_not_datetime() {
        // Counter-intuitive but correct: MySQL stores TIMESTAMP as UTC and
        // converts it per session, while DATETIME is a wall-clock value.
        assert_eq!(
            c("timestamp", "timestamp"),
            TypeClass::Timestamp { tz: true }
        );
        assert_eq!(
            c("datetime", "datetime(6)"),
            TypeClass::Timestamp { tz: false }
        );
    }

    #[test]
    fn text_length_comes_from_the_metadata_column() {
        assert_eq!(
            classify("varchar", "varchar(120)", Some(120), None, None),
            TypeClass::Text { max_len: Some(120) }
        );
        assert_eq!(c("longtext", "longtext"), TypeClass::Text { max_len: None });
    }

    #[test]
    fn decimal_carries_precision_and_scale() {
        assert_eq!(
            classify("decimal", "decimal(14,4)", None, Some(14), Some(4)),
            TypeClass::Decimal {
                precision: Some(14),
                scale: Some(4)
            }
        );
    }

    #[test]
    fn binary_types_are_all_bytes() {
        for t in [
            "binary",
            "varbinary",
            "tinyblob",
            "blob",
            "mediumblob",
            "longblob",
            "bit",
        ] {
            assert_eq!(c(t, t), TypeClass::Bytes, "{t}");
        }
    }

    #[test]
    fn json_sorts_keys_like_jsonb() {
        assert_eq!(c("json", "json"), TypeClass::Json { binary: true });
    }

    #[test]
    fn unknown_types_are_carried_through() {
        assert_eq!(
            c("geometry", "geometry"),
            TypeClass::Other {
                name: "geometry".into()
            }
        );
        assert_eq!(
            c("set", "set('a','b')"),
            TypeClass::Other { name: "set".into() }
        );
        assert!(c("geometry", "geometry").needs_text_cast());
    }

    #[test]
    fn enums_are_keyed_by_their_declaration() {
        // There is no named enum type to key on, so the declaration itself is
        // the identity, which also means changing the labels changes the key,
        // and drift reports it.
        let class = c("enum", "enum('free','pro')");
        assert_eq!(
            class,
            TypeClass::Enum {
                name: "enum('free','pro')".into()
            }
        );
    }

    #[test]
    fn enum_labels_parse_out_of_the_declaration() {
        assert_eq!(
            parse_enum_labels("enum('free','pro','team')"),
            ["free", "pro", "team"]
        );
        assert_eq!(parse_enum_labels("enum('a')"), ["a"]);
        assert!(parse_enum_labels("int").is_empty());
    }

    #[test]
    fn enum_labels_handle_quotes_commas_and_escapes() {
        // A label may contain the delimiter and the quote character.
        assert_eq!(
            parse_enum_labels("enum('a,b','it''s','say \"hi\"')"),
            ["a,b", "it's", "say \"hi\""]
        );
        assert_eq!(
            parse_enum_labels(r"enum('back\\slash','tab\there')"),
            ["back\\slash", "tab\there"]
        );
        assert_eq!(parse_enum_labels("enum('')"), [""]);
    }

    #[test]
    fn auto_increment_is_only_set_from_a_verified_integer() {
        let id = TableId::new("app", "users");
        assert_eq!(
            set_auto_increment_sql(&id, "42").unwrap(),
            "ALTER TABLE `app`.`users` AUTO_INCREMENT = 42"
        );
        // The value cannot be parameterised, so anything but digits is refused
        // rather than spliced into DDL.
        for bad in ["", "1; DROP TABLE users", "-1", "1.5", "abc"] {
            assert!(
                set_auto_increment_sql(&id, bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn next_auto_increment_query_is_quoted() {
        let sql = next_auto_increment_sql(&TableId::new("app", "us`ers"), "i`d");
        assert!(sql.contains("`us``ers`"), "{sql}");
        assert!(sql.contains("`i``d`"), "{sql}");
    }

    #[test]
    fn split_list_handles_empty() {
        assert!(split_list("").is_empty());
        assert_eq!(split_list("a,b"), ["a", "b"]);
    }
}

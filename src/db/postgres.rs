//! Postgres schema introspection via `pg_catalog`.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use indexmap::IndexMap;

use crate::db::{Db, Fields};
use crate::schema::{Column, ForeignKey, Schema, Table, TableId, TypeClass};

/// Schemas that never hold user data.
const SYSTEM_SCHEMAS: &str = "('pg_catalog', 'information_schema', 'pg_toast')";

/// Columns of every user table, ordered so tables come out grouped and columns
/// come out in physical order.
///
/// `format_type` is used for `sql_type` because its output is directly usable as
/// a cast target, including any needed quoting, which is exactly what the write
/// path needs.
const COLUMNS_SQL: &str = "
SELECT n.nspname                                        AS schema_name,
       c.relname                                        AS table_name,
       a.attname                                        AS column_name,
       a.attnum::text                                   AS attnum,
       format_type(a.atttypid, a.atttypmod)             AS sql_type,
       (NOT a.attnotnull)::text                         AS nullable,
       (a.atthasdef OR a.attidentity <> '')::text       AS has_default,
       (a.attgenerated <> '')::text                     AS generated,
       (a.attidentity <> ''
         OR pg_get_serial_sequence(
              quote_ident(n.nspname) || '.' || quote_ident(c.relname),
              a.attname) IS NOT NULL)::text             AS identity,
       t.typtype::text                                  AS typtype,
       t.typname                                        AS typname,
       COALESCE(bt.typname, '')                         AS base_typname,
       COALESCE(bt.typtype::text, '')                   AS base_typtype
  FROM pg_attribute a
  JOIN pg_class     c  ON c.oid = a.attrelid
  JOIN pg_namespace n  ON n.oid = c.relnamespace
  JOIN pg_type      t  ON t.oid = a.atttypid
  LEFT JOIN pg_type bt ON bt.oid = NULLIF(t.typbasetype, 0)
 WHERE c.relkind IN ('r', 'p')
   AND a.attnum > 0
   AND NOT a.attisdropped
   AND n.nspname NOT LIKE 'pg_temp%'
   AND n.nspname NOT IN SYSTEM_SCHEMAS
 ORDER BY n.nspname, c.relname, a.attnum
";

/// Primary keys and unique constraints usable as upsert targets.
///
/// Partial indexes (`indpred IS NOT NULL`) and expression indexes (an `indkey`
/// entry of 0) are excluded: neither is a valid `ON CONFLICT` target.
const KEYS_SQL: &str = "
SELECT n.nspname                                    AS schema_name,
       c.relname                                    AS table_name,
       i.indisprimary::text                         AS is_primary,
       (SELECT string_agg(a.attname, ',' ORDER BY k.ord)
          FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a
            ON a.attrelid = c.oid AND a.attnum = k.attnum)  AS cols
  FROM pg_index     i
  JOIN pg_class     c ON c.oid = i.indrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'p')
   AND (i.indisprimary OR i.indisunique)
   AND i.indisvalid
   AND i.indpred IS NULL
   AND 0 <> ALL (i.indkey::int2[])
   AND n.nspname NOT IN SYSTEM_SCHEMAS
 ORDER BY n.nspname, c.relname, i.indisprimary DESC, 4
";

const FOREIGN_KEYS_SQL: &str = "
SELECT n.nspname                                    AS schema_name,
       c.relname                                    AS table_name,
       con.conname                                  AS fk_name,
       (SELECT string_agg(a.attname, ',' ORDER BY k.ord)
          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a
            ON a.attrelid = con.conrelid AND a.attnum = k.attnum)   AS cols,
       fn.nspname                                   AS ref_schema,
       fc.relname                                   AS ref_table,
       (SELECT string_agg(a.attname, ',' ORDER BY k.ord)
          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a
            ON a.attrelid = con.confrelid AND a.attnum = k.attnum)  AS ref_cols,
       con.condeferrable::text                      AS deferrable
  FROM pg_constraint con
  JOIN pg_class     c  ON c.oid = con.conrelid
  JOIN pg_namespace n  ON n.oid = c.relnamespace
  JOIN pg_class     fc ON fc.oid = con.confrelid
  JOIN pg_namespace fn ON fn.oid = fc.relnamespace
 WHERE con.contype = 'f'
   AND n.nspname NOT IN SYSTEM_SCHEMAS
 ORDER BY n.nspname, c.relname, con.conname
";

const ENUMS_SQL: &str = "
SELECT t.typname                                            AS type_name,
       string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder) AS labels
  FROM pg_enum      e
  JOIN pg_type      t ON t.oid = e.enumtypid
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname NOT IN SYSTEM_SCHEMAS
 GROUP BY t.typname
 ORDER BY t.typname
";

/// Introspect the live schema.
pub async fn introspect(db: &Db) -> Result<Schema> {
    let default_schema = db
        .query_text("SELECT current_schema()")
        .await
        .context("reading current_schema()")?
        .first()
        .and_then(|r| r.first().cloned().flatten())
        .unwrap_or_else(|| "public".to_string());

    let mut tables: IndexMap<TableId, Table> = IndexMap::new();

    for row in db
        .query_text(&COLUMNS_SQL.replace("SYSTEM_SCHEMAS", SYSTEM_SCHEMAS))
        .await
        .context("introspecting columns")?
    {
        let f = Fields::new(&row, 13, "columns")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let sql_type = f.text(4)?.to_string();
        let typtype = f.text(9)?;
        let typname = f.text(10)?;
        let base_typname = f.opt(11);
        let base_typtype = f.opt(12);

        let column = Column {
            name: f.text(2)?.to_string(),
            class: classify(
                &sql_type,
                typtype,
                typname,
                base_typname.unwrap_or(""),
                base_typtype.unwrap_or(""),
            ),
            sql_type,
            nullable: f.bool(5)?,
            has_default: f.bool(6)?,
            generated: f.bool(7)?,
            identity: f.bool(8)?,
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
        .context("introspecting primary keys and unique constraints")?
    {
        let f = Fields::new(&row, 4, "keys")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let is_primary = f.bool(2)?;
        let cols: Vec<String> = split_list(f.text(3)?);
        let Some(table) = tables.get_mut(&id) else {
            continue;
        };
        if is_primary {
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
        let f = Fields::new(&row, 8, "foreign keys")?;
        let id = TableId::new(f.text(0)?, f.text(1)?);
        let fk = ForeignKey {
            name: f.text(2)?.to_string(),
            columns: split_list(f.text(3)?),
            references: TableId::new(f.text(4)?, f.text(5)?),
            ref_columns: split_list(f.text(6)?),
            deferrable: f.bool(7)?,
        };
        if let Some(table) = tables.get_mut(&id) {
            table.foreign_keys.push(fk);
        }
    }

    let mut enums = IndexMap::new();
    for row in db
        .query_text(&ENUMS_SQL.replace("SYSTEM_SCHEMAS", SYSTEM_SCHEMAS))
        .await
        .context("introspecting enum types")?
    {
        let f = Fields::new(&row, 2, "enums")?;
        enums.insert(f.text(0)?.to_string(), split_list(f.text(1)?));
    }

    // Keep only enums some column actually uses, so an unrelated type change
    // elsewhere in the database does not register as drift.
    let used: BTreeSet<String> = tables
        .values()
        .flat_map(|t| t.columns.iter())
        .filter_map(|c| enum_name(&c.class))
        .collect();
    enums.retain(|name, _| used.contains(name));

    Ok(Schema {
        default_schema,
        tables,
        enums,
    })
}

/// Enum type name referenced by a class, looking through arrays.
fn enum_name(class: &TypeClass) -> Option<String> {
    match class {
        TypeClass::Enum { name } => Some(name.clone()),
        TypeClass::Array { of } => enum_name(of),
        _ => None,
    }
}

/// Every user table, for `graine init`.
pub async fn list_tables(db: &Db) -> Result<Vec<TableId>> {
    let sql = format!(
        "SELECT n.nspname, c.relname
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p')
            AND n.nspname NOT LIKE 'pg_temp%'
            AND n.nspname NOT IN {SYSTEM_SCHEMAS}
          ORDER BY n.nspname, c.relname"
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

/// Advance the sequence behind an identity/serial column past the loaded rows.
///
/// Without this the next application insert reuses a key that a seed row already
/// took, the classic seed-tool footgun.
pub fn fix_sequence_sql(id: &TableId, column: &str) -> String {
    // `is_called = false` means the next `nextval` returns exactly this value,
    // so an empty table correctly resets to 1.
    format!(
        "SELECT setval(
                  pg_get_serial_sequence({}, {}),
                  COALESCE((SELECT MAX({}) FROM {}), 0) + 1,
                  false)
           WHERE pg_get_serial_sequence({}, {}) IS NOT NULL",
        crate::dialect::quote_literal(&table_ref_literal(id)),
        crate::dialect::quote_literal(column),
        quote_ident(column),
        quote_table(id),
        crate::dialect::quote_literal(&table_ref_literal(id)),
        crate::dialect::quote_literal(column),
    )
}

fn table_ref_literal(id: &TableId) -> String {
    match &id.schema {
        Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&id.name)),
        None => quote_ident(&id.name),
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn quote_table(id: &TableId) -> String {
    table_ref_literal(id)
}

fn split_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',').map(|p| p.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Type classification
// ---------------------------------------------------------------------------

/// Map a Postgres type onto a [`TypeClass`].
///
/// `sql_type` is `format_type()` output (`character varying(50)`,
/// `numeric(10,2)`, `integer[]`). `typtype` is the `pg_type.typtype` code, which
/// is how enums are recognised, their name carries no marker.
pub fn classify(
    sql_type: &str,
    typtype: &str,
    typname: &str,
    base_typname: &str,
    base_typtype: &str,
) -> TypeClass {
    // Domains: classify by the underlying base type, since that is what
    // constrains the values, but keep `sql_type` (the domain name) for casts.
    if typtype == "d" && !base_typname.is_empty() {
        let inner = classify_name(base_typname, base_typtype, base_typname);
        if !matches!(inner, TypeClass::Other { .. }) {
            return inner;
        }
    }

    let trimmed = sql_type.trim();
    if let Some(inner) = trimmed.strip_suffix("[]") {
        return TypeClass::Array {
            of: Box::new(classify(
                inner,
                typtype,
                typname.trim_start_matches('_'),
                "",
                "",
            )),
        };
    }

    if typtype == "e" {
        return TypeClass::Enum {
            name: typname.to_string(),
        };
    }

    classify_name(trimmed, typtype, typname)
}

fn classify_name(sql_type: &str, typtype: &str, typname: &str) -> TypeClass {
    let (base, args) = split_type_args(sql_type);
    let base_lower = base.to_ascii_lowercase();

    match base_lower.as_str() {
        "boolean" | "bool" => TypeClass::Bool,
        "smallint" | "int2" | "smallserial" => TypeClass::Int { bits: 16 },
        "integer" | "int" | "int4" | "serial" => TypeClass::Int { bits: 32 },
        "bigint" | "int8" | "bigserial" => TypeClass::Int { bits: 64 },
        "real" | "float4" => TypeClass::Float { bits: 32 },
        "double precision" | "float8" => TypeClass::Float { bits: 64 },
        "numeric" | "decimal" | "money" => {
            let mut it = args.iter();
            TypeClass::Decimal {
                precision: it.next().and_then(|a| a.parse().ok()),
                scale: it.next().and_then(|a| a.parse().ok()),
            }
        }
        "text" | "name" | "citext" | "xml" => TypeClass::Text { max_len: None },
        "character varying" | "varchar" | "character" | "char" | "bpchar" => TypeClass::Text {
            max_len: args.first().and_then(|a| a.parse().ok()),
        },
        "bytea" => TypeClass::Bytes,
        "uuid" => TypeClass::Uuid,
        "json" => TypeClass::Json { binary: false },
        "jsonb" => TypeClass::Json { binary: true },
        "date" => TypeClass::Date,
        "time without time zone" | "time" => TypeClass::Time { tz: false },
        "time with time zone" | "timetz" => TypeClass::Time { tz: true },
        "timestamp without time zone" | "timestamp" => TypeClass::Timestamp { tz: false },
        "timestamp with time zone" | "timestamptz" => TypeClass::Timestamp { tz: true },
        "interval" => TypeClass::Interval,
        _ if typtype == "e" => TypeClass::Enum {
            name: typname.to_string(),
        },
        _ => TypeClass::Other {
            name: sql_type.to_string(),
        },
    }
}

/// Split `numeric(10,2)` into `("numeric", ["10", "2"])`.
fn split_type_args(s: &str) -> (&str, Vec<&str>) {
    let Some(open) = s.find('(') else {
        return (s, Vec::new());
    };
    let Some(close) = s.rfind(')') else {
        return (s, Vec::new());
    };
    if close < open {
        return (s, Vec::new());
    }
    let base = s[..open].trim();
    let args = s[open + 1..close]
        .split(',')
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect();
    // `timestamp(3) with time zone` keeps its modifier in the middle, so glue
    // the tail back on to preserve the full type name.
    let tail = s[close + 1..].trim();
    if tail.is_empty() {
        (base, args)
    } else {
        // Leak-free approach is not possible without allocation, so recognise
        // the two shapes Postgres actually produces.
        let full = if tail.starts_with("with") || tail.starts_with("without") {
            match (base, tail) {
                ("timestamp", t) if t.contains("without") => "timestamp without time zone",
                ("timestamp", _) => "timestamp with time zone",
                ("time", t) if t.contains("without") => "time without time zone",
                ("time", _) => "time with time zone",
                (b, _) => b,
            }
        } else {
            base
        };
        (full, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(sql_type: &str) -> TypeClass {
        classify(sql_type, "b", "", "", "")
    }

    #[test]
    fn classifies_integers_by_width() {
        assert_eq!(c("smallint"), TypeClass::Int { bits: 16 });
        assert_eq!(c("integer"), TypeClass::Int { bits: 32 });
        assert_eq!(c("bigint"), TypeClass::Int { bits: 64 });
    }

    #[test]
    fn classifies_floats_and_decimals() {
        assert_eq!(c("real"), TypeClass::Float { bits: 32 });
        assert_eq!(c("double precision"), TypeClass::Float { bits: 64 });
        assert_eq!(
            c("numeric(10,2)"),
            TypeClass::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        // An unconstrained numeric has no precision to compare against.
        assert_eq!(
            c("numeric"),
            TypeClass::Decimal {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn classifies_text_with_and_without_a_length() {
        assert_eq!(c("text"), TypeClass::Text { max_len: None });
        assert_eq!(
            c("character varying(255)"),
            TypeClass::Text { max_len: Some(255) }
        );
        assert_eq!(c("character varying"), TypeClass::Text { max_len: None });
        assert_eq!(c("character(2)"), TypeClass::Text { max_len: Some(2) });
    }

    #[test]
    fn classifies_temporal_types_including_precision_modifiers() {
        assert_eq!(c("date"), TypeClass::Date);
        assert_eq!(
            c("timestamp with time zone"),
            TypeClass::Timestamp { tz: true }
        );
        assert_eq!(
            c("timestamp without time zone"),
            TypeClass::Timestamp { tz: false }
        );
        // format_type puts the precision in the middle of the type name.
        assert_eq!(
            c("timestamp(3) with time zone"),
            TypeClass::Timestamp { tz: true }
        );
        assert_eq!(
            c("timestamp(6) without time zone"),
            TypeClass::Timestamp { tz: false }
        );
        assert_eq!(
            c("time(3) without time zone"),
            TypeClass::Time { tz: false }
        );
        assert_eq!(c("time with time zone"), TypeClass::Time { tz: true });
        assert_eq!(c("interval"), TypeClass::Interval);
    }

    #[test]
    fn classifies_json_binary_flag() {
        assert_eq!(c("json"), TypeClass::Json { binary: false });
        assert_eq!(c("jsonb"), TypeClass::Json { binary: true });
    }

    #[test]
    fn classifies_arrays_by_element_type() {
        assert_eq!(
            c("integer[]"),
            TypeClass::Array {
                of: Box::new(TypeClass::Int { bits: 32 })
            }
        );
        assert_eq!(
            c("text[]"),
            TypeClass::Array {
                of: Box::new(TypeClass::Text { max_len: None })
            }
        );
    }

    #[test]
    fn recognises_enums_from_typtype_not_from_the_name() {
        // An enum's type name looks like any other identifier, so only typtype
        // distinguishes it.
        assert_eq!(
            classify("tier", "e", "tier", "", ""),
            TypeClass::Enum {
                name: "tier".into()
            }
        );
        // The same name without the marker is an unmodelled type.
        assert_eq!(
            classify("tier", "b", "tier", "", ""),
            TypeClass::Other {
                name: "tier".into()
            }
        );
    }

    #[test]
    fn enum_arrays_carry_the_element_enum() {
        let class = classify("tier[]", "e", "_tier", "", "");
        assert_eq!(
            class,
            TypeClass::Array {
                of: Box::new(TypeClass::Enum {
                    name: "tier".into()
                })
            }
        );
        assert_eq!(enum_name(&class).as_deref(), Some("tier"));
    }

    #[test]
    fn domains_classify_as_their_base_type() {
        // A domain over text must behave like text for widening and encoding.
        assert_eq!(
            classify("email_address", "d", "email_address", "text", "b"),
            TypeClass::Text { max_len: None }
        );
    }

    #[test]
    fn unknown_types_are_carried_through_not_dropped() {
        assert_eq!(
            c("tsvector"),
            TypeClass::Other {
                name: "tsvector".into()
            }
        );
        assert!(c("tsvector").needs_text_cast());
    }

    #[test]
    fn splits_type_arguments() {
        assert_eq!(
            split_type_args("numeric(10,2)"),
            ("numeric", vec!["10", "2"])
        );
        assert_eq!(split_type_args("text"), ("text", vec![]));
        assert_eq!(
            split_type_args("character varying(50)"),
            ("character varying", vec!["50"])
        );
    }

    #[test]
    fn sequence_fix_is_guarded_and_quoted() {
        let sql = fix_sequence_sql(&TableId::new("public", "users"), "id");
        assert!(sql.contains("setval"));
        assert!(
            sql.contains("'\"public\".\"users\"'"),
            "the table reference must be a quoted literal holding quoted identifiers: {sql}"
        );
        // A table with no sequence must be a no-op rather than an error.
        assert!(sql.contains("IS NOT NULL"), "{sql}");
    }

    #[test]
    fn sequence_fix_escapes_hostile_identifiers() {
        let sql = fix_sequence_sql(&TableId::new("pu'blic", "us\"ers"), "i'd");
        // The apostrophe is doubled inside the literal and the quote inside the
        // identifier, so neither can terminate its context early.
        assert!(sql.contains("pu''blic"), "{sql}");
        assert!(sql.contains("us\"\""), "{sql}");
        assert!(sql.contains("i''d"), "{sql}");
    }

    #[test]
    fn split_list_handles_empty() {
        assert!(split_list("").is_empty());
        assert_eq!(split_list("a"), ["a"]);
        assert_eq!(split_list("a,b"), ["a", "b"]);
    }
}

//! Pushing seed files back into a target database.
//!
//! The whole load runs inside one transaction by default, so a failure halfway
//! through leaves the database exactly as it was. That is why `DELETE FROM` is
//! used instead of `TRUNCATE` (see [`crate::dialect::Dialect::delete_all`]) and
//! why sequence fixup on MySQL has to be deferred until after the commit.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::config::{LoadMode, ResolvedTable};
use crate::db::PinnedConn;
use crate::dialect::Dialect;
use crate::schema::{Column, Schema, Table, TableId};
use crate::value::Value;

/// What a load did, or would do, to one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePlan {
    pub table: TableId,
    pub mode: LoadMode,
    pub rows: u64,
    /// Columns being written, in schema order.
    pub columns: Vec<String>,
    /// The upsert conflict target, when the mode uses one.
    pub key: Vec<String>,
}

/// Outcome of loading one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableResult {
    pub table: TableId,
    pub mode: LoadMode,
    /// Rows read from the seed files.
    pub rows: u64,
    /// Rows the database reported as affected. Under `upsert` an update counts
    /// too, so this is not a pure insert count.
    pub affected: u64,
    /// Rows removed by a `truncate_first` pass.
    pub deleted: u64,
}

/// Resolve the conflict target for a table, honouring a `key:` override.
pub fn upsert_key(table: &Table, cfg: &ResolvedTable) -> Result<Vec<String>> {
    if let Some(key) = &cfg.key {
        for k in key {
            if table.column(k).is_none() {
                bail!(
                    "table {}: `key` names {k:?}, which the table does not have",
                    cfg.id
                );
            }
        }
        // A conflict target must be backed by a unique constraint, or the
        // database rejects the statement with a message that does not explain
        // why. Say so here instead.
        let is_unique = table.primary_key == *key || table.unique.iter().any(|u| u == key);
        if !is_unique {
            bail!(
                "table {}: `key: [{}]` is not backed by a primary key or unique constraint, so \
                 it cannot be an upsert conflict target.\n\
                 The table's primary key is {} and its unique constraints are {}.",
                cfg.id,
                key.join(", "),
                if table.primary_key.is_empty() {
                    "(none)".to_string()
                } else {
                    format!("({})", table.primary_key.join(", "))
                },
                if table.unique.is_empty() {
                    "(none)".to_string()
                } else {
                    table
                        .unique
                        .iter()
                        .map(|u| format!("({})", u.join(", ")))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        }
        return Ok(key.clone());
    }
    Ok(table.upsert_key().map(|k| k.to_vec()).unwrap_or_default())
}

/// Build the parameterised `INSERT` for one row.
///
/// One statement per row rather than a multi-row batch: it keeps the generated
/// SQL identical for every row (so the server can reuse the plan), and it means
/// an error message names the offending row instead of a batch of a hundred.
pub fn insert_statement(
    dialect: &dyn Dialect,
    table: &Table,
    columns: &[&Column],
    key: &[String],
    mode: LoadMode,
) -> Result<String> {
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let placeholders: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| dialect.write_expr(c, i + 1))
        .collect();

    Ok(format!(
        "{} {} ({}) VALUES ({}){}",
        dialect.insert_verb(mode),
        dialect.quote_table(&table.id),
        names
            .iter()
            .map(|n| dialect.quote_ident(n))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders.join(", "),
        dialect.conflict_clause(table, key, &names, mode)?
    ))
}

/// Check the rows against the live column definitions before writing anything.
///
/// Catches the cases schema drift classification cannot: an explicit null in a
/// column that is now NOT NULL, or a value too long for a narrowed type. Failing
/// here means the transaction never opens.
pub fn validate_rows(
    table: &Table,
    columns: &[&Column],
    rows: &[Vec<Value>],
    cfg: &ResolvedTable,
) -> Result<()> {
    for (i, row) in rows.iter().enumerate() {
        for (col, value) in columns.iter().zip(row) {
            if value.is_null() && !col.nullable && !col.has_default {
                bail!(
                    "table {}: row {} has null in {:?}, which is NOT NULL with no default.\n\
                     Re-export the table, or fix the seed file.",
                    cfg.id,
                    i + 1,
                    col.name
                );
            }
            if let (crate::schema::TypeClass::Text { max_len: Some(max) }, Some(text)) =
                (&col.class, value.to_text())
            {
                let len = text.chars().count();
                if len > *max as usize {
                    bail!(
                        "table {}: row {} has {len} characters in {:?}, which holds at most {max}",
                        cfg.id,
                        i + 1,
                        col.name
                    );
                }
            }
        }
    }

    // A duplicate key inside one file cannot be resolved by any mode: under
    // insert it aborts, and under upsert the second row silently wins. Either
    // way the file is wrong, so say so.
    let key = upsert_key(table, cfg)?;
    if !key.is_empty() {
        let positions: Vec<usize> = key
            .iter()
            .filter_map(|k| columns.iter().position(|c| c.name == *k))
            .collect();
        if positions.len() == key.len() {
            let mut seen = std::collections::BTreeMap::new();
            for (i, row) in rows.iter().enumerate() {
                let k: Vec<String> = positions
                    .iter()
                    .map(|p| row[*p].to_text().unwrap_or_else(|| "NULL".into()))
                    .collect();
                if let Some(first) = seen.insert(k.clone(), i + 1) {
                    bail!(
                        "table {}: rows {first} and {} share the key ({}); the seed file has \
                         duplicates",
                        cfg.id,
                        i + 1,
                        k.join(", ")
                    );
                }
            }
        }
    }

    Ok(())
}

/// Empty every `truncate_first` table, children before parents.
///
/// This has to be a separate pass in reverse load order: emptying a parent while
/// its children still hold rows violates the foreign key, so it cannot be done
/// inline just before each table's inserts.
///
/// `order` is the insert order; this walks it backwards.
pub async fn delete_pass(
    conn: &mut PinnedConn<'_>,
    dialect: &dyn Dialect,
    order: &[TableId],
    truncating: &[TableId],
) -> Result<std::collections::BTreeMap<TableId, u64>> {
    let mut deleted = std::collections::BTreeMap::new();
    for id in order.iter().rev() {
        if !truncating.contains(id) {
            continue;
        }
        let n = conn
            .execute(&dialect.delete_all(id))
            .await
            .with_context(|| format!("emptying {id} before loading it"))?;
        deleted.insert(id.clone(), n);
    }
    Ok(deleted)
}

/// Load one table's rows on an already-open connection.
///
/// Any `truncate_first` emptying must already have happened; see [`delete_pass`].
pub async fn load_table(
    conn: &mut PinnedConn<'_>,
    dialect: &dyn Dialect,
    table: &Table,
    columns: &[&Column],
    rows: &[Vec<Value>],
    cfg: &ResolvedTable,
) -> Result<TableResult> {
    let key = upsert_key(table, cfg)?;

    // A bulk load has nowhere to put a conflict clause, so it only applies to
    // the modes that have none.
    let bulk = matches!(cfg.load_mode, LoadMode::Insert | LoadMode::TruncateFirst)
        .then(|| dialect.copy_in_statement(table, columns))
        .flatten();
    if let Some(sql) = bulk {
        let mut payload = crate::io::LineBuffer::new();
        for row in rows {
            let fields: Vec<String> = row.iter().map(crate::format::csv::encode_field).collect();
            payload.push_line(&crate::format::csv::encode_record(&fields));
        }
        let affected = conn
            .copy_in(&sql, &payload.finish())
            .await
            .with_context(|| format!("bulk loading {}", cfg.id))?;
        return Ok(TableResult {
            table: table.id.clone(),
            mode: cfg.load_mode,
            rows: rows.len() as u64,
            affected,
            deleted: 0,
        });
    }

    let sql = insert_statement(dialect, table, columns, &key, cfg.load_mode)?;
    let mut affected = 0;
    for (i, row) in rows.iter().enumerate() {
        let binds: Vec<Option<String>> = columns
            .iter()
            .zip(row)
            .map(|(col, v)| dialect.bind_text(col, v))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("table {}: row {}", cfg.id, i + 1))?;
        affected += conn
            .execute_with(&sql, &binds)
            .await
            .with_context(|| format!("table {}: inserting row {}", cfg.id, i + 1))?;
    }

    Ok(TableResult {
        table: table.id.clone(),
        mode: cfg.load_mode,
        rows: rows.len() as u64,
        affected,
        deleted: 0,
    })
}

/// Columns whose MySQL `AUTO_INCREMENT` needs resetting after a load.
///
/// MySQL cannot parameterise an `AUTO_INCREMENT` assignment, so the value is
/// read first and substituted into DDL, and `ALTER TABLE` commits implicitly,
/// so the pair runs after the load's transaction.
pub fn mysql_fixups(table: &Table) -> Vec<(TableId, String)> {
    table
        .identity_columns()
        .map(|c| (table.id.clone(), c.name.clone()))
        .collect()
}

/// Read the next `AUTO_INCREMENT` value for a table, then set it.
pub async fn fix_mysql_auto_increment(
    db: &crate::db::Db,
    id: &TableId,
    column: &str,
) -> Result<()> {
    let rows = db
        .query_text(&crate::db::mysql::next_auto_increment_sql(id, column))
        .await
        .with_context(|| format!("reading the next AUTO_INCREMENT for {id}"))?;
    let next = rows
        .first()
        .and_then(|r| r.first().cloned().flatten())
        .unwrap_or_else(|| "1".to_string());
    let stmt = crate::db::mysql::set_auto_increment_sql(id, next.trim())?;
    db.execute(&stmt)
        .await
        .with_context(|| format!("setting AUTO_INCREMENT on {id}"))?;
    Ok(())
}

/// Refuse to write to a source marked read-only.
pub fn check_writable(src: &crate::source::ResolvedSource) -> Result<()> {
    if src.read_only {
        bail!(
            "source {:?} is marked `read_only: true` in the config, so seedle will not write to \
             it.\n\
             Pass --source with a writable source, or remove the flag if this really is the \
             target.",
            src.name
        );
    }
    Ok(())
}

/// Whether a load needs explicit confirmation before proceeding.
///
/// Two independent reasons: the target is not this machine, or the plan destroys
/// existing rows.
pub fn confirmation_reasons(
    src: &crate::source::ResolvedSource,
    plans: &[TablePlan],
) -> Vec<String> {
    // One entry per thing to accept, so agreeing to empty one table is never
    // taken as agreeing to empty another.
    let mut reasons = Vec::new();
    if !src.is_local() {
        reasons.push(format!(
            "source {:?} is not local ({})",
            src.name,
            src.redacted_url()
        ));
    }
    for p in plans.iter().filter(|p| p.mode == LoadMode::TruncateFirst) {
        reasons.push(format!(
            "{} will be emptied first, discarding every row it holds",
            p.table
        ));
    }
    reasons
}

/// Render a plan as the `seedle plan` output.
/// Draw the plan as a dependency tree, roots first.
///
/// The flat plan says what order tables load in; this says why. A table appears
/// under each parent it needs, and a table already shown deeper up the tree is
/// marked rather than expanded again, so a diamond does not print twice.
pub fn render_tree(source_name: &str, plans: &[TablePlan], schema: &Schema) -> String {
    if plans.is_empty() {
        return "nothing to load\n".to_string();
    }

    let included: BTreeSet<&TableId> = plans.iter().map(|p| &p.table).collect();
    let rows: BTreeMap<&TableId, u64> = plans.iter().map(|p| (&p.table, p.rows)).collect();

    // children[parent] = tables that reference it, within the plan.
    let mut children: BTreeMap<&TableId, BTreeSet<&TableId>> = BTreeMap::new();
    let mut has_parent: BTreeSet<&TableId> = BTreeSet::new();
    for p in plans {
        let Some(table) = schema.get(&p.table) else {
            continue;
        };
        for fk in &table.foreign_keys {
            let Some(parent) = schema.resolve(&fk.references) else {
                continue;
            };
            if parent == p.table {
                continue;
            }
            if let Some(parent) = included.iter().find(|id| ***id == parent) {
                children.entry(parent).or_default().insert(&p.table);
                has_parent.insert(&p.table);
            }
        }
    }

    let mut out = format!("load order for source {source_name:?}:\n");
    let mut drawn: BTreeSet<&TableId> = BTreeSet::new();
    let roots: Vec<&TableId> = plans
        .iter()
        .map(|p| &p.table)
        .filter(|id| !has_parent.contains(id))
        .collect();

    for root in roots {
        draw(&mut out, root, &children, &rows, &mut drawn, "", true);
    }
    // Anything left is inside a cycle, which has no root to start from.
    for p in plans {
        if !drawn.contains(&p.table) {
            draw(&mut out, &p.table, &children, &rows, &mut drawn, "", true);
        }
    }

    let total: u64 = plans.iter().map(|p| p.rows).sum();
    out.push_str(&format!(
        "\n{} table{}, {total} row{} total\n",
        plans.len(),
        if plans.len() == 1 { "" } else { "s" },
        if total == 1 { "" } else { "s" }
    ));
    out
}

fn draw<'a>(
    out: &mut String,
    id: &'a TableId,
    children: &BTreeMap<&'a TableId, BTreeSet<&'a TableId>>,
    rows: &BTreeMap<&'a TableId, u64>,
    drawn: &mut BTreeSet<&'a TableId>,
    prefix: &str,
    last: bool,
) {
    let branch = if prefix.is_empty() {
        String::new()
    } else if last {
        format!("{prefix}`- ")
    } else {
        format!("{prefix}|- ")
    };
    let count = rows.get(id).copied().unwrap_or(0);

    if !drawn.insert(id) {
        out.push_str(&format!("{branch}{id} (above)\n"));
        return;
    }
    out.push_str(&format!("{branch}{id}  {count} rows\n"));

    let kids: Vec<&TableId> = children.get(id).into_iter().flatten().copied().collect();
    let child_prefix = if prefix.is_empty() {
        "  ".to_string()
    } else if last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}|  ")
    };
    for (i, child) in kids.iter().enumerate() {
        draw(
            out,
            child,
            children,
            rows,
            drawn,
            &child_prefix,
            i + 1 == kids.len(),
        );
    }
}

pub fn render_plan(source_name: &str, plans: &[TablePlan]) -> String {
    if plans.is_empty() {
        return "nothing to load\n".to_string();
    }
    let width = plans
        .iter()
        .map(|p| p.table.to_string().chars().count())
        .max()
        .unwrap_or(0);
    let mut out = format!("load order for source {source_name:?}:\n");
    for (i, p) in plans.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}. {:width$}  {:>7} rows  {}{}\n",
            i + 1,
            p.table.to_string(),
            p.rows,
            p.mode.as_str(),
            if p.mode == LoadMode::Upsert && !p.key.is_empty() {
                format!(" on ({})", p.key.join(", "))
            } else {
                String::new()
            },
            width = width
        ));
    }
    let total: u64 = plans.iter().map(|p| p.rows).sum();
    out.push_str(&format!(
        "\n{} table{}, {total} row{} total\n",
        plans.len(),
        if plans.len() == 1 { "" } else { "s" },
        if total == 1 { "" } else { "s" }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Engine;
    use crate::config::{Format, JsonMode, Layout};
    use crate::dialect::{Mysql, Postgres, Sqlite};
    use crate::schema::TypeClass;

    /// `sql_type` mirrors what `format_type()` reports, since that is what the
    /// write path casts to, it is not the same string as the class label.
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

    fn users() -> Table {
        Table {
            id: TableId::new("public", "users"),
            columns: vec![
                Column {
                    nullable: false,
                    identity: true,
                    ..col("id", "bigint", TypeClass::Int { bits: 64 })
                },
                col(
                    "email",
                    "varchar(20)",
                    TypeClass::Text { max_len: Some(20) },
                ),
            ],
            primary_key: vec!["id".into()],
            unique: vec![vec!["email".into()]],
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
            format: Format::Jsonl,
            layout: Layout::Single,
            pretty: false,
            json: JsonMode::Unroll,
            load_mode: mode,
            key: None,
        }
    }

    fn refs(t: &Table) -> Vec<&Column> {
        t.columns.iter().collect()
    }

    #[test]
    fn postgres_insert_casts_every_placeholder_to_the_column_type() {
        let t = users();
        let sql =
            insert_statement(&Postgres, &t, &refs(&t), &["id".into()], LoadMode::Insert).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"users\" (\"id\", \"email\") \
             VALUES (CAST($1 AS bigint), CAST($2 AS varchar(20)))"
        );
    }

    #[test]
    fn upsert_adds_the_conflict_clause() {
        let t = users();
        let sql =
            insert_statement(&Postgres, &t, &refs(&t), &["id".into()], LoadMode::Upsert).unwrap();
        assert!(
            sql.ends_with("ON CONFLICT (\"id\") DO UPDATE SET \"email\" = EXCLUDED.\"email\""),
            "{sql}"
        );
    }

    #[test]
    fn mysql_insert_uses_positional_placeholders() {
        let t = users();
        let sql =
            insert_statement(&Mysql, &t, &refs(&t), &["id".into()], LoadMode::Upsert).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO `public`.`users` (`id`, `email`) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE `email` = VALUES(`email`)"
        );
    }

    #[test]
    fn truncate_first_mode_emits_a_plain_insert() {
        let t = users();
        let sql = insert_statement(
            &Postgres,
            &t,
            &refs(&t),
            &["id".into()],
            LoadMode::TruncateFirst,
        )
        .unwrap();
        assert!(
            !sql.contains("ON CONFLICT"),
            "the table is empty, so there can be no conflict: {sql}"
        );
    }

    // -- key resolution -----------------------------------------------------

    #[test]
    fn key_defaults_to_the_primary_key() {
        assert_eq!(
            upsert_key(&users(), &cfg(LoadMode::Upsert)).unwrap(),
            ["id"]
        );
    }

    #[test]
    fn key_override_must_be_backed_by_a_constraint() {
        let mut c = cfg(LoadMode::Upsert);
        c.key = Some(vec!["email".into()]);
        // Backed by a unique constraint: fine.
        assert_eq!(upsert_key(&users(), &c).unwrap(), ["email"]);

        // Not backed by anything: rejected here, with the real constraints
        // listed, rather than as an opaque database error.
        let mut t = users();
        t.unique.clear();
        let err = upsert_key(&t, &c).unwrap_err().to_string();
        assert!(err.contains("not backed by"), "{err}");
        assert!(
            err.contains("(id)"),
            "the error should list the real key: {err}"
        );
    }

    #[test]
    fn key_override_naming_a_missing_column_is_rejected() {
        let mut c = cfg(LoadMode::Upsert);
        c.key = Some(vec!["ghost".into()]);
        assert!(
            upsert_key(&users(), &c)
                .unwrap_err()
                .to_string()
                .contains("ghost")
        );
    }

    // -- row validation -----------------------------------------------------

    #[test]
    fn a_null_in_a_not_null_column_is_caught_before_the_transaction_opens() {
        let t = users();
        let rows = vec![vec![Value::Null, Value::Text("a".into())]];
        let err = validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Insert))
            .unwrap_err()
            .to_string();
        assert!(err.contains("null in \"id\""), "{err}");
        assert!(err.contains("NOT NULL"), "{err}");
    }

    #[test]
    fn a_null_is_allowed_when_the_column_has_a_default() {
        let mut t = users();
        t.columns[0].has_default = true;
        let rows = vec![vec![Value::Null, Value::Text("a".into())]];
        assert!(validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Insert)).is_ok());
    }

    #[test]
    fn overlong_text_is_caught_by_character_count_not_byte_count() {
        let t = users();
        // 21 characters, over the varchar(20) limit.
        let rows = vec![vec![Value::Int(1), Value::Text("x".repeat(21))]];
        let err = validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Insert))
            .unwrap_err()
            .to_string();
        assert!(err.contains("21 characters"), "{err}");

        // 20 multi-byte characters is 60 bytes but still fits.
        let rows = vec![vec![Value::Int(1), Value::Text("🌱".repeat(20))]];
        assert!(
            validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Insert)).is_ok(),
            "varchar length is counted in characters, not bytes"
        );
    }

    #[test]
    fn duplicate_keys_within_one_file_are_rejected() {
        let t = users();
        let rows = vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
            vec![Value::Int(1), Value::Text("c".into())],
        ];
        let err = validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Upsert))
            .unwrap_err()
            .to_string();
        assert!(err.contains("rows 1 and 3"), "{err}");
        assert!(err.contains("duplicates"), "{err}");
    }

    #[test]
    fn distinct_keys_pass_validation() {
        let t = users();
        let rows = vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ];
        assert!(validate_rows(&t, &refs(&t), &rows, &cfg(LoadMode::Upsert)).is_ok());
    }

    #[test]
    fn no_rows_validates_trivially() {
        let t = users();
        assert!(validate_rows(&t, &refs(&t), &[], &cfg(LoadMode::Upsert)).is_ok());
    }

    // -- sequences and constraints -----------------------------------------

    #[test]
    fn identity_columns_get_a_sequence_fixup() {
        let fixups = Postgres.sequence_fixups(&users());
        assert_eq!(fixups.len(), 1);
        assert!(fixups[0].contains("setval"), "{}", fixups[0]);

        // MySQL needs a read-then-ALTER pair instead, so it produces no
        // in-transaction statement.
        assert!(Mysql.sequence_fixups(&users()).is_empty());
        assert_eq!(mysql_fixups(&users()).len(), 1);
        assert_eq!(mysql_fixups(&users())[0].1, "id");

        assert!(Sqlite.sequence_fixups(&users())[0].contains("sqlite_sequence"));
    }

    #[test]
    fn a_table_without_an_identity_column_needs_no_fixup() {
        let mut t = users();
        t.columns[0].identity = false;
        assert!(Postgres.sequence_fixups(&t).is_empty());
        assert!(Sqlite.sequence_fixups(&t).is_empty());
    }

    #[test]
    fn only_mysql_defers_its_fixup_past_the_commit() {
        // ALTER TABLE commits implicitly on MySQL, so running it inside the
        // transaction would silently break atomicity.
        assert!(Mysql.fixup_after_commit());
        assert!(!Postgres.fixup_after_commit());
        assert!(!Sqlite.fixup_after_commit());
    }

    #[test]
    fn postgres_only_defers_constraints_that_are_actually_deferrable() {
        assert_eq!(
            Postgres.defer_constraints(true),
            Some("SET CONSTRAINTS ALL DEFERRED")
        );
        assert_eq!(
            Postgres.defer_constraints(false),
            None,
            "asking Postgres to defer a non-deferrable constraint is an error"
        );
    }

    #[test]
    fn the_other_engines_can_always_suspend_key_checking() {
        assert_eq!(
            Mysql.defer_constraints(false),
            Some("SET FOREIGN_KEY_CHECKS = 0")
        );
        assert_eq!(
            Mysql.restore_constraints(),
            Some("SET FOREIGN_KEY_CHECKS = 1")
        );
        assert_eq!(
            Sqlite.defer_constraints(false),
            Some("PRAGMA defer_foreign_keys = ON")
        );
        // Postgres defers only within the transaction, so nothing to undo.
        assert_eq!(Postgres.restore_constraints(), None);
        assert_eq!(Sqlite.restore_constraints(), None);
    }

    // -- guard rails --------------------------------------------------------

    fn source(name: &str, url: &str, read_only: bool) -> crate::source::ResolvedSource {
        crate::source::ResolvedSource {
            name: name.into(),
            engine: Engine::Postgres,
            url: url.into(),
            read_only,
            origin: "test".into(),
            storage: None,
        }
    }

    #[test]
    fn a_read_only_source_is_refused_with_the_fix_named() {
        let err = check_writable(&source("prod", "postgres://p/app", true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("read_only"), "{err}");
        assert!(err.contains("--source"), "{err}");
        assert!(check_writable(&source("dev", "postgres://localhost/app", false)).is_ok());
    }

    #[test]
    fn a_local_non_destructive_load_needs_no_confirmation() {
        let plans = vec![TablePlan {
            table: TableId::bare("users"),
            mode: LoadMode::Upsert,
            rows: 3,
            columns: vec!["id".into()],
            key: vec!["id".into()],
        }];
        assert!(
            confirmation_reasons(&source("dev", "postgres://localhost/app", false), &plans)
                .is_empty()
        );
    }

    #[test]
    fn a_remote_target_and_a_truncate_are_each_a_reason_to_confirm() {
        let safe = vec![TablePlan {
            table: TableId::bare("users"),
            mode: LoadMode::Upsert,
            rows: 1,
            columns: vec![],
            key: vec![],
        }];
        let destructive = vec![TablePlan {
            table: TableId::bare("settings"),
            mode: LoadMode::TruncateFirst,
            rows: 1,
            columns: vec![],
            key: vec![],
        }];

        let remote = source("staging", "postgres://u:pw@staging.example.com/app", false);
        let local = source("dev", "postgres://localhost/app", false);

        let r = confirmation_reasons(&remote, &safe);
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("not local"), "{r:?}");
        // And the reason must not leak the password.
        assert!(
            !r[0].contains("pw"),
            "credentials leaked into output: {r:?}"
        );

        let r = confirmation_reasons(&local, &destructive);
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("emptied first"), "{r:?}");

        assert_eq!(confirmation_reasons(&remote, &destructive).len(), 2);
    }

    #[test]
    fn plan_output_lists_order_rows_and_mode() {
        let plans = vec![
            TablePlan {
                table: TableId::new("public", "orgs"),
                mode: LoadMode::Upsert,
                rows: 3,
                columns: vec!["id".into()],
                key: vec!["id".into()],
            },
            TablePlan {
                table: TableId::new("public", "users"),
                mode: LoadMode::TruncateFirst,
                rows: 10,
                columns: vec!["id".into()],
                key: vec![],
            },
        ];
        // A plan is rendered from the ordered list, so orgs must precede users.
        let out = render_plan("dev", &plans);
        assert!(out.contains("1. public.orgs"), "{out}");
        assert!(out.contains("2. public.users"), "{out}");
        assert!(out.contains("upsert on (id)"), "{out}");
        assert!(out.contains("truncate_first"), "{out}");
        assert!(out.contains("2 tables, 13 rows total"), "{out}");
    }

    #[test]
    fn an_empty_plan_says_so() {
        assert_eq!(render_plan("dev", &[]), "nothing to load\n");
    }
}

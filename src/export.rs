//! Pulling rows out of a source database into seed files.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{Config, ResolvedTable};
use crate::db::Db;
use crate::dialect::Dialect;
use crate::format;
use crate::schema::{Column, Schema, Table};
use crate::value::Value;

/// What one table's export produced.
#[derive(Debug, Clone)]
pub struct TableExport {
    pub table: crate::schema::TableId,
    pub rows: u64,
    pub files: Vec<format::Output>,
}

/// Columns of `table` that `cfg` selects, in the schema's column order.
///
/// Order comes from the schema, never from the config, so two configs listing
/// the same columns differently still produce identical files. Generated columns
/// are always dropped: they cannot be written back, so exporting them would
/// produce a file that cannot be loaded.
pub fn selected_columns<'a>(table: &'a Table, cfg: &ResolvedTable) -> Result<Vec<&'a Column>> {
    if let Some(wanted) = &cfg.columns {
        for name in wanted {
            let Some(col) = table.column(name) else {
                bail!(
                    "table {}: `columns` names {name:?}, which the table does not have.\n\
                     Available: {}",
                    cfg.id,
                    column_list(table)
                );
            };
            if col.generated {
                bail!(
                    "table {}: `columns` names the generated column {name:?}, which cannot be \
                     written back",
                    cfg.id
                );
            }
        }
    }
    for name in &cfg.exclude_columns {
        if table.column(name).is_none() {
            bail!(
                "table {}: `exclude_columns` names {name:?}, which the table does not have.\n\
                 Available: {}",
                cfg.id,
                column_list(table)
            );
        }
    }

    let picked: Vec<&Column> = table
        .writable_columns()
        .filter(|c| match &cfg.columns {
            Some(wanted) => wanted.contains(&c.name),
            None => !cfg.exclude_columns.contains(&c.name),
        })
        .collect();

    if picked.is_empty() {
        bail!(
            "table {}: every column is excluded, so there would be nothing to export",
            cfg.id
        );
    }

    // A row that cannot be identified cannot be upserted or given a stable
    // per-row filename. Warn by failing early rather than producing files that
    // only break later.
    if !table.primary_key.is_empty() {
        let missing: Vec<&String> = table
            .primary_key
            .iter()
            .filter(|k| !picked.iter().any(|c| c.name == **k))
            .collect();
        if !missing.is_empty() && matches!(cfg.load_mode, crate::config::LoadMode::Upsert) {
            bail!(
                "table {}: the primary key column{} {} {} excluded, but the table's load mode is \
                 `upsert`, which needs the key to conflict on.\n\
                 Either keep the key columns or set `load: insert` for this table.",
                cfg.id,
                if missing.len() == 1 { "" } else { "s" },
                missing
                    .iter()
                    .map(|m| format!("{m:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if missing.len() == 1 { "is" } else { "are" }
            );
        }
    }

    Ok(picked)
}

fn column_list(table: &Table) -> String {
    table
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the `SELECT` for a table.
///
/// The critical part is the `ORDER BY`: it always ends up a *total* order, so
/// two exports of unchanged data are byte-identical. Without that, rows come
/// back in whatever order the storage engine felt like, and every export
/// produces diff noise.
pub fn build_query(
    dialect: &dyn Dialect,
    table: &Table,
    columns: &[&Column],
    cfg: &ResolvedTable,
) -> Result<String> {
    let select = columns
        .iter()
        .map(|c| dialect.read_expr(c))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!("SELECT {select} FROM {}", dialect.quote_table(&table.id));

    if let Some(filter) = &cfg.filter {
        // Raw SQL by design: `where:` is an escape hatch for the source dialect,
        // and it comes from the config file, not from data. Parenthesised so a
        // predicate containing OR cannot change the shape of the statement.
        sql.push_str(&format!(" WHERE ({filter})"));
    }

    for key in &cfg.order_by {
        if table.column(key).is_none() {
            bail!(
                "table {}: `order_by` names {key:?}, which the table does not have.\n\
                 Available: {}",
                cfg.id,
                column_list(table)
            );
        }
    }

    let order = total_order(table, columns, cfg);
    if !order.is_empty() {
        let terms: Vec<String> = order
            .iter()
            .map(|name| match table.column(name) {
                Some(col) => dialect.order_expr(col),
                None => dialect.quote_ident(name),
            })
            .collect();
        sql.push_str(&format!(" ORDER BY {}", terms.join(", ")));
    }

    if let Some(limit) = cfg.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    Ok(sql)
}

/// The ordering columns: the configured ones first, then the primary key, then
/// every remaining exported column — deduped, so the result is a total order.
///
/// Appending the rest is what guarantees determinism: ordering by a non-unique
/// column alone leaves ties, and ties are resolved differently run to run.
pub fn total_order(table: &Table, columns: &[&Column], cfg: &ResolvedTable) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let push = |name: &str, order: &mut Vec<String>| {
        if !order.iter().any(|o| o == name) {
            order.push(name.to_string());
        }
    };

    for key in &cfg.order_by {
        push(key, &mut order);
    }
    for key in &table.primary_key {
        // Only orderable if we are actually selecting it.
        if columns.iter().any(|c| c.name == *key) {
            push(key, &mut order);
        }
    }
    for col in columns {
        // Ordering by a json or array column is either illegal or
        // collation-dependent, so those are left out; the columns above are
        // normally enough, and a table whose only distinguishing column is json
        // is pathological.
        if col.class.needs_text_cast() || matches!(col.class, crate::schema::TypeClass::Json { .. })
        {
            continue;
        }
        push(&col.name, &mut order);
    }
    order
}

/// Export one table.
pub async fn export_table(
    db: &Db,
    schema: &Schema,
    cfg: &ResolvedTable,
    sql_batch: usize,
) -> Result<TableExport> {
    let id = schema.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
    let table = schema
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("table {} is not in the schema", cfg.id))?;

    let columns = selected_columns(table, cfg)?;
    let classes: Vec<_> = columns.iter().map(|c| c.class.clone()).collect();
    let query = build_query(db.dialect(), table, &columns, cfg)?;

    let mut writer = format::writer(
        table,
        columns.clone(),
        cfg,
        db.dialect(),
        &schema.default_schema,
        sql_batch,
    )?;

    let mut rows = 0u64;
    let mut decode_error = None;
    db.for_each_text_row(&query, |text_row| {
        if text_row.len() != classes.len() {
            bail!(
                "query for {} returned {} columns, expected {}",
                cfg.id,
                text_row.len(),
                classes.len()
            );
        }
        let values: Vec<Value> = match classes
            .iter()
            .zip(&text_row)
            .enumerate()
            .map(|(i, (class, text))| {
                Value::parse(class, text.as_deref())
                    .with_context(|| format!("row {}, column {:?}", rows + 1, columns[i].name))
            })
            .collect::<Result<Vec<_>>>()
        {
            Ok(v) => v,
            Err(e) => {
                decode_error = Some(e);
                return Ok(());
            }
        };
        writer.write_row(&values)?;
        rows += 1;
        Ok(())
    })
    .await
    .with_context(|| format!("exporting {}", cfg.id))?;

    if let Some(e) = decode_error {
        return Err(e).with_context(|| format!("exporting {}", cfg.id));
    }

    Ok(TableExport {
        table: id,
        rows,
        files: writer.finish()?,
    })
}

/// Write a table's files under `out_dir`.
pub fn write_files(out_dir: &Path, export: &TableExport) -> Result<Vec<std::path::PathBuf>> {
    export
        .files
        .iter()
        .map(|f| {
            let path = out_dir.join(&f.path);
            crate::io::write_atomic(&path, &f.bytes)?;
            Ok(path)
        })
        .collect()
}

/// Combined content hash for a table, over its files in path order.
///
/// A per-row table has many files; hashing them in a fixed order gives one
/// comparable value for the lock.
pub fn content_hash(export: &TableExport) -> String {
    use sha2::{Digest, Sha256};
    let mut files: Vec<&format::Output> = export.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut h = Sha256::new();
    for f in files {
        h.update(f.path.as_bytes());
        h.update(b"\0");
        h.update(&f.bytes);
    }
    format!("{:x}", h.finalize())
}

/// The path recorded in the lock for a table: the single file, or the directory
/// for a per-row table.
pub fn lock_path(export: &TableExport, cfg: &ResolvedTable, default_schema: &str) -> String {
    match cfg.layout {
        crate::config::Layout::PerRow => format!("{}/", cfg.id.file_stem(default_schema)),
        crate::config::Layout::Single => export
            .files
            .first()
            .map(|f| f.path.clone())
            .unwrap_or_else(|| cfg.id.file_stem(default_schema)),
    }
}

/// Cap on how many tables are exported at once, honouring the config while
/// staying inside the connection pool.
pub fn concurrency(cfg: &Config, table_count: usize) -> usize {
    cfg.export.concurrency.min(table_count.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Format, JsonMode, Layout, LoadMode};
    use crate::dialect::Postgres;
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

    fn users() -> Table {
        Table {
            id: TableId::new("public", "users"),
            columns: vec![
                col("id", TypeClass::Int { bits: 64 }),
                col("email", TypeClass::Text { max_len: None }),
                col("created_at", TypeClass::Timestamp { tz: true }),
                col("prefs", TypeClass::Json { binary: true }),
            ],
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    fn cfg() -> ResolvedTable {
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
            load_mode: LoadMode::Upsert,
            key: None,
        }
    }

    fn query(t: &Table, c: &ResolvedTable) -> String {
        let cols = selected_columns(t, c).unwrap();
        build_query(&Postgres, t, &cols, c).unwrap()
    }

    #[test]
    fn selects_every_column_as_text_by_default() {
        let sql = query(&users(), &cfg());
        assert!(
            sql.starts_with(
                "SELECT \"id\"::text, \"email\"::text, \"created_at\"::text, \"prefs\"::text \
                 FROM \"public\".\"users\""
            ),
            "{sql}"
        );
    }

    #[test]
    fn order_by_is_always_a_total_order() {
        // The point: even with no configured ordering, ties are impossible.
        let sql = query(&users(), &cfg());
        assert!(
            sql.contains("ORDER BY \"id\", \"email\" COLLATE \"C\", \"created_at\""),
            "{sql}"
        );
    }

    #[test]
    fn configured_ordering_comes_first_then_the_rest_is_appended() {
        let mut c = cfg();
        c.order_by = vec!["created_at".into()];
        let sql = query(&users(), &c);
        // created_at is not unique, so the key and remaining columns must follow
        // to break ties deterministically.
        assert!(
            sql.contains("ORDER BY \"created_at\", \"id\", \"email\" COLLATE \"C\""),
            "{sql}"
        );
    }

    #[test]
    fn ordering_columns_are_not_duplicated() {
        let mut c = cfg();
        c.order_by = vec!["id".into(), "email".into()];
        let t = users();
        let cols = selected_columns(&t, &c).unwrap();
        let order = total_order(&t, &cols, &c);
        let mut sorted = order.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            order.len(),
            sorted.len(),
            "duplicate ordering columns: {order:?}"
        );
        assert_eq!(order, ["id", "email", "created_at"]);
    }

    #[test]
    fn json_columns_are_left_out_of_the_ordering() {
        // Postgres cannot order by jsonb without an operator class, and array
        // ordering is collation-dependent.
        let t = users();
        let c = cfg();
        let cols = selected_columns(&t, &c).unwrap();
        let order = total_order(&t, &cols, &c);
        assert!(!order.contains(&"prefs".to_string()), "{order:?}");
    }

    #[test]
    fn filter_is_parenthesised_so_or_cannot_widen_the_statement() {
        let mut c = cfg();
        c.filter = Some("a = 1 OR b = 2".into());
        let sql = query(&users(), &c);
        assert!(sql.contains("WHERE (a = 1 OR b = 2)"), "{sql}");
    }

    #[test]
    fn limit_is_applied_after_ordering() {
        let mut c = cfg();
        c.limit = Some(10);
        let sql = query(&users(), &c);
        let order_at = sql.find("ORDER BY").unwrap();
        let limit_at = sql.find("LIMIT 10").unwrap();
        assert!(
            order_at < limit_at,
            "a LIMIT without a prior ORDER BY is not deterministic: {sql}"
        );
    }

    #[test]
    fn exclude_columns_drops_them_from_the_select() {
        let mut c = cfg();
        c.exclude_columns = vec!["prefs".into()];
        let sql = query(&users(), &c);
        assert!(!sql.contains("prefs"), "{sql}");
        assert!(sql.contains("\"email\"::text"), "{sql}");
    }

    #[test]
    fn explicit_columns_follow_schema_order_not_config_order() {
        // Two configs naming the same columns in different orders must produce
        // byte-identical files.
        let mut a = cfg();
        a.columns = Some(vec!["email".into(), "id".into()]);
        let mut b = cfg();
        b.columns = Some(vec!["id".into(), "email".into()]);
        assert_eq!(query(&users(), &a), query(&users(), &b));
        assert!(query(&users(), &a).contains("\"id\"::text, \"email\"::text"));
    }

    #[test]
    fn generated_columns_are_never_exported() {
        let mut t = users();
        t.columns.push(Column {
            generated: true,
            ..col("search", TypeClass::Text { max_len: None })
        });
        let sql = query(&t, &cfg());
        assert!(
            !sql.contains("search"),
            "a generated column cannot be written back, so exporting it makes an unloadable file: {sql}"
        );
    }

    #[test]
    fn a_misspelled_column_is_named_in_the_error() {
        let mut c = cfg();
        c.exclude_columns = vec!["prefz".into()];
        let err = selected_columns(&users(), &c).unwrap_err().to_string();
        assert!(err.contains("prefz"), "{err}");
        assert!(
            err.contains("Available:"),
            "the error should list the real columns: {err}"
        );

        let mut c = cfg();
        c.columns = Some(vec!["nope".into()]);
        assert!(
            selected_columns(&users(), &c)
                .unwrap_err()
                .to_string()
                .contains("nope")
        );

        let mut c = cfg();
        c.order_by = vec!["nope".into()];
        let t = users();
        let cols = selected_columns(&t, &c).unwrap();
        assert!(
            build_query(&Postgres, &t, &cols, &c)
                .unwrap_err()
                .to_string()
                .contains("nope")
        );
    }

    #[test]
    fn excluding_the_key_under_upsert_fails_early_with_the_fix_named() {
        let mut c = cfg();
        c.exclude_columns = vec!["id".into()];
        let err = selected_columns(&users(), &c).unwrap_err().to_string();
        assert!(err.contains("primary key"), "{err}");
        assert!(err.contains("load: insert"), "{err}");
    }

    #[test]
    fn excluding_the_key_is_fine_for_a_plain_insert() {
        let mut c = cfg();
        c.exclude_columns = vec!["id".into()];
        c.load_mode = LoadMode::Insert;
        let t = users();
        let cols = selected_columns(&t, &c).unwrap();
        assert!(!cols.iter().any(|col| col.name == "id"));
    }

    #[test]
    fn excluding_everything_is_an_error() {
        let mut c = cfg();
        c.load_mode = LoadMode::Insert;
        c.exclude_columns = vec![
            "id".into(),
            "email".into(),
            "created_at".into(),
            "prefs".into(),
        ];
        let err = selected_columns(&users(), &c).unwrap_err().to_string();
        assert!(err.contains("nothing to export"), "{err}");
    }

    #[test]
    fn content_hash_is_order_independent_but_content_sensitive() {
        let mk = |files: Vec<format::Output>| TableExport {
            table: TableId::bare("t"),
            rows: 0,
            files,
        };
        let a = format::Output {
            path: "t/a.json".into(),
            bytes: b"1".to_vec(),
        };
        let b = format::Output {
            path: "t/b.json".into(),
            bytes: b"2".to_vec(),
        };

        assert_eq!(
            content_hash(&mk(vec![a.clone(), b.clone()])),
            content_hash(&mk(vec![b.clone(), a.clone()])),
            "file discovery order must not change the hash"
        );
        let changed = format::Output {
            path: "t/b.json".into(),
            bytes: b"3".to_vec(),
        };
        assert_ne!(
            content_hash(&mk(vec![a.clone(), b])),
            content_hash(&mk(vec![a, changed])),
            "changed content must change the hash"
        );
    }

    #[test]
    fn lock_path_names_a_directory_for_per_row_tables() {
        let mut c = cfg();
        c.layout = Layout::PerRow;
        c.format = Format::Json;
        let e = TableExport {
            table: TableId::new("public", "users"),
            rows: 2,
            files: vec![format::Output {
                path: "users/1.json".into(),
                bytes: vec![],
            }],
        };
        assert_eq!(lock_path(&e, &c, "public"), "users/");

        let c = cfg();
        let e = TableExport {
            table: TableId::new("public", "users"),
            rows: 2,
            files: vec![format::Output {
                path: "users.jsonl".into(),
                bytes: vec![],
            }],
        };
        assert_eq!(lock_path(&e, &c, "public"), "users.jsonl");
    }

    #[test]
    fn non_default_schemas_are_qualified_in_filenames() {
        let mut c = cfg();
        c.id = TableId::new("audit", "events");
        let e = TableExport {
            table: c.id.clone(),
            rows: 0,
            files: vec![],
        };
        assert_eq!(lock_path(&e, &c, "public"), "audit.events");
    }
}

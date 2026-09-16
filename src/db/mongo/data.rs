//! Reading collections into seed files and writing them back.
//!
//! Both reuse the relational format writers and readers unchanged: a document
//! becomes a row in the schema's column order, so `jsonl` output looks the same
//! as any other table's and the same reader loads it.

use anyhow::{Context, Result, bail};
use bson::Document;

use super::{ops, value};
use crate::config::{LoadMode, ResolvedTable};
use crate::db::Db;
use crate::export::TableExport;
use crate::load::TableResult;
use crate::schema::Schema;

/// Export one collection, streaming documents into the configured writer.
pub async fn export_collection(
    db: &Db,
    schema: &Schema,
    cfg: &ResolvedTable,
    out_dir: &std::path::Path,
    mut tick: impl FnMut(u64),
) -> Result<TableExport> {
    let id = schema.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
    let table = schema
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("collection {} is not in the schema", cfg.id))?;
    let columns = crate::export::selected_columns(table, cfg)?;

    // `where:` is a JSON filter document here, not a SQL predicate.
    let filter = match &cfg.filter {
        None => None,
        Some(text) => Some(parse_filter(text, &cfg.config_key)?),
    };
    let docs = ops::find_sorted(db, &id.name, filter, cfg.limit.map(|n| n as i64)).await?;

    let mut writer = crate::format::writer(
        table,
        columns.clone(),
        cfg,
        // A document engine has no dialect, and only the `.sql` format needs
        // one, which validation refuses for Mongo.
        &crate::dialect::Postgres,
        &schema.default_schema,
        1,
        out_dir,
    )?;

    let mut rows = 0u64;
    for doc in &docs {
        let row = value::document_to_row(doc, &columns)
            .with_context(|| format!("exporting {}", cfg.id))?;
        writer.write_row(&row)?;
        rows += 1;
        tick(rows);
    }

    let written = writer.finish()?;
    Ok(TableExport {
        table: id,
        rows,
        paths: written.paths,
        sha256: written.sha256,
        // Mongo enforces no references, so there is nothing for the referential
        // check to index.
        keys: Default::default(),
    })
}

/// Load one collection's seed rows back as documents.
pub async fn load_collection(
    db: &Db,
    schema: &Schema,
    cfg: &ResolvedTable,
    rows: &[Vec<crate::value::Value>],
) -> Result<TableResult> {
    let id = schema.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
    let table = schema
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("collection {} is not in the schema", cfg.id))?;
    let columns = crate::export::selected_columns(table, cfg)?;

    let deleted = if cfg.load_mode == LoadMode::TruncateFirst {
        ops::delete_all(db, &id.name).await?
    } else {
        0
    };

    let docs: Vec<Document> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            value::row_to_document(row, &columns)
                .with_context(|| format!("{}: document {}", cfg.id, i + 1))
        })
        .collect::<Result<_>>()?;

    let mode = match cfg.load_mode {
        LoadMode::Upsert => ops::Write::Replace,
        LoadMode::Insert | LoadMode::TruncateFirst => ops::Write::Insert,
        LoadMode::SkipExisting => ops::Write::SetOnInsert,
    };
    let affected = ops::write(db, &id.name, &docs, mode).await?;

    Ok(TableResult {
        table: id,
        mode: cfg.load_mode,
        rows: rows.len() as u64,
        affected,
        deleted,
    })
}

/// A `where:` filter, which is a JSON document rather than a SQL predicate.
fn parse_filter(text: &str, table: &str) -> Result<Document> {
    let json: serde_json::Value = serde_json::from_str(text).with_context(|| {
        format!(
            "table {table}: on MongoDB `where` is a filter document, not SQL. \
             Write it as JSON, e.g. {{\"status\": {{\"$ne\": \"draft\"}}}}"
        )
    })?;
    match bson::Bson::try_from(json).context("converting the filter to BSON")? {
        bson::Bson::Document(d) => Ok(d),
        _ => bail!("table {table}: `where` must be a JSON object"),
    }
}

/// Live document counts, for `graine status`.
pub async fn count(db: &Db, collection: &str) -> Result<u64> {
    ops::count(db, collection).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_is_json_not_sql() {
        let err = parse_filter("status <> 'draft'", "orders")
            .unwrap_err()
            .to_string();
        assert!(err.contains("filter document"), "{err}");

        let ok = parse_filter(r#"{"status": {"$ne": "draft"}}"#, "orders").unwrap();
        assert!(ok.contains_key("status"));
    }

    #[test]
    fn a_filter_must_be_an_object() {
        assert!(parse_filter("[1, 2]", "orders").is_err());
    }
}

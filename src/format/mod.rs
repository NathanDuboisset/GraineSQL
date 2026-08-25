//! Seed file formats: writing rows out and reading them back.
//!
//! The writers and readers here are exact inverses for every format except
//! `sql`, which is deliberately write-only.

pub mod csv;
pub mod json;
pub mod sql;

use anyhow::{Result, bail};

use crate::config::{Format, Layout, ResolvedTable};
use crate::schema::{Column, Table};
use crate::value::Value;

/// One file produced by a writer: a path relative to the output directory, and
/// its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Incremental row sink, so a large table never has to be held in memory.
pub trait RowWriter {
    fn write_row(&mut self, row: &[Value]) -> Result<()>;
    /// Consume the writer and produce the files: one entry for a single-file
    /// layout, one per row for `layout: per_row`.
    fn finish(self: Box<Self>) -> Result<Vec<Output>>;
}

/// Build the writer for a table's configured format and layout.
pub fn writer<'a>(
    table: &'a Table,
    columns: Vec<&'a Column>,
    cfg: &'a ResolvedTable,
    dialect: &'a dyn crate::dialect::Dialect,
    default_schema: &str,
    sql_batch: usize,
) -> Result<Box<dyn RowWriter + 'a>> {
    let stem = cfg.id.file_stem(default_schema);
    Ok(match (cfg.layout, cfg.format) {
        (Layout::Single, Format::Jsonl | Format::JsonlGz) => Box::new(json::JsonlWriter::new(
            format!("{stem}.{}", cfg.format.extension()),
            columns,
            cfg.json,
            cfg.format.compressed(),
        )),
        (Layout::Single, Format::Csv) => {
            Box::new(csv::CsvWriter::new(format!("{stem}.csv"), columns))
        }
        (Layout::Single, Format::Sql) => Box::new(sql::SqlWriter::new(
            format!("{stem}.sql"),
            table,
            columns,
            cfg,
            dialect,
            sql_batch,
        )?),
        (Layout::PerRow, Format::Json) => Box::new(json::PerRowWriter::new(
            stem, table, columns, cfg.json, cfg.pretty,
        )),
        (layout, format) => bail!(
            "table {}: {layout:?} layout with format {} is not a valid combination",
            cfg.id,
            format.extension()
        ),
    })
}

/// Read a table's rows back from its seed file(s).
pub fn read(
    dir: &std::path::Path,
    columns: &[&Column],
    cfg: &ResolvedTable,
    default_schema: &str,
    engine: crate::config::Engine,
) -> Result<Vec<Vec<Value>>> {
    let stem = cfg.id.file_stem(default_schema);
    match (cfg.layout, cfg.format) {
        (Layout::Single, Format::Jsonl | Format::JsonlGz) => json::read_jsonl(
            &dir.join(format!("{stem}.{}", cfg.format.extension())),
            columns,
            cfg.format.compressed(),
        ),
        (Layout::Single, Format::Csv) => csv::read_csv(&dir.join(format!("{stem}.csv")), columns),
        (Layout::PerRow, Format::Json) => json::read_per_row(&dir.join(&stem), columns),
        (Layout::Single, Format::Sql) => {
            sql::read_sql(&dir.join(format!("{stem}.sql")), columns, engine)
        }
        (layout, format) => bail!(
            "table {}: {layout:?} layout with format {} is not a valid combination",
            cfg.id,
            format.extension()
        ),
    }
}

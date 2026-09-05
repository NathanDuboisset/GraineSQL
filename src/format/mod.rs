//! Seed file formats: writing rows out and reading them back.
//!
//! The writers and readers here are exact inverses for every format except
//! `sql`, which is deliberately write-only.

pub mod csv;
pub mod json;
pub mod sql;

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{Format, Layout, ResolvedTable};
use crate::schema::{Column, Table};
use crate::value::Value;

/// What a table's writer committed to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Paths relative to the output directory, in the order they were written.
    pub paths: Vec<String>,
    /// The table's content hash, per [`crate::lock::FileEntry::sha256`].
    pub sha256: String,
}

/// Incremental row sink, so a large table never has to be held in memory.
pub trait RowWriter {
    fn write_row(&mut self, row: &[Value]) -> Result<()>;
    /// Commit whatever is open and report what landed on disk.
    fn finish(self: Box<Self>) -> Result<Written>;
}

/// Build the writer for a table's configured format and layout.
///
/// The output file is opened here rather than at the first row, so a
/// permissions or ENOSPC failure surfaces before the query runs.
pub fn writer<'a>(
    table: &'a Table,
    columns: Vec<&'a Column>,
    cfg: &'a ResolvedTable,
    dialect: &'a dyn crate::dialect::Dialect,
    default_schema: &str,
    sql_batch: usize,
    out_dir: &Path,
) -> Result<Box<dyn RowWriter + 'a>> {
    let stem = cfg.id.file_stem(default_schema);
    Ok(match (cfg.layout, cfg.format) {
        (Layout::Single, Format::Jsonl | Format::JsonlGz) => Box::new(json::JsonlWriter::new(
            out_dir,
            format!("{stem}.{}", cfg.format.extension()),
            columns,
            cfg.json,
            cfg.format.compressed(),
        )?),
        (Layout::Single, Format::Csv) => Box::new(csv::CsvWriter::new(
            out_dir,
            format!("{stem}.csv"),
            columns,
        )?),
        (Layout::Single, Format::Sql) => Box::new(sql::SqlWriter::new(
            out_dir,
            format!("{stem}.sql"),
            table,
            columns,
            cfg,
            dialect,
            sql_batch,
        )?),
        (Layout::PerRow, Format::Json) => Box::new(json::PerRowWriter::new(
            out_dir, stem, table, columns, cfg.json, cfg.pretty,
        )?),
        (layout, format) => bail!(
            "table {}: {layout:?} layout with format {} is not a valid combination",
            cfg.id,
            format.extension()
        ),
    })
}

/// Open a table's output file with its content hash seeded by the path.
///
/// The recorded sha256 covers `path \0 bytes`, so it is deliberately not a bare
/// `sha256sum` of the file: a renamed table cannot keep a matching hash.
pub(crate) fn open(out_dir: &Path, path: &str) -> Result<crate::io::AtomicFile> {
    let mut f = crate::io::AtomicFile::create(&out_dir.join(path))?;
    f.hash_prefix(path.as_bytes());
    f.hash_prefix(b"\0");
    Ok(f)
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

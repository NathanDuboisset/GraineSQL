//! Database connections and text-oriented row access.
//!
//! Every read goes through a session pinned by [`Dialect::session_setup`] and
//! selects columns already cast to text, so a row is always
//! `Vec<Option<String>>` regardless of engine. That keeps a single decode path
//! in [`crate::value`] and removes any dependence on driver type mapping.

pub mod mysql;
pub mod postgres;

use anyhow::{Context, Result};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, Row};

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::config::Engine;
use crate::dialect::{self, Dialect};
use crate::schema::{Schema, TableId, TypeClass};
use crate::source::ResolvedSource;

/// One row, every column already rendered as text. `None` is SQL NULL.
pub type TextRow = Vec<Option<String>>;

/// Enum type name a class refers to, looking through arrays.
fn enum_name(class: &TypeClass) -> Option<String> {
    match class {
        TypeClass::Enum { name } => Some(name.clone()),
        TypeClass::Array { of } => enum_name(of),
        _ => None,
    }
}

/// A configured table that the database does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub wanted: Vec<TableId>,
    /// Every table the database does have, for the error message.
    pub found: Vec<TableId>,
}

impl Missing {
    /// Error text for the case where the caller cannot explain the absence.
    pub fn describe(&self) -> String {
        let names: Vec<String> = self.wanted.iter().map(|t| t.to_string()).collect();
        let mut found: Vec<String> = self.found.iter().map(|t| t.to_string()).collect();
        found.sort();
        format!(
            "table{} {} listed in the config {} not exist in the database.\n\
             Tables found: {}",
            if names.len() == 1 { "" } else { "s" },
            names.join(", "),
            if names.len() == 1 { "does" } else { "do" },
            if found.is_empty() {
                "(none)".to_string()
            } else {
                found.join(", ")
            }
        )
    }
}

/// Narrow a schema to `wanted` plus every table reachable from it by foreign
/// key, so the lock stays scoped to what seedle actually touches while still
/// describing the tables that constrain load order.
///
/// Tables the database does not have are reported rather than raised: when a
/// lock knows the table, its absence is *drift* — a dropped table — and belongs
/// in the drift report with everything else. Only a caller with no lock to
/// compare against treats it as a plain error.
pub fn prune(schema: &Schema, wanted: &[TableId]) -> Result<(Schema, Option<Missing>)> {
    if wanted.is_empty() {
        return Ok((schema.clone(), None));
    }

    let mut keep: BTreeSet<TableId> = BTreeSet::new();
    let mut queue: Vec<TableId> = Vec::new();
    let mut missing: Vec<TableId> = Vec::new();

    for id in wanted {
        match schema.resolve(id) {
            Some(resolved) => queue.push(resolved),
            None => missing.push(id.clone()),
        }
    }

    while let Some(id) = queue.pop() {
        if !keep.insert(id.clone()) {
            continue;
        }
        if let Some(table) = schema.get(&id) {
            for fk in &table.foreign_keys {
                if let Some(parent) = schema.resolve(&fk.references) {
                    queue.push(parent);
                }
            }
        }
    }

    // Preserve the original (sorted) iteration order rather than discovery order.
    let mut tables = IndexMap::new();
    for (id, table) in &schema.tables {
        if keep.contains(id) {
            // Drop foreign keys pointing outside the kept set; they cannot
            // constrain a load that never touches the referenced table.
            let mut table = table.clone();
            table.foreign_keys.retain(|fk| {
                schema
                    .resolve(&fk.references)
                    .is_some_and(|p| keep.contains(&p))
            });
            tables.insert(id.clone(), table);
        }
    }

    let used: BTreeSet<String> = tables
        .values()
        .flat_map(|t| t.columns.iter())
        .filter_map(|c| enum_name(&c.class))
        .collect();
    let mut enums = schema.enums.clone();
    enums.retain(|name, _| used.contains(name));

    let missing = (!missing.is_empty()).then(|| Missing {
        wanted: missing,
        found: schema.tables.keys().cloned().collect(),
    });
    Ok((
        Schema {
            default_schema: schema.default_schema.clone(),
            tables,
            enums,
        },
        missing,
    ))
}

pub enum Pool {
    Pg(PgPool),
    My(MySqlPool),
}

pub struct Db {
    pool: Pool,
    dialect: Box<dyn Dialect>,
    pub source_name: String,
}

impl Db {
    pub async fn connect(src: &ResolvedSource) -> Result<Db> {
        Self::connect_with(src, 1).await
    }

    /// Connect with room for `max_conns` concurrent operations.
    pub async fn connect_with(src: &ResolvedSource, max_conns: u32) -> Result<Db> {
        let dialect = dialect::for_engine(src.engine);
        let setup: &'static [&'static str] = dialect.session_setup();
        let max_conns = max_conns.max(1);

        let pool = match src.engine {
            Engine::Postgres => Pool::Pg(
                PgPoolOptions::new()
                    .max_connections(max_conns)
                    .after_connect(move |conn, _| {
                        Box::pin(async move {
                            for stmt in setup {
                                conn.execute(*stmt).await?;
                            }
                            Ok(())
                        })
                    })
                    .connect(&src.url)
                    .await
                    .with_context(|| {
                        format!(
                            "connecting to source {:?} at {}",
                            src.name,
                            src.redacted_url()
                        )
                    })?,
            ),
            Engine::Mysql => Pool::My(
                MySqlPoolOptions::new()
                    .max_connections(max_conns)
                    .after_connect(move |conn, _| {
                        Box::pin(async move {
                            for stmt in setup {
                                conn.execute(*stmt).await?;
                            }
                            Ok(())
                        })
                    })
                    .connect(&src.url)
                    .await
                    .with_context(|| {
                        format!(
                            "connecting to source {:?} at {}",
                            src.name,
                            src.redacted_url()
                        )
                    })?,
            ),
        };

        Ok(Db {
            pool,
            dialect,
            source_name: src.name.clone(),
        })
    }

    pub fn dialect(&self) -> &dyn Dialect {
        self.dialect.as_ref()
    }

    pub fn engine(&self) -> Engine {
        self.dialect.engine()
    }

    /// Round-trip check, used by `seedle sources`.
    pub async fn ping(&self) -> Result<String> {
        let sql = match self.engine() {
            Engine::Postgres => "SELECT version()",
            Engine::Mysql => "SELECT version()",
        };
        let rows = self.query_text(sql).await?;
        Ok(rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_else(|| "(unknown version)".to_string()))
    }

    /// Run a query, returning every column as text.
    pub async fn query_text(&self, sql: &str) -> Result<Vec<TextRow>> {
        let mut out = Vec::new();
        self.for_each_text_row(sql, |row| {
            out.push(row);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Stream a query, invoking `f` per row. Returns the row count.
    ///
    /// Streaming rather than buffering keeps memory flat on wide tables; the
    /// callback shape avoids exposing an engine-specific stream type.
    pub async fn for_each_text_row<F>(&self, sql: &str, mut f: F) -> Result<u64>
    where
        F: FnMut(TextRow) -> Result<()>,
    {
        use futures_util::StreamExt;
        let mut n = 0u64;
        match &self.pool {
            Pool::Pg(p) => {
                let mut stream = sqlx::query(sql).fetch(p);
                while let Some(row) = stream.next().await {
                    let row = row.with_context(|| failed_sql(sql))?;
                    f(pg_row_to_text(&row)?)?;
                    n += 1;
                }
            }
            Pool::My(p) => {
                let mut stream = sqlx::query(sql).fetch(p);
                while let Some(row) = stream.next().await {
                    let row = row.with_context(|| failed_sql(sql))?;
                    f(my_row_to_text(&row)?)?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Execute a statement with no parameters, returning rows affected.
    pub async fn execute(&self, sql: &str) -> Result<u64> {
        match &self.pool {
            Pool::Pg(p) => Ok(sqlx::query(sql)
                .execute(p)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            Pool::My(p) => Ok(sqlx::query(sql)
                .execute(p)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
        }
    }

    /// Execute a statement with text bind parameters, returning rows affected.
    pub async fn execute_with(&self, sql: &str, binds: &[Option<String>]) -> Result<u64> {
        match &self.pool {
            Pool::Pg(p) => {
                let mut q = sqlx::query(sql);
                for b in binds {
                    q = q.bind(b.clone());
                }
                Ok(q.execute(p)
                    .await
                    .with_context(|| failed_sql(sql))?
                    .rows_affected())
            }
            Pool::My(p) => {
                let mut q = sqlx::query(sql);
                for b in binds {
                    q = q.bind(b.clone());
                }
                Ok(q.execute(p)
                    .await
                    .with_context(|| failed_sql(sql))?
                    .rows_affected())
            }
        }
    }

    /// Acquire one connection and pin it for the caller's exclusive use.
    ///
    /// A load must run every statement on the same connection, or `BEGIN` and
    /// the inserts would land on different sessions and the transaction would be
    /// meaningless.
    pub async fn pinned(&self) -> Result<PinnedConn<'_>> {
        Ok(match &self.pool {
            Pool::Pg(p) => PinnedConn::Pg(p.acquire().await.context("acquiring a connection")?),
            Pool::My(p) => PinnedConn::My(p.acquire().await.context("acquiring a connection")?),
        })
    }
}

/// A single connection held for the duration of a transaction.
pub enum PinnedConn<'a> {
    Pg(sqlx::pool::PoolConnection<sqlx::Postgres>),
    My(sqlx::pool::PoolConnection<sqlx::MySql>),
    #[allow(dead_code)]
    Phantom(std::marker::PhantomData<&'a ()>),
}

impl PinnedConn<'_> {
    pub async fn execute(&mut self, sql: &str) -> Result<u64> {
        match self {
            PinnedConn::Pg(c) => Ok(sqlx::query(sql)
                .execute(&mut **c)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            PinnedConn::My(c) => Ok(sqlx::query(sql)
                .execute(&mut **c)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            PinnedConn::Phantom(_) => unreachable!("phantom variant is never constructed"),
        }
    }

    pub async fn execute_with(&mut self, sql: &str, binds: &[Option<String>]) -> Result<u64> {
        match self {
            PinnedConn::Pg(c) => {
                let mut q = sqlx::query(sql);
                for b in binds {
                    q = q.bind(b.clone());
                }
                Ok(q.execute(&mut **c)
                    .await
                    .with_context(|| failed_sql(sql))?
                    .rows_affected())
            }
            PinnedConn::My(c) => {
                let mut q = sqlx::query(sql);
                for b in binds {
                    q = q.bind(b.clone());
                }
                Ok(q.execute(&mut **c)
                    .await
                    .with_context(|| failed_sql(sql))?
                    .rows_affected())
            }
            PinnedConn::Phantom(_) => unreachable!("phantom variant is never constructed"),
        }
    }

    pub async fn query_text(&mut self, sql: &str) -> Result<Vec<TextRow>> {
        match self {
            PinnedConn::Pg(c) => {
                let rows = sqlx::query(sql)
                    .fetch_all(&mut **c)
                    .await
                    .with_context(|| failed_sql(sql))?;
                rows.iter().map(pg_row_to_text).collect()
            }
            PinnedConn::My(c) => {
                let rows = sqlx::query(sql)
                    .fetch_all(&mut **c)
                    .await
                    .with_context(|| failed_sql(sql))?;
                rows.iter().map(my_row_to_text).collect()
            }
            PinnedConn::Phantom(_) => unreachable!("phantom variant is never constructed"),
        }
    }
}

/// Truncated SQL for an error message. Long generated statements would
/// otherwise bury the actual database error.
fn failed_sql(sql: &str) -> String {
    const MAX: usize = 400;
    if sql.len() <= MAX {
        format!("running: {sql}")
    } else {
        format!("running: {}… ({} bytes total)", &sql[..MAX], sql.len())
    }
}

fn pg_row_to_text(row: &sqlx::postgres::PgRow) -> Result<TextRow> {
    (0..row.len()).map(|i| text_at(row, i)).collect()
}

fn my_row_to_text(row: &sqlx::mysql::MySqlRow) -> Result<TextRow> {
    (0..row.len()).map(|i| text_at(row, i)).collect()
}

/// Read column `i` as text.
///
/// The query casts every column to a character type, but MySQL sometimes
/// reports such a result as binary, so fall back to decoding the raw bytes.
fn text_at<R>(row: &R, i: usize) -> Result<Option<String>>
where
    R: Row,
    for<'a> Option<String>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<Vec<u8>>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    usize: sqlx::ColumnIndex<R>,
{
    match row.try_get::<Option<String>, _>(i) {
        Ok(v) => Ok(v),
        Err(_) => {
            let raw: Option<Vec<u8>> = row
                .try_get(i)
                .with_context(|| format!("reading column {i} as text"))?;
            match raw {
                None => Ok(None),
                Some(bytes) => Ok(Some(String::from_utf8(bytes).with_context(|| {
                    format!("column {i} is not valid UTF-8 after the text cast")
                })?)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_reports_missing_tables_instead_of_raising() {
        // A dropped table has to reach drift classification, which explains it
        // far better than a bare "does not exist" from here.
        let schema = Schema {
            default_schema: "public".into(),
            tables: IndexMap::new(),
            enums: IndexMap::new(),
        };
        let (_pruned, missing) = prune(&schema, &[TableId::bare("gone")]).unwrap();
        let missing = missing.expect("the absent table should be reported");
        assert_eq!(missing.wanted, [TableId::bare("gone")]);
        assert!(missing.describe().contains("gone"));
        assert!(missing.describe().contains("(none)"));
    }
}

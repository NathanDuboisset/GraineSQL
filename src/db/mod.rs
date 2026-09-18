//! Database connections and text-oriented row access.
//!
//! Every read goes through a session pinned by [`Dialect::session_setup`] and
//! selects columns already cast to text, so a row is always
//! `Vec<Option<String>>` regardless of engine. That keeps a single decode path
//! in [`crate::value`] and removes any dependence on driver type mapping.

pub mod mongo;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

use anyhow::{Context, Result, bail};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::{Executor, Row};

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::config::Engine;
use crate::dialect::{self, Dialect};
use crate::schema::{Schema, TableId, TypeClass};
use crate::source::ResolvedSource;

/// One row, every column already rendered as text. `None` is SQL NULL.
pub type TextRow = Vec<Option<String>>;

/// Unreachable from user input: every command branches on [`Engine::is_sql`]
/// before taking a SQL path, so this names a bug rather than a mistake.
#[cfg(feature = "mongo")]
const NOT_SQL: &str = "internal: a SQL statement was built for a document engine";

/// What a build without the feature says when a config asks for Mongo.
#[cfg(not(feature = "mongo"))]
const NO_MONGO: &str =
    "this build has no MongoDB support; install with `cargo install grainesql --features mongo`";

/// Levenshtein distance, for "did you mean" suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    // Only the previous row is needed, so this stays O(min) in memory.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Positional accessor over an introspection row, with errors naming the query.
///
/// `PRAGMA` results are variable-width, so `new` takes a minimum rather than an
/// exact count.
pub struct Fields<'a> {
    row: &'a [Option<String>],
    what: &'static str,
}

impl<'a> Fields<'a> {
    pub fn new(row: &'a [Option<String>], min: usize, what: &'static str) -> Result<Self> {
        anyhow::ensure!(
            row.len() >= min,
            "{what} introspection returned {} columns, expected at least {min}",
            row.len()
        );
        Ok(Self { row, what })
    }

    pub fn text(&self, i: usize) -> Result<&str> {
        self.row
            .get(i)
            .and_then(|v| v.as_deref())
            .ok_or_else(|| anyhow::anyhow!("{} introspection: column {i} was NULL", self.what))
    }

    pub fn opt(&self, i: usize) -> Option<&str> {
        self.row.get(i).and_then(|v| v.as_deref())
    }

    /// A boolean in whichever spelling the engine uses.
    pub fn bool(&self, i: usize) -> Result<bool> {
        Ok(matches!(self.text(i)?, "t" | "true" | "1"))
    }
}

/// Introspect the live schema. The single dispatch point per engine.
pub async fn introspect(db: &Db) -> Result<Schema> {
    let mut schema = match db.engine().dialect() {
        Engine::Mysql => mysql::introspect(db).await,
        Engine::Sqlite => sqlite::introspect(db).await,
        Engine::Postgres | Engine::Supabase => postgres::introspect(db).await,
        #[cfg(feature = "mongo")]
        Engine::Mongo => mongo::introspect(db).await,
        #[cfg(not(feature = "mongo"))]
        Engine::Mongo => Err(anyhow::anyhow!(NO_MONGO)),
    }?;

    // A primary key column is NOT NULL whatever the catalog says. SQLite only
    // enforces that for an INTEGER PRIMARY KEY, so without this a lock taken
    // there claims every other key column is nullable and reads as breaking
    // drift against any engine that does enforce it.
    for table in schema.tables.values_mut() {
        let pk = table.primary_key.clone();
        for col in table.columns.iter_mut().filter(|c| pk.contains(&c.name)) {
            col.nullable = false;
        }
    }
    Ok(schema)
}

/// Every user table, for `graine init` and `graine add`.
pub async fn list_tables(db: &Db) -> Result<Vec<TableId>> {
    match db.engine().dialect() {
        Engine::Mysql => mysql::list_tables(db).await,
        Engine::Sqlite => sqlite::list_tables(db).await,
        Engine::Postgres | Engine::Supabase => postgres::list_tables(db).await,
        #[cfg(feature = "mongo")]
        Engine::Mongo => mongo::list_collections(db).await,
        #[cfg(not(feature = "mongo"))]
        Engine::Mongo => Err(anyhow::anyhow!(NO_MONGO)),
    }
}

/// The schema new objects land in when unqualified.
pub async fn default_schema(db: &Db) -> Result<String> {
    let sql = match db.engine().dialect() {
        Engine::Mysql => "SELECT DATABASE()",
        Engine::Sqlite => return Ok(sqlite::SCHEMA.to_string()),
        #[cfg(feature = "mongo")]
        Engine::Mongo => return mongo::database_name(db),
        #[cfg(not(feature = "mongo"))]
        Engine::Mongo => bail!(NO_MONGO),
        Engine::Postgres | Engine::Supabase => "SELECT current_schema()",
    };
    Ok(db
        .query_text(sql)
        .await?
        .first()
        .and_then(|r| r.first().cloned().flatten())
        .unwrap_or_default())
}

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
    ///
    /// Names a likely intended table per missing entry rather than dumping the
    /// whole catalogue: a real database has hundreds of tables, and a wall of
    /// them buries the one line that matters.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let names: Vec<String> = self.wanted.iter().map(|t| t.to_string()).collect();
        out.push_str(&format!(
            "table{} {} listed in the config {} not exist in the database.\n",
            if names.len() == 1 { "" } else { "s" },
            names.join(", "),
            if names.len() == 1 { "does" } else { "do" }
        ));

        for missing in &self.wanted {
            match self.closest(&missing.name) {
                Some(hit) => out.push_str(&format!("  {missing} , did you mean {hit}?\n")),
                None => out.push_str(&format!("  {missing} , no similar table name\n")),
            }
        }
        out.push_str(&format!(
            "The database has {} table{} in total; run `graine init --url ...` to list them.",
            self.found.len(),
            if self.found.len() == 1 { "" } else { "s" }
        ));
        out
    }

    /// The existing table whose name is closest to `name`, if any is close.
    fn closest(&self, name: &str) -> Option<&TableId> {
        self.found
            .iter()
            .map(|t| (edit_distance(name, &t.name), t))
            // A suggestion is only helpful if it is actually similar; a third of
            // the name being different is where it stops being a likely typo.
            .filter(|(d, t)| *d * 3 <= t.name.len().max(name.len()))
            .min_by_key(|(d, t)| (*d, t.to_string()))
            .map(|(_, t)| t)
    }
}

/// Narrow a schema to `wanted` plus every table reachable from it by foreign
/// key, so the lock stays scoped to what GraineSQL actually touches while still
/// describing the tables that constrain load order.
///
/// Tables the database does not have are reported rather than raised: when a
/// lock knows the table, its absence is *drift*, a dropped table, and belongs
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
    Lite(SqlitePool),
    /// The client plus the database named in the URL, which Mongo needs
    /// explicitly where a SQL connection carries it.
    #[cfg(feature = "mongo")]
    Mongo(mongodb::Client, String),
}

pub struct Db {
    pub(crate) pool: Pool,
    /// `None` for an engine that does not speak SQL.
    dialect: Option<Box<dyn Dialect>>,
    engine: Engine,
    pub source_name: String,
}

impl Db {
    pub async fn connect(src: &ResolvedSource) -> Result<Db> {
        Self::connect_with(src, 1).await
    }

    /// Connect with room for `max_conns` concurrent operations.
    pub async fn connect_with(src: &ResolvedSource, max_conns: u32) -> Result<Db> {
        let dialect = dialect::for_engine(src.engine);
        let setup: &'static [&'static str] =
            dialect.as_ref().map(|d| d.session_setup()).unwrap_or(&[]);
        let max_conns = max_conns.max(1);

        let pool = match src.engine.dialect() {
            Engine::Sqlite => Pool::Lite(
                SqlitePoolOptions::new()
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
                        format!("opening source {:?} at {}", src.name, src.redacted_url())
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
            #[cfg(feature = "mongo")]
            Engine::Mongo => {
                let client = mongodb::Client::with_uri_str(&src.url)
                    .await
                    .with_context(|| {
                        format!(
                            "connecting to source {:?} at {}",
                            src.name,
                            src.redacted_url()
                        )
                    })?;
                let database = mongodb::options::ClientOptions::parse(&src.url)
                    .await
                    .ok()
                    .and_then(|o| o.default_database)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "source {:?}: the MongoDB URL names no database. Append one, \
                             as in mongodb://host:27017/myapp",
                            src.name
                        )
                    })?;
                Pool::Mongo(client, database)
            }
            #[cfg(not(feature = "mongo"))]
            Engine::Mongo => bail!(
                "source {:?} uses `engine: mongo`, which this build does not have. \
                 Install with `--features mongo`.",
                src.name
            ),
            Engine::Postgres | Engine::Supabase => Pool::Pg(
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
        };

        Ok(Db {
            pool,
            dialect,
            engine: src.engine,
            source_name: src.name.clone(),
        })
    }

    /// The SQL dialect. Only reachable for a SQL engine: every command branches
    /// on [`Engine::is_sql`] first, so this cannot be hit from user input.
    pub fn dialect(&self) -> &dyn Dialect {
        self.dialect
            .as_deref()
            .expect("internal: the SQL path was reached for a document engine")
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// Round-trip check, used by `graine sources`.
    pub async fn ping(&self) -> Result<String> {
        #[cfg(feature = "mongo")]
        if let Pool::Mongo(..) = &self.pool {
            let n = mongo::ops::list_collections(self).await?.len();
            return Ok(format!(
                "mongodb, {n} collection{}",
                if n == 1 { "" } else { "s" }
            ));
        }
        let rows = self.query_text(self.dialect().version_query()).await?;
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
            #[cfg(feature = "mongo")]
            Pool::Mongo(..) => bail!(NOT_SQL),
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
            Pool::Lite(p) => {
                let mut stream = sqlx::query(sql).fetch(p);
                while let Some(row) = stream.next().await {
                    let row = row.with_context(|| failed_sql(sql))?;
                    f(lite_row_to_text(&row)?)?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Execute a statement with no parameters, returning rows affected.
    pub async fn execute(&self, sql: &str) -> Result<u64> {
        match &self.pool {
            #[cfg(feature = "mongo")]
            Pool::Mongo(..) => bail!(NOT_SQL),
            Pool::Pg(p) => Ok(sqlx::raw_sql(sql)
                .execute(p)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            Pool::My(p) => Ok(sqlx::raw_sql(sql)
                .execute(p)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            Pool::Lite(p) => Ok(sqlx::raw_sql(sql)
                .execute(p)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
        }
    }

    /// Execute a statement with text bind parameters, returning rows affected.
    pub async fn execute_with(&self, sql: &str, binds: &[Option<String>]) -> Result<u64> {
        match &self.pool {
            #[cfg(feature = "mongo")]
            Pool::Mongo(..) => bail!(NOT_SQL),
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
            Pool::Lite(p) => {
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
            #[cfg(feature = "mongo")]
            Pool::Mongo(..) => bail!(NOT_SQL),
            Pool::Pg(p) => PinnedConn::Pg(p.acquire().await.context("acquiring a connection")?),
            Pool::My(p) => PinnedConn::My(p.acquire().await.context("acquiring a connection")?),
            Pool::Lite(p) => PinnedConn::Lite(p.acquire().await.context("acquiring a connection")?),
        })
    }
}

/// A single connection held for the duration of a transaction.
pub enum PinnedConn<'a> {
    Pg(sqlx::pool::PoolConnection<sqlx::Postgres>),
    My(sqlx::pool::PoolConnection<sqlx::MySql>),
    Lite(sqlx::pool::PoolConnection<sqlx::Sqlite>),
    #[allow(dead_code)]
    Phantom(std::marker::PhantomData<&'a ()>),
}

impl PinnedConn<'_> {
    pub async fn execute(&mut self, sql: &str) -> Result<u64> {
        match self {
            PinnedConn::Pg(c) => Ok(sqlx::raw_sql(sql)
                .execute(&mut **c)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            PinnedConn::My(c) => Ok(sqlx::raw_sql(sql)
                .execute(&mut **c)
                .await
                .with_context(|| failed_sql(sql))?
                .rows_affected()),
            PinnedConn::Lite(c) => Ok(sqlx::raw_sql(sql)
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
            PinnedConn::Lite(c) => {
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

    /// Stream a CSV payload into `COPY ... FROM STDIN`. Postgres only.
    ///
    /// `chunks` is pulled lazily so the whole payload is never in memory at
    /// once.
    pub async fn copy_in<I>(&mut self, sql: &str, chunks: I) -> Result<u64>
    where
        I: IntoIterator<Item = Result<Vec<u8>>>,
    {
        match self {
            PinnedConn::Pg(c) => {
                let mut sink = c.copy_in_raw(sql).await.with_context(|| failed_sql(sql))?;
                for chunk in chunks {
                    sink.send(chunk?.as_slice())
                        .await
                        .context("sending the copy stream")?;
                }
                sink.finish().await.with_context(|| failed_sql(sql))
            }
            _ => bail!("this engine has no COPY FROM STDIN"),
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
            PinnedConn::Lite(c) => {
                let rows = sqlx::query(sql)
                    .fetch_all(&mut **c)
                    .await
                    .with_context(|| failed_sql(sql))?;
                rows.iter().map(lite_row_to_text).collect()
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
        format!("running: {}... ({} bytes total)", &sql[..MAX], sql.len())
    }
}

fn pg_row_to_text(row: &sqlx::postgres::PgRow) -> Result<TextRow> {
    (0..row.len()).map(|i| text_at(row, i)).collect()
}

fn my_row_to_text(row: &sqlx::mysql::MySqlRow) -> Result<TextRow> {
    (0..row.len()).map(|i| text_at(row, i)).collect()
}

/// SQLite stores whatever was written, regardless of the declared type, so a
/// `PRAGMA` result or a loosely-typed column can arrive as any of the storage
/// classes. Each is tried in turn rather than assuming one.
fn lite_row_to_text(row: &sqlx::sqlite::SqliteRow) -> Result<TextRow> {
    (0..row.len())
        .map(|i| {
            if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                return Ok(v);
            }
            if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                return Ok(v.map(|n| n.to_string()));
            }
            if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
                return Ok(v.map(crate::value::format_float));
            }
            let raw: Option<Vec<u8>> = row
                .try_get(i)
                .with_context(|| format!("reading column {i}"))?;
            match raw {
                None => Ok(None),
                Some(b) => {
                    Ok(Some(String::from_utf8(b).with_context(|| {
                        format!("column {i} is not valid UTF-8")
                    })?))
                }
            }
        })
        .collect()
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
        // A dropped table has to reach drift classification, which explains
        // it; a bare "does not exist" from here would not.
        let schema = Schema {
            default_schema: "public".into(),
            tables: IndexMap::new(),
            enums: IndexMap::new(),
        };
        let (_pruned, missing) = prune(&schema, &[TableId::bare("gone")]).unwrap();
        let missing = missing.expect("the absent table should be reported");
        assert_eq!(missing.wanted, [TableId::bare("gone")]);
        assert!(missing.describe().contains("gone"));
    }

    #[test]
    fn a_missing_table_error_suggests_a_near_match_not_the_whole_catalogue() {
        let mut tables = IndexMap::new();
        for name in ["c_agents_v2", "g_companies", "g_projects", "r_files"] {
            let id = TableId::new("public", name);
            tables.insert(
                id.clone(),
                crate::schema::Table {
                    id,
                    columns: vec![],
                    primary_key: vec![],
                    unique: vec![],
                    foreign_keys: vec![],
                },
            );
        }
        let schema = Schema {
            default_schema: "public".into(),
            tables,
            enums: IndexMap::new(),
        };

        let (_p, missing) = prune(&schema, &[TableId::bare("c_agents")]).unwrap();
        let text = missing.expect("missing").describe();
        assert!(text.contains("did you mean public.c_agents_v2?"), "{text}");
        // The full table list must not be dumped; a real database has hundreds.
        assert!(!text.contains("g_companies"), "{text}");
        assert!(text.contains("4 tables in total"), "{text}");
    }

    #[test]
    fn an_unrelated_name_gets_no_bogus_suggestion() {
        let mut tables = IndexMap::new();
        let id = TableId::new("public", "g_companies");
        tables.insert(
            id.clone(),
            crate::schema::Table {
                id,
                columns: vec![],
                primary_key: vec![],
                unique: vec![],
                foreign_keys: vec![],
            },
        );
        let schema = Schema {
            default_schema: "public".into(),
            tables,
            enums: IndexMap::new(),
        };
        let (_p, missing) = prune(&schema, &[TableId::bare("zzzzzz")]).unwrap();
        let text = missing.expect("missing").describe();
        assert!(text.contains("no similar table name"), "{text}");
    }

    #[test]
    fn edit_distance_is_symmetric_and_correct() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("a", ""), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("c_agents", "c_agents_v2"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "cba"), edit_distance("cba", "abc"));
    }
}

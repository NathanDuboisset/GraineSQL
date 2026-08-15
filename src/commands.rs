//! Command implementations — the layer that wires config, database, lock, and
//! formats together.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::cli::{Cli, Command};
use crate::config::{Config, Engine, ResolvedTable};
use crate::db::{self, Db};
use crate::export;
use crate::format;
use crate::load;
use crate::lock::{BucketEntry, FileEntry, Lock, drift};
use crate::order;
use crate::schema::{Schema, TableId};
use crate::source::{self, ResolvedSource};
use crate::storage::{self, BucketExport, StorageClient};

/// Shared setup: config, resolved source, and reporting settings.
pub struct Ctx {
    pub cfg: Config,
    pub cli_source: Option<String>,
    pub json: bool,
    pub verbose: bool,
    pub quiet: bool,
}

impl Ctx {
    pub fn new(cli: &Cli) -> Result<Ctx> {
        let path = cli.config_path()?;
        let cfg = Config::load(&path)?;
        Ok(Ctx {
            cfg,
            cli_source: cli.source.clone(),
            json: cli.json,
            verbose: cli.verbose,
            quiet: cli.quiet,
        })
    }

    pub fn source_name(&self) -> Result<String> {
        match &self.cli_source {
            Some(s) => Ok(s.clone()),
            None => Ok(self.cfg.default_source()?.to_string()),
        }
    }

    pub fn resolve_source(&self) -> Result<ResolvedSource> {
        source::resolve(&self.cfg, &self.source_name()?)
    }

    pub async fn connect(&self, max_conns: u32) -> Result<(ResolvedSource, Db)> {
        let src = self.resolve_source()?;
        let db = Db::connect_with(&src, max_conns).await?;
        Ok((src, db))
    }

    /// Print unless `--quiet`.
    pub fn say(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            println!("{}", msg.as_ref());
        }
    }

    /// Print only under `--verbose`.
    pub fn detail(&self, msg: impl AsRef<str>) {
        if self.verbose && !self.quiet {
            println!("{}", msg.as_ref());
        }
    }

    /// Warnings go to stderr so they never contaminate piped output.
    pub fn warn(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("{}", msg.as_ref());
        }
    }
}

/// Introspect the live schema, pruned to the configured tables and their
/// foreign-key closure.
pub async fn introspect(db: &Db, cfg: &Config) -> Result<Schema> {
    let full = match db.engine() {
        Engine::Postgres => db::postgres::introspect(db).await?,
        Engine::Mysql => db::mysql::introspect(db).await?,
    };
    let wanted: Vec<TableId> = cfg.resolved_tables()?.into_iter().map(|t| t.id).collect();
    let (mut live, missing) = db::prune(&full, &wanted)?;

    let lock = Lock::read(&cfg.lock_path()).ok();

    if let Some(missing) = missing {
        // A table the lock knows about but the database no longer has is a
        // dropped table, which the drift report explains far better than a bare
        // "does not exist". Anything the lock has never heard of is a typo in
        // the config, and there is nothing to compare it against.
        let unknown: Vec<TableId> = match &lock {
            Some(lock) => missing
                .wanted
                .iter()
                .filter(|id| lock.to_schema().resolve(id).is_none())
                .cloned()
                .collect(),
            None => missing.wanted.clone(),
        };
        if !unknown.is_empty() {
            bail!(
                "{}",
                db::Missing {
                    wanted: unknown,
                    found: missing.found
                }
                .describe()
            );
        }
    }

    // The lock decides column order wherever one exists, so the physical layout
    // of this particular database cannot leak into the exported files.
    if let Some(lock) = &lock {
        lock.align_column_order(&mut live);
    }
    Ok(live)
}

/// The load order for the configured tables.
pub fn compute_order(schema: &Schema, cfg: &Config) -> Result<Vec<TableId>> {
    let wanted: Vec<TableId> = cfg.resolved_tables()?.into_iter().map(|t| t.id).collect();
    let ordered = order::topological(schema, &wanted);
    // Only the configured tables belong in the manifest; parents pulled in for
    // ordering are not exported.
    let configured: Vec<TableId> = ordered
        .tables
        .into_iter()
        .filter(|id| {
            wanted
                .iter()
                .any(|w| schema.resolve(w).is_some_and(|r| r == *id) || w == id)
        })
        .collect();
    Ok(configured)
}

/// Check the live schema against the lock, applying the drift policy.
///
/// Breaking drift aborts; benign drift warns and continues. `force` downgrades
/// breaking to a loud warning.
pub fn gate_on_drift(ctx: &Ctx, lock: &Lock, live: &Schema, force: bool) -> Result<drift::Report> {
    let report = drift::classify(&lock.to_schema(), live);
    if report.is_empty() {
        return Ok(report);
    }

    if report.has_breaking() {
        if !force {
            bail!(
                "schema drift vs seedle.lock\n{}\nNothing was changed. Review the differences, \
                 then run `seedle lock` to accept them, or `--force` to proceed anyway.",
                report.render()
            );
        }
        ctx.warn(format!(
            "warning: proceeding past breaking schema drift because --force was given\n{}",
            report.render()
        ));
    } else {
        ctx.warn(format!("note: benign schema drift\n{}", report.render()));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// sources
// ---------------------------------------------------------------------------

pub async fn cmd_sources(ctx: &Ctx, no_connect: bool) -> Result<()> {
    let default = ctx.cfg.default_source().ok();
    let mut rows = Vec::new();

    for (name, src) in &ctx.cfg.sources {
        let is_default = default == Some(name.as_str());
        let mut entry = serde_json::json!({
            "name": name,
            "engine": src.engine.as_str(),
            "default": is_default,
            "read_only": src.read_only,
        });

        match source::resolve(&ctx.cfg, name) {
            Err(e) => {
                entry["status"] = serde_json::json!("unresolved");
                entry["error"] = serde_json::json!(format!("{e:#}"));
            }
            Ok(resolved) => {
                entry["url"] = serde_json::json!(resolved.redacted_url());
                entry["origin"] = serde_json::json!(resolved.origin);
                entry["local"] = serde_json::json!(resolved.is_local());
                if no_connect {
                    entry["status"] = serde_json::json!("resolved");
                } else {
                    match Db::connect(&resolved).await {
                        Ok(db) => match db.ping().await {
                            Ok(version) => {
                                entry["status"] = serde_json::json!("ok");
                                entry["version"] = serde_json::json!(version);
                            }
                            Err(e) => {
                                entry["status"] = serde_json::json!("error");
                                entry["error"] = serde_json::json!(format!("{e:#}"));
                            }
                        },
                        Err(e) => {
                            entry["status"] = serde_json::json!("error");
                            entry["error"] = serde_json::json!(format!("{e:#}"));
                        }
                    }
                    // Storage is reported separately: a source can have a
                    // reachable database and an unreachable storage service.
                    if resolved.storage.is_some() {
                        match storage_client(&resolved) {
                            Err(e) => {
                                entry["storage"] = serde_json::json!("unresolved");
                                entry["storage_error"] = serde_json::json!(format!("{e:#}"));
                            }
                            Ok(client) => match client.ping().await {
                                Ok(n) => {
                                    entry["storage"] = serde_json::json!("ok");
                                    entry["storage_buckets"] = serde_json::json!(n);
                                }
                                Err(e) => {
                                    entry["storage"] = serde_json::json!("error");
                                    entry["storage_error"] = serde_json::json!(format!("{e:#}"));
                                }
                            },
                        }
                    }
                }
            }
        }
        rows.push(entry);
    }

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let width = ctx.cfg.sources.keys().map(|k| k.len()).max().unwrap_or(0);
        for r in &rows {
            let name = r["name"].as_str().unwrap_or("");
            let marker = if r["default"] == serde_json::json!(true) {
                "*"
            } else {
                " "
            };
            let status = r["status"].as_str().unwrap_or("?");
            let mut line = format!(
                "{marker} {:width$}  {:9}  {status}",
                name,
                r["engine"].as_str().unwrap_or(""),
                width = width
            );
            if let Some(url) = r["url"].as_str() {
                line.push_str(&format!("  {url}"));
            }
            if r["read_only"] == serde_json::json!(true) {
                line.push_str("  [read-only]");
            }
            if let Some(st) = r["storage"].as_str() {
                line.push_str(&match r["storage_buckets"].as_u64() {
                    Some(n) => format!("  storage:{st} ({n} bucket(s))"),
                    None => format!("  storage:{st}"),
                });
            }
            println!("{line}");
            for key in ["error", "storage_error"] {
                if let Some(err) = r[key].as_str() {
                    println!("  {:width$}  {err}", "", width = width);
                }
            }
            if let Some(origin) = r["origin"].as_str() {
                ctx.detail(format!(
                    "  {:width$}  credentials from {origin}",
                    "",
                    width = width
                ));
            }
        }
    }

    let failed = rows.iter().any(|r| {
        r["status"] == serde_json::json!("error") || r["status"] == serde_json::json!("unresolved")
    });
    if failed {
        bail!("one or more sources could not be reached");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lock / diff
// ---------------------------------------------------------------------------

pub async fn cmd_lock(ctx: &Ctx, check: bool) -> Result<()> {
    let (_src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let order = compute_order(&live, &ctx.cfg)?;
    let lock_path = ctx.cfg.lock_path();

    if check {
        let existing = Lock::read(&lock_path)?;
        let report = drift::classify(&existing.to_schema(), &live);
        if ctx.json {
            println!("{}", drift_json(&report));
        } else {
            print!("{}", report.render());
        }
        if !report.is_empty() {
            bail!(
                "seedle.lock is out of date with source {:?}; run `seedle lock` to update it",
                db.source_name
            );
        }
        return Ok(());
    }

    // Preserve the recorded file hashes: relocking is a schema operation and
    // should not silently claim the seed files were re-verified.
    let previous = Lock::read(&lock_path).ok();
    let mut lock = Lock::build(db.engine(), &live, &order);
    if let Some(prev) = &previous {
        lock.files = prev
            .files
            .iter()
            .filter(|(id, _)| lock.schema.contains_key(*id))
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
    }
    lock.write(&lock_path)?;

    match previous {
        None => ctx.say(format!(
            "wrote {} ({} tables, fingerprint {})",
            lock_path.display(),
            lock.schema.len(),
            lock.fingerprint
        )),
        Some(prev) if prev.fingerprint == lock.fingerprint => ctx.say(format!(
            "{} is already up to date ({})",
            lock_path.display(),
            lock.fingerprint
        )),
        Some(prev) => {
            let report = drift::classify(&prev.to_schema(), &live);
            ctx.say(format!(
                "updated {} ({} -> {})\n{}",
                lock_path.display(),
                prev.fingerprint,
                lock.fingerprint,
                report.render()
            ));
        }
    }
    Ok(())
}

pub async fn cmd_diff(ctx: &Ctx) -> Result<()> {
    let (_src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let lock = Lock::read(&ctx.cfg.lock_path())?;
    let report = drift::classify(&lock.to_schema(), &live);

    if ctx.json {
        println!("{}", drift_json(&report));
    } else {
        print!("{}", report.render());
    }

    if report.has_breaking() {
        std::process::exit(1);
    }
    Ok(())
}

fn drift_json(report: &drift::Report) -> String {
    let items: Vec<serde_json::Value> = report
        .drifts
        .iter()
        .map(|d| {
            serde_json::json!({
                "severity": d.severity.label(),
                "table": d.table.to_string(),
                "column": d.column,
                "change": d.what,
                "note": d.note,
            })
        })
        .collect();
    let (breaking, benign) = report.counts();
    serde_json::to_string_pretty(&serde_json::json!({
        "breaking": breaking,
        "benign": benign,
        "drifts": items,
    }))
    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn cmd_export(
    ctx: &Ctx,
    tables: Option<Vec<String>>,
    buckets: Option<Vec<String>>,
    no_buckets: bool,
    format_override: Option<crate::config::Format>,
    out_override: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let selected = ctx.cfg.select_tables(tables.as_deref())?;
    let buckets = if no_buckets {
        Some(Vec::new())
    } else {
        buckets
    };
    if selected.is_empty() && ctx.cfg.buckets.is_empty() {
        bail!("the config lists no tables and no buckets, so there is nothing to export");
    }

    let max_conns = export::concurrency(&ctx.cfg, selected.len()) as u32;
    let (_src, db) = ctx.connect(max_conns).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let order = compute_order(&live, &ctx.cfg)?;

    let out_dir = match &out_override {
        Some(o) => ctx.cfg.base_dir.join(o),
        None => ctx.cfg.out_dir(),
    };
    // The lock lives beside the data it describes: its `files` paths are
    // relative to the output directory, so splitting them would make the
    // recorded hashes point at the wrong files.
    let lock_path = out_dir.join(crate::config::LOCK_FILENAME);

    // Drift is always judged against the project's committed lock, not against
    // whatever happens to be in a scratch output directory.
    let existing = Lock::read(&ctx.cfg.lock_path()).ok();
    if let Some(lock) = &existing {
        gate_on_drift(ctx, lock, &live, force)?;
    } else if !force {
        ctx.warn(format!(
            "note: no lock file at {} yet; it will be created by this export",
            ctx.cfg.lock_path().display()
        ));
    }

    // Export in load order, so progress output reads the way the data depends.
    let mut ordered: Vec<ResolvedTable> = Vec::new();
    for id in &order {
        if let Some(t) = selected
            .iter()
            .find(|t| live.resolve(&t.id).as_ref() == Some(id) || t.id == *id)
        {
            let mut t = t.clone();
            if let Some(f) = format_override {
                t.format = f;
                t.layout = crate::config::Layout::Single;
                t.pretty = false;
            }
            ordered.push(t);
        }
    }

    let mut written: Vec<PathBuf> = Vec::new();
    let mut files: IndexMap<TableId, FileEntry> = IndexMap::new();
    let mut summary = Vec::new();
    let mut total_rows = 0u64;

    for cfg in &ordered {
        let exported = export::export_table(&db, &live, cfg, ctx.cfg.export.sql_batch).await?;
        ctx.detail(format!(
            "  {} -> {}",
            cfg.id,
            exported
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        written.extend(export::write_files(&out_dir, &exported)?);
        total_rows += exported.rows;

        files.insert(
            exported.table.clone(),
            FileEntry {
                path: export::lock_path(&exported, cfg, &live.default_schema),
                rows: exported.rows,
                sha256: export::content_hash(&exported),
            },
        );
        summary.push(serde_json::json!({
            "table": exported.table.to_string(),
            "rows": exported.rows,
            "files": exported.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        }));
    }

    // Buckets: the half of a project a SQL dump cannot capture.
    let (bucket_entries, bucket_summary) =
        export_buckets(ctx, &_src, &out_dir, buckets.as_deref()).await?;

    // A table removed from the config leaves an orphan file behind, which would
    // then load stale data forever. Remove ours, never anything else.
    let removed = crate::io::prune_stale(&out_dir, &written)?;
    for path in &removed {
        ctx.detail(format!("  removed stale {}", path.display()));
    }

    let mut lock = Lock::build(db.engine(), &live, &order);
    lock.files = files;
    lock.buckets = bucket_entries;
    lock.write(&lock_path)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "out": out_dir.display().to_string(),
                "tables": summary,
                "buckets": bucket_summary,
                "rows": total_rows,
                "removed": removed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "fingerprint": lock.fingerprint,
            }))?
        );
    } else {
        let objects: u64 = lock.buckets.values().map(|b| b.objects).sum();
        ctx.say(format!(
            "exported {} table{}, {total_rows} row{}{} to {}",
            ordered.len(),
            if ordered.len() == 1 { "" } else { "s" },
            if total_rows == 1 { "" } else { "s" },
            if lock.buckets.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} bucket{} ({objects} object{}, {})",
                    lock.buckets.len(),
                    if lock.buckets.len() == 1 { "" } else { "s" },
                    if objects == 1 { "" } else { "s" },
                    human_bytes(lock.buckets.values().map(|b| b.bytes).sum())
                )
            },
            out_dir.display()
        ));
        if !removed.is_empty() {
            ctx.say(format!("removed {} stale file(s)", removed.len()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// buckets
// ---------------------------------------------------------------------------

/// Storage client for a source, or a clear error explaining what is missing.
fn storage_client(src: &ResolvedSource) -> Result<StorageClient> {
    let access = src.storage.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "source {:?} has no `storage:` block, so its buckets cannot be reached.\n\
             Add `storage: {{url_var: SUPABASE_URL, key_var: SUPABASE_SERVICE_ROLE_KEY}}` to it.",
            src.name
        )
    })?;
    StorageClient::new(access)
}

/// Export every selected bucket into `out_dir/buckets/<name>/`.
async fn export_buckets(
    ctx: &Ctx,
    src: &ResolvedSource,
    out_dir: &Path,
    only: Option<&[String]>,
) -> Result<(IndexMap<String, BucketEntry>, Vec<serde_json::Value>)> {
    let selected = ctx.cfg.select_buckets(only)?;
    let mut entries = IndexMap::new();
    let mut summary = Vec::new();
    if selected.is_empty() {
        return Ok((entries, summary));
    }

    let client = storage_client(src)?;

    for (name, cfg) in &selected {
        let (export, blobs) = storage::export_bucket(&client, name, cfg)
            .await
            .with_context(|| format!("exporting bucket {name:?}"))?;

        let dir = out_dir.join("buckets").join(name);
        // Objects removed upstream must not linger locally, or a later load
        // would put them back. Rebuilding the directory is the simplest way to
        // guarantee the tree matches the manifest exactly.
        if dir.join("objects").exists() {
            std::fs::remove_dir_all(dir.join("objects"))
                .with_context(|| format!("clearing {}", dir.join("objects").display()))?;
        }
        for (key, bytes) in &blobs {
            let path = BucketExport::object_path(&dir, key)?;
            crate::io::write_atomic(&path, bytes)?;
        }
        crate::io::write_atomic(&dir.join("bucket.json"), &export.settings_bytes()?)?;
        crate::io::write_atomic(&dir.join("manifest.jsonl"), &export.manifest_bytes()?)?;

        let bytes = export.total_bytes();
        ctx.detail(format!(
            "  bucket {name}: {} object(s), {}",
            export.objects.len(),
            human_bytes(bytes)
        ));
        summary.push(serde_json::json!({
            "bucket": name,
            "objects": export.objects.len(),
            "bytes": bytes,
        }));
        entries.insert(
            name.clone(),
            BucketEntry {
                objects: export.objects.len() as u64,
                bytes,
                sha256: export.hash()?,
            },
        );
    }
    Ok((entries, summary))
}

/// Upload every selected bucket back into a project.
///
/// Deliberately not transactional: object storage has no transaction to join.
/// Uploads are upserts, so a re-run after a failure converges rather than
/// duplicating, and the database load has already committed by this point.
async fn load_buckets(
    ctx: &Ctx,
    src: &ResolvedSource,
    out_dir: &Path,
    only: Option<&[String]>,
    dry_run: bool,
) -> Result<Vec<serde_json::Value>> {
    let selected = ctx.cfg.select_buckets(only)?;
    let mut summary = Vec::new();
    if selected.is_empty() {
        return Ok(summary);
    }

    let client = storage_client(src)?;

    for (name, _cfg) in &selected {
        let dir = out_dir.join("buckets").join(name);
        if !dir.is_dir() {
            bail!(
                "bucket {name:?} has no exported files at {} (run `seedle export` first)",
                dir.display()
            );
        }
        let export = BucketExport::read(&dir)?;

        // Verify before uploading: a corrupted local file should not be pushed.
        let mut payloads = Vec::with_capacity(export.objects.len());
        for o in &export.objects {
            let path = BucketExport::object_path(&dir, &o.path)?;
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let actual = crate::lock::file_hash(&bytes);
            if actual != o.sha256 {
                bail!(
                    "bucket {name:?}: {} does not match its recorded hash.\n                       manifest: {}\n  on disk:  {actual}\n                     Re-export the bucket, or restore the file.",
                    path.display(),
                    o.sha256
                );
            }
            payloads.push((o.path.clone(), bytes, o.content_type.clone()));
        }

        if dry_run {
            ctx.say(format!(
                "would upload {} object(s) to bucket {name}",
                payloads.len()
            ));
            summary.push(serde_json::json!({
                "bucket": name, "objects": payloads.len(), "uploaded": 0, "dry_run": true,
            }));
            continue;
        }

        if client.ensure_bucket(&export.settings).await? {
            ctx.detail(format!("  created bucket {name}"));
        }
        for (key, bytes, content_type) in payloads {
            client
                .upload(name, &key, bytes, content_type.as_deref())
                .await?;
        }
        ctx.detail(format!(
            "  bucket {name}: {} object(s) uploaded",
            export.objects.len()
        ));
        summary.push(serde_json::json!({
            "bucket": name,
            "objects": export.objects.len(),
            "uploaded": export.objects.len(),
        }));
    }
    Ok(summary)
}

/// Byte count in the largest unit that keeps it readable.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// plan / load
// ---------------------------------------------------------------------------

/// Read every selected table's rows and build the load plan.
async fn build_plans(
    ctx: &Ctx,
    db: &Db,
    live: &Schema,
    tables: Option<Vec<String>>,
    force: bool,
) -> Result<(
    Vec<(ResolvedTable, Vec<Vec<crate::value::Value>>)>,
    Vec<load::TablePlan>,
)> {
    let selected = ctx.cfg.select_tables(tables.as_deref())?;
    let lock = Lock::read(&ctx.cfg.lock_path())?;
    gate_on_drift(ctx, &lock, live, force)?;

    let out_dir = ctx.cfg.out_dir();
    let order = compute_order(live, &ctx.cfg)?;

    let mut loads = Vec::new();
    let mut plans = Vec::new();

    for id in &order {
        let Some(cfg) = selected
            .iter()
            .find(|t| live.resolve(&t.id).as_ref() == Some(id) || t.id == *id)
        else {
            continue;
        };
        if !format::is_loadable(cfg) {
            bail!(
                "{}",
                format::read(&out_dir, &[], cfg, &live.default_schema).unwrap_err()
            );
        }

        let table = live
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("table {id} is not in the live schema"))?;
        let columns = export::selected_columns(table, cfg)?;
        let rows = format::read(&out_dir, &columns, cfg, &live.default_schema)?;
        load::validate_rows(table, &columns, &rows, cfg)?;

        plans.push(load::TablePlan {
            table: id.clone(),
            mode: cfg.load_mode,
            rows: rows.len() as u64,
            columns: columns.iter().map(|c| c.name.clone()).collect(),
            key: load::upsert_key(table, cfg)?,
        });
        loads.push((cfg.clone(), rows));
    }

    let _ = db;
    Ok((loads, plans))
}

pub async fn cmd_plan(ctx: &Ctx, tables: Option<Vec<String>>) -> Result<()> {
    let (_src, db) = ctx.connect(1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let (_loads, plans) = build_plans(ctx, &db, &live, tables, false).await?;

    if ctx.json {
        let items: Vec<serde_json::Value> = plans
            .iter()
            .map(|p| {
                serde_json::json!({
                    "table": p.table.to_string(),
                    "mode": p.mode.as_str(),
                    "rows": p.rows,
                    "columns": p.columns,
                    "key": p.key,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        print!("{}", load::render_plan(&db.source_name, &plans));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_load(
    ctx: &Ctx,
    tables: Option<Vec<String>>,
    buckets: Option<Vec<String>>,
    no_buckets: bool,
    dry_run: bool,
    no_transaction: bool,
    yes: bool,
    force: bool,
) -> Result<()> {
    let src = ctx.resolve_source()?;
    load::check_writable(&src)?;

    let db = Db::connect_with(&src, 1).await?;
    let live = introspect(&db, &ctx.cfg).await?;
    let (loads, plans) = build_plans(ctx, &db, &live, tables, force).await?;

    if plans.is_empty() {
        ctx.say("nothing to load");
        return Ok(());
    }

    let reasons = load::confirmation_reasons(&src, &plans);
    if !reasons.is_empty() && !yes && !dry_run {
        ctx.say(load::render_plan(&db.source_name, &plans));
        for r in &reasons {
            ctx.warn(format!("  ! {r}"));
        }
        if !confirm("Proceed?")? {
            ctx.say("aborted");
            return Ok(());
        }
    }

    let order = compute_order(&live, &ctx.cfg)?;
    let cycles = order::topological(&live, &order).cycles;
    let use_transaction = !no_transaction;

    let mut conn = db.pinned().await?;
    if use_transaction {
        conn.execute("BEGIN")
            .await
            .context("starting the transaction")?;
    }

    // A cycle among the loaded tables can only be satisfied if constraint
    // checking is deferred to commit time.
    let mut restored = false;
    if !cycles.is_empty() {
        let all_deferrable = cycles.iter().all(|c| c.all_deferrable);
        match load::defer_constraints(db.engine(), all_deferrable) {
            Some(stmt) => {
                conn.execute(stmt).await.with_context(|| {
                    format!("deferring constraints for a foreign-key cycle: {stmt}")
                })?;
                restored = true;
                ctx.detail(format!("  {stmt} (foreign-key cycle detected)"));
            }
            None => {
                let names: Vec<String> = cycles
                    .iter()
                    .map(|c| {
                        c.tables
                            .iter()
                            .map(|t| t.to_string())
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    })
                    .collect();
                if use_transaction {
                    let _ = conn.execute("ROLLBACK").await;
                }
                bail!(
                    "these tables form a foreign-key cycle: {}\n\
                     Postgres can only load a cycle when every constraint in it is DEFERRABLE. \
                     Mark them `DEFERRABLE INITIALLY IMMEDIATE`, or load the tables in separate \
                     runs.",
                    names.join("; ")
                );
            }
        }
    }

    // Emptying happens up front, children before parents, so a parent is never
    // deleted while its children still reference it.
    let truncating: Vec<TableId> = plans
        .iter()
        .filter(|p| p.mode == crate::config::LoadMode::TruncateFirst)
        .map(|p| p.table.clone())
        .collect();
    let mut deleted_counts = std::collections::BTreeMap::new();
    if !truncating.is_empty() {
        match load::delete_pass(&mut conn, db.dialect(), &order, &truncating).await {
            Ok(counts) => deleted_counts = counts,
            Err(e) => {
                if use_transaction {
                    let _ = conn.execute("ROLLBACK").await;
                    return Err(
                        e.context("load failed and was rolled back; the database is unchanged")
                    );
                }
                return Err(e);
            }
        }
    }

    let mut results = Vec::new();
    let mut failure = None;
    for (cfg, rows) in &loads {
        let id = live.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
        let Some(table) = live.get(&id) else { continue };
        let columns = export::selected_columns(table, cfg)?;
        match load::load_table(&mut conn, db.dialect(), table, &columns, rows, cfg).await {
            Ok(mut r) => {
                r.deleted = deleted_counts.get(&r.table).copied().unwrap_or(0);
                ctx.detail(format!(
                    "  {} {} rows ({})",
                    r.table,
                    r.rows,
                    r.mode.as_str()
                ));
                results.push(r);
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    if let Some(e) = failure {
        if use_transaction {
            let _ = conn.execute("ROLLBACK").await;
            return Err(e.context("load failed and was rolled back; the database is unchanged"));
        }
        return Err(e.context(
            "load failed partway through with --no-transaction; the database is partly written",
        ));
    }

    if restored && let Some(stmt) = load::restore_constraints(db.engine()) {
        conn.execute(stmt).await?;
    }

    // Sequence fixup belongs inside the transaction wherever the engine allows
    // it, so a rollback undoes it too.
    let deferred_fixups = load::fixup_after_commit(db.engine());
    if ctx.cfg.load.fix_sequences && !deferred_fixups {
        for (cfg, _) in &loads {
            let id = live.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
            let Some(table) = live.get(&id) else { continue };
            for stmt in load::sequence_fixups(db.engine(), table) {
                conn.execute(&stmt)
                    .await
                    .with_context(|| format!("advancing the sequence for {}", table.id))?;
            }
        }
    }

    if use_transaction {
        if dry_run {
            conn.execute("ROLLBACK")
                .await
                .context("rolling back the dry run")?;
        } else {
            conn.execute("COMMIT")
                .await
                .context("committing the load")?;
        }
    } else if dry_run {
        ctx.warn("warning: --dry-run with --no-transaction cannot undo anything; changes are live");
    }

    // Buckets go after the commit: object storage has no transaction to join, so
    // uploading earlier could leave files behind for rows that were then rolled
    // back. Uploads are upserts, so re-running after a failure converges.
    let bucket_summary = if no_buckets {
        Vec::new()
    } else {
        load_buckets(ctx, &src, &ctx.cfg.out_dir(), buckets.as_deref(), dry_run).await?
    };

    if ctx.json {
        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "table": r.table.to_string(),
                    "mode": r.mode.as_str(),
                    "rows": r.rows,
                    "affected": r.affected,
                    "deleted": r.deleted,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": dry_run,
                "tables": items,
                "buckets": bucket_summary,
            }))?
        );
    } else {
        let total: u64 = results.iter().map(|r| r.rows).sum();
        let deleted: u64 = results.iter().map(|r| r.deleted).sum();
        ctx.say(format!(
            "{} {} table{}, {total} row{}{} into source {:?}",
            if dry_run { "would load" } else { "loaded" },
            results.len(),
            if results.len() == 1 { "" } else { "s" },
            if total == 1 { "" } else { "s" },
            if deleted > 0 {
                format!(" (after deleting {deleted})")
            } else {
                String::new()
            },
            db.source_name
        ));
        if !bucket_summary.is_empty() {
            let objects: u64 = bucket_summary
                .iter()
                .map(|b| b["objects"].as_u64().unwrap_or(0))
                .sum();
            ctx.say(format!(
                "{} {objects} object{} across {} bucket{}",
                if dry_run { "would upload" } else { "uploaded" },
                if objects == 1 { "" } else { "s" },
                bucket_summary.len(),
                if bucket_summary.len() == 1 { "" } else { "s" }
            ));
        }
        if dry_run {
            ctx.say("dry run: rolled back, nothing was committed");
        }
    }

    // MySQL's AUTO_INCREMENT reset needs a read followed by an ALTER TABLE,
    // which implicitly commits — so it can only run once the load itself has
    // committed, and is skipped entirely for a dry run.
    if deferred_fixups && ctx.cfg.load.fix_sequences && !dry_run {
        drop(conn);
        for (cfg, _) in &loads {
            let id = live.resolve(&cfg.id).unwrap_or_else(|| cfg.id.clone());
            let Some(table) = live.get(&id) else { continue };
            for (table_id, column) in load::mysql_fixups(table) {
                load::fix_mysql_auto_increment(&db, &table_id, &column).await?;
                ctx.detail(format!("  reset AUTO_INCREMENT on {table_id}.{column}"));
            }
        }
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("{prompt} refusing to continue without a terminal to confirm on; pass --yes");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

pub fn cmd_verify(ctx: &Ctx) -> Result<()> {
    let lock = Lock::read(&ctx.cfg.lock_path())?;
    let out_dir = ctx.cfg.out_dir();
    let schema = lock.to_schema();
    let selected = ctx.cfg.resolved_tables()?;

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0;

    for cfg in &selected {
        let Some(id) = schema.resolve(&cfg.id) else {
            problems.push(format!("{}: not in seedle.lock", cfg.id));
            continue;
        };
        let Some(entry) = lock.files.get(&id) else {
            problems.push(format!(
                "{id}: no file recorded in seedle.lock (run `seedle export`)"
            ));
            continue;
        };
        let Some(table) = schema.get(&id) else {
            continue;
        };
        let columns = match export::selected_columns(table, cfg) {
            Ok(c) => c,
            Err(e) => {
                problems.push(format!("{id}: {e}"));
                continue;
            }
        };

        if !format::is_loadable(cfg) {
            // A .sql file cannot be re-read, so only its bytes can be checked.
            match hash_recorded_path(&out_dir, &entry.path) {
                Ok(hash) if hash == entry.sha256 => checked += 1,
                Ok(_) => problems.push(format!("{id}: {} has changed since export", entry.path)),
                Err(e) => problems.push(format!("{id}: {e}")),
            }
            continue;
        }

        match format::read(&out_dir, &columns, cfg, &schema.default_schema) {
            Err(e) => problems.push(format!("{id}: {e:#}")),
            Ok(rows) => {
                if rows.len() as u64 != entry.rows {
                    problems.push(format!(
                        "{id}: {} rows on disk, {} recorded in seedle.lock",
                        rows.len(),
                        entry.rows
                    ));
                }
                if let Err(e) = load::validate_rows(table, &columns, &rows, cfg) {
                    problems.push(format!("{id}: {e}"));
                }
                checked += 1;
            }
        }
    }

    // Buckets verify entirely offline: every object is re-hashed from disk and
    // compared with the manifest, and the manifest with the lock.
    let mut bucket_objects = 0u64;
    for (name, entry) in &lock.buckets {
        let dir = out_dir.join("buckets").join(name);
        match BucketExport::read(&dir) {
            Err(e) => problems.push(format!("bucket {name}: {e:#}")),
            Ok(export) => {
                match export.hash() {
                    Ok(h) if h == entry.sha256 => {}
                    Ok(h) => problems.push(format!(
                        "bucket {name}: manifest hash {h} does not match seedle.lock ({})",
                        entry.sha256
                    )),
                    Err(e) => problems.push(format!("bucket {name}: {e:#}")),
                }
                for o in &export.objects {
                    let path = match BucketExport::object_path(&dir, &o.path) {
                        Ok(p) => p,
                        Err(e) => {
                            problems.push(format!("bucket {name}: {e}"));
                            continue;
                        }
                    };
                    match std::fs::read(&path) {
                        Err(e) => problems.push(format!("bucket {name}: {}: {e}", path.display())),
                        Ok(bytes) => {
                            let actual = crate::lock::file_hash(&bytes);
                            if actual != o.sha256 {
                                problems.push(format!(
                                    "bucket {name}: {} changed since export (sha256 {actual}, \
                                     manifest says {})",
                                    o.path, o.sha256
                                ));
                            }
                            bucket_objects += 1;
                        }
                    }
                }
            }
        }
    }

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "checked": checked,
                "bucket_objects": bucket_objects,
                "problems": problems,
                "ok": problems.is_empty(),
            }))?
        );
    }

    if !problems.is_empty() {
        bail!(
            "seed files do not match seedle.lock:\n{}",
            problems
                .iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    if !ctx.json {
        ctx.say(format!(
            "verified {checked} table{}{} against {}",
            if checked == 1 { "" } else { "s" },
            if lock.buckets.is_empty() {
                String::new()
            } else {
                format!(
                    " and {bucket_objects} bucket object{}",
                    if bucket_objects == 1 { "" } else { "s" }
                )
            },
            ctx.cfg.lock_path().display()
        ));
    }
    Ok(())
}

fn hash_recorded_path(out_dir: &Path, rel: &str) -> Result<String> {
    let path = out_dir.join(rel);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(crate::lock::file_hash(&bytes))
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

pub async fn cmd_init(cli: &Cli, url: Option<String>, force: bool) -> Result<()> {
    let path = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(crate::config::CONFIG_FILENAME));
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }

    let mut tables: Vec<TableId> = Vec::new();
    let mut engine = Engine::Postgres;
    let mut default_schema = String::from("public");

    if let Some(url) = &url {
        engine = if url.starts_with("mysql") || url.starts_with("mariadb") {
            Engine::Mysql
        } else {
            Engine::Postgres
        };
        let src = ResolvedSource {
            name: "init".into(),
            engine,
            url: url.clone(),
            read_only: true,
            origin: "--url".into(),
            storage: None,
        };
        let db = Db::connect(&src).await?;
        tables = match engine {
            Engine::Postgres => db::postgres::list_tables(&db).await?,
            Engine::Mysql => db::mysql::list_tables(&db).await?,
        };
        default_schema = db
            .query_text(match engine {
                Engine::Postgres => "SELECT current_schema()",
                Engine::Mysql => "SELECT DATABASE()",
            })
            .await
            .unwrap_or_default()
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_else(|| default_schema.clone());
    }

    let table_block = if tables.is_empty() {
        "  # Only tables listed here are ever touched.\n  # users:\n  #   where: \"created_at > now() - interval '30 days'\"\n  #   order_by: [id]\n".to_string()
    } else {
        tables
            .iter()
            // Qualifying with the default schema is noise; `users` reads better
            // than `public.users` and resolves to the same table.
            .map(|t| format!("  {}: {{}}\n", t.file_stem(&default_schema)))
            .collect::<String>()
    };

    let content = format!(
        "version: 1\n\n\
         sources:\n  \
           dev:\n    \
             engine: {}\n    \
             # Credentials never live in this file.\n    \
             env_file: .env\n    \
             url_var: DATABASE_URL\n    \
             default: true\n\n\
         export:\n  \
           out: seed\n  \
           format: jsonl\n  \
           # Embed json/jsonb columns as nested values rather than escaped strings.\n  \
           json: unroll\n\n\
         load:\n  \
           default: upsert\n\n\
         tables:\n{table_block}",
        engine.as_str()
    );

    crate::io::write_atomic(&path, content.as_bytes())?;
    println!("wrote {}", path.display());
    if tables.is_empty() {
        println!("Add your tables under `tables:`, then run `seedle lock`.");
    } else {
        println!(
            "Listed {} table{}. Trim the list, then run `seedle lock`.",
            tables.len(),
            if tables.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------

pub async fn dispatch(cli: Cli) -> Result<()> {
    // `init` runs before a config exists, so it does not build a Ctx.
    if let Command::Init { url, force } = &cli.command {
        return cmd_init(&cli, url.clone(), *force).await;
    }

    let ctx = Ctx::new(&cli)?;
    match cli.command {
        Command::Init { .. } => unreachable!("handled above"),
        Command::Sources { no_connect } => cmd_sources(&ctx, no_connect).await,
        Command::Lock { check } => cmd_lock(&ctx, check).await,
        Command::Diff => cmd_diff(&ctx).await,
        Command::Export {
            tables,
            buckets,
            no_buckets,
            format,
            out,
            force,
        } => cmd_export(&ctx, tables, buckets, no_buckets, format, out, force).await,
        Command::Plan { tables } => cmd_plan(&ctx, tables).await,
        Command::Load {
            tables,
            buckets,
            no_buckets,
            dry_run,
            no_transaction,
            yes,
            force,
        } => {
            cmd_load(
                &ctx,
                tables,
                buckets,
                no_buckets,
                dry_run,
                no_transaction,
                yes,
                force,
            )
            .await
        }
        Command::Verify => cmd_verify(&ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::drift::{Drift, Report, Severity};

    #[test]
    fn drift_json_reports_both_counts_and_every_item() {
        let report = Report {
            drifts: vec![
                Drift {
                    severity: Severity::Breaking,
                    table: TableId::new("public", "users"),
                    column: Some("age".into()),
                    what: "int32 -> int16".into(),
                    note: "narrowing".into(),
                },
                Drift {
                    severity: Severity::Benign,
                    table: TableId::new("public", "users"),
                    column: None,
                    what: "table added".into(),
                    note: String::new(),
                },
            ],
        };
        let parsed: serde_json::Value = serde_json::from_str(&drift_json(&report)).unwrap();
        assert_eq!(parsed["breaking"], 1);
        assert_eq!(parsed["benign"], 1);
        assert_eq!(parsed["drifts"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["drifts"][0]["severity"], "breaking");
        assert_eq!(parsed["drifts"][0]["column"], "age");
        assert_eq!(parsed["drifts"][1]["column"], serde_json::Value::Null);
    }
}
